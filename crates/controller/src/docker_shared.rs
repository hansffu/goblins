//! Weak registry plus explicit user leases: history never keeps a daemon alive.
use crate::{
    Result,
    config::Launch,
    docker::{Engine, storage_root},
    docker_scope::Scope,
    network::Network,
    process::{self, Cancellation},
    session::Inheritance,
    unix,
};
use std::{
    collections::BTreeMap,
    fs::{self, File},
    os::{
        fd::{AsRawFd, RawFd},
        unix::fs::MetadataExt,
    },
    path::{Path, PathBuf},
    process::Command,
    sync::{Arc, Mutex, MutexGuard, OnceLock, Weak},
    time::Duration,
};

#[derive(Clone, Default)]
pub(crate) struct Selection {
    pub scope: Weak<Shared>,
    pub name: Option<String>,
    pub enabled: bool,
}
pub(crate) type Grant = (File, PathBuf, bool);
pub(crate) struct Shared {
    pub name: Option<String>,
    pub directory: PathBuf,
    pub user: File,
    pub net: File,
    online: bool,
    state: Mutex<State>,
    _keeper: Keeper,
}
struct Keeper {
    stop: Option<std::sync::mpsc::Sender<()>>,
    thread: Option<std::thread::JoinHandle<()>>,
}
impl Drop for Keeper {
    fn drop(&mut self) {
        self.stop.take();
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}
struct State {
    // Explicit drop ordering is also used when the last Docker user leaves.
    engine: Option<Engine>,
    users: usize,
    grants: Vec<Grant>,
    owners: Vec<Inheritance>,
    store: Option<PathBuf>,
    network: Option<Network>,
    scope: Scope,
    _owner: Inheritance,
}
impl Drop for State {
    fn drop(&mut self) {
        self.engine.take();
        self.network.take();
    }
}

fn lock<'a, T>(mutex: &'a Mutex<T>, cancel: &Cancellation) -> Result<MutexGuard<'a, T>> {
    loop {
        cancel.check()?;
        match mutex.try_lock() {
            Ok(guard) => return Ok(guard),
            Err(std::sync::TryLockError::Poisoned(_)) => {
                return Err("Docker scope state poisoned".into());
            }
            Err(_) => std::thread::sleep(Duration::from_millis(20)),
        }
    }
}
type Slot = Arc<Mutex<Weak<Shared>>>;
static NAMED: OnceLock<Mutex<BTreeMap<(PathBuf, String), Slot>>> = OnceLock::new();
impl Shared {
    pub fn prepare(
        launch: &Launch,
        owner: Inheritance,
        directory: &Path,
        name: Option<String>,
        cancel: &Cancellation,
    ) -> Result<Arc<Self>> {
        let slot = if let Some(name) = &name {
            let realm = launch
                .protected_paths
                .last()
                .cloned()
                .unwrap_or_else(|| directory.to_path_buf());
            let mut registry = lock(NAMED.get_or_init(Mutex::default), cancel)?;
            Some(
                registry
                    .entry((realm, name.clone()))
                    .or_insert_with(|| Arc::new(Mutex::new(Weak::new())))
                    .clone(),
            )
        } else {
            None
        };
        let mut slot_guard = slot.as_ref().map(|slot| lock(slot, cancel)).transpose()?;
        if let Some(shared) = slot_guard.as_ref().and_then(|s| s.upgrade()) {
            shared.compatible(launch)?;
            return Ok(shared);
        }
        static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
        let path = directory.join(format!(
            "docker-runtime-{}",
            NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        ));
        unix::private_directory(&path)?;
        fs::create_dir(path.join("docker-socket"))?;
        // Linux PDEATHSIG follows the creating THREAD. A session worker may
        // exit while other members still use this scope; keep the creator alive
        // independently until the shared namespace and pasta have been stopped.
        let (ready_tx, ready_rx) = std::sync::mpsc::sync_channel(1);
        let (stop_tx, stop_rx) = std::sync::mpsc::channel();
        let setup_launch = launch.clone();
        let setup_path = path.clone();
        let setup_cancel = cancel.clone();
        let thread = std::thread::spawn(move || {
            let setup = || -> Result<(Scope, Option<Network>)> {
                let scope = Scope::start(&setup_launch, &setup_path, &setup_cancel)?;
                let network = setup_launch
                    .pasta
                    .as_ref()
                    .map(|pasta| {
                        Network::attach(
                            pasta,
                            scope.user.try_clone()?,
                            scope.net.try_clone()?,
                            &scope.net_path,
                            &setup_path,
                            &setup_cancel,
                        )
                    })
                    .transpose()?;
                Ok((scope, network))
            };
            let result = setup();
            let success = result.is_ok();
            if ready_tx.send(result).is_ok() && success {
                let _ = stop_rx.recv();
            }
        });
        let keeper = Keeper {
            stop: Some(stop_tx),
            thread: Some(thread),
        };
        let (scope, network) = ready_rx.recv().map_err(|_| "Docker scope owner exited")??;
        let shared = Arc::new(Self {
            name,
            directory: path,
            user: scope.user.try_clone()?,
            net: scope.net.try_clone()?,
            online: launch.pasta.is_some(),
            state: Mutex::new(State {
                engine: None,
                users: 0,
                grants: vec![],
                owners: vec![],
                store: None,
                network,
                scope,
                _owner: owner,
            }),
            _keeper: keeper,
        });
        if let Some(guard) = slot_guard.as_mut() {
            **guard = Arc::downgrade(&shared);
        }
        Ok(shared)
    }
    pub fn compatible(&self, launch: &Launch) -> Result<()> {
        if self.online != launch.pasta.is_some() {
            return Err("Docker scope has a different network policy (online/offline)".into());
        }
        Ok(())
    }
    pub fn alive(&self) -> bool {
        let Ok(mut state) = self.state.try_lock() else {
            return true;
        };
        state.scope.alive() && state.engine.as_mut().is_none_or(Engine::alive)
    }
    pub fn fds(&self) -> [RawFd; 2] {
        [self.user.as_raw_fd(), self.net.as_raw_fd()]
    }
    pub fn socket(&self) -> PathBuf {
        self.directory.join("docker-socket/docker.sock")
    }
    pub fn acquire(
        self: &Arc<Self>,
        launch: &Launch,
        filesystem: &[String],
        sources: &[RawFd],
        grants: Vec<Grant>,
        owner: Inheritance,
        store: PathBuf,
        cancel: &Cancellation,
    ) -> Result<Lease> {
        self.compatible(launch)?;
        let mut state = lock(&self.state, cancel)?;
        // Reject ambiguous aliases and access-mode conflicts before changing a
        // live engine. Trust groups share grants, not pathname interpretation.
        for (source, dest, ro) in &grants {
            if let Some((old_source, _, old_ro)) =
                state.grants.iter().find(|(_, old, _)| old == dest)
            {
                let a = source.metadata()?;
                let b = old_source.metadata()?;
                if (a.dev(), a.ino(), ro) == (b.dev(), b.ino(), old_ro) {
                    continue;
                }
            }
            for (old_source, old_dest, old_ro) in &state.grants {
                if dest == old_dest {
                    let a = source.metadata()?;
                    let b = old_source.metadata()?;
                    if (a.dev(), a.ino(), ro) != (b.dev(), b.ino(), old_ro) {
                        return Err(format!("Docker scope mount conflict at {}: one engine cannot give members different sources or access modes at the same path", dest.display()).into());
                    }
                } else if dest.starts_with(old_dest) || old_dest.starts_with(dest) {
                    return Err(format!(
                        "Docker scope has overlapping grants at {} and {}",
                        dest.display(),
                        old_dest.display()
                    )
                    .into());
                }
            }
        }
        if state.engine.is_none() {
            // Only the explicitly granted paths and runtime closure enter the
            // engine. Session control sockets and package profiles do not.
            let mut args = vec![];
            let mut i = 0;
            while i < filesystem.len() {
                let count = match filesystem[i].as_str() {
                    "--setenv" | "--symlink" | "--bind-fd" | "--ro-bind-fd" | "--bind"
                    | "--ro-bind" => 3,
                    _ => 2,
                };
                if i + count > filesystem.len() {
                    return Err("invalid engine filesystem plan".into());
                }
                let private = matches!(filesystem[i].as_str(), "--bind" | "--ro-bind")
                    && matches!(
                        filesystem[i + 2].as_str(),
                        "/run/goblins/request.sock" | "/run/goblins/packages" | "/run/goblins/bin"
                    );
                if !private {
                    args.extend_from_slice(&filesystem[i..i + count]);
                }
                i += count;
            }
            state.engine = Some(Engine::start(
                launch,
                &args,
                sources,
                &storage_root(true)?,
                &self.directory,
                cancel,
                &state.scope,
                self.name.as_deref(),
            )?);
            state.store = Some(store);
        } else {
            state.owners.push(owner.clone());
            for (source, dest, ro) in &grants {
                if state.grants.iter().any(|(_, old, _)| old == dest) {
                    continue;
                }
                if dest.starts_with("/nix/store") {
                    let target = state
                        .store
                        .as_ref()
                        .unwrap()
                        .join(dest.file_name().ok_or("invalid store grant")?);
                    if source.metadata()?.is_dir() {
                        fs::create_dir_all(&target)?;
                    } else if !target.exists() {
                        File::create(&target)?;
                    }
                }
                let engine = state.engine.as_ref().unwrap();
                let keep = [
                    state.scope.outer.as_raw_fd(),
                    engine.mounts.as_raw_fd(),
                    engine.root.as_raw_fd(),
                    source.as_raw_fd(),
                ];
                let mut command = Command::new(&launch.helper);
                command
                    .arg("--docker-bind")
                    .args(keep.map(|fd| fd.to_string()))
                    .arg(dest)
                    .arg(if *ro { "ro" } else { "rw" });
                process::command_with_fds(&mut command, &self.directory, cancel, &keep)?;
                state.grants.push((source.try_clone()?, dest.clone(), *ro));
            }
        }
        for grant in grants {
            if !state.grants.iter().any(|(_, dest, _)| *dest == grant.1) {
                state.grants.push(grant);
            }
        }
        state.owners.push(owner);
        state.users += 1;
        Ok(Lease {
            shared: self.clone(),
        })
    }
}
pub(crate) struct Lease {
    pub shared: Arc<Shared>,
}
impl Drop for Lease {
    fn drop(&mut self) {
        if let Ok(mut state) = self.shared.state.lock() {
            state.users -= 1;
            if state.users == 0 {
                state.engine.take();
                state.owners.clear();
                state.grants.clear();
                state.store = None;
                let _ = fs::remove_file(self.shared.socket());
            }
        }
    }
}
