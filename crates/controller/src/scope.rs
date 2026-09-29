//! One live instance per named scope: namespaces, network, managed storage
//! and an optional Docker engine. Members hold strong references; the
//! registry only a weak one, so the last member leaving stops the scope.
use crate::{
    Result,
    config::{Launch, ScopeDefinition},
    docker::{Engine, storage_root},
    network::Network,
    process::{self, Cancellation},
    scope_keeper::Namespaces,
    scope_storage::Storage,
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

pub(crate) type Grant = (File, PathBuf, bool);
/// A grant already given to the engine, by identity. Holding descriptors for
/// every member's store closure would exhaust the controller's descriptor
/// limit; the engine's own mounts keep the granted objects alive.
type Granted = ((u64, u64), PathBuf, bool);
fn identity(file: &File) -> std::io::Result<(u64, u64)> {
    let meta = file.metadata()?;
    Ok((meta.dev(), meta.ino()))
}
pub(crate) struct Shared {
    pub name: String,
    pub definition: ScopeDefinition,
    pub directory: PathBuf,
    pub user: File,
    pub net: File,
    pub pid: Option<File>,
    online: bool,
    state: Mutex<State>,
    _keeper: Keeper,
    // Dropped last: temporary data outlives every member process and engine.
    storage: Storage,
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
    grants: Vec<Granted>,
    owners: Vec<Inheritance>,
    store: Option<PathBuf>,
    network: Option<Network>,
    namespaces: Namespaces,
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
                return Err("scope state poisoned".into());
            }
            Err(_) => std::thread::sleep(Duration::from_millis(20)),
        }
    }
}
type Slot = Arc<Mutex<Weak<Shared>>>;
static NAMED: OnceLock<Mutex<BTreeMap<(PathBuf, String), Slot>>> = OnceLock::new();
impl Shared {
    /// Join the live instance of the launch's scope, or start it.
    pub fn prepare(
        launch: &Launch,
        owner: Inheritance,
        directory: &Path,
        cancel: &Cancellation,
    ) -> Result<Arc<Self>> {
        let selected = launch.scope.as_ref().ok_or("launch has no scope")?;
        let realm = launch
            .protected_paths
            .last()
            .cloned()
            .unwrap_or_else(|| directory.to_path_buf());
        let slot = lock(NAMED.get_or_init(Mutex::default), cancel)?
            .entry((realm, selected.name.clone()))
            .or_insert_with(|| Arc::new(Mutex::new(Weak::new())))
            .clone();
        // Serializes startup and final teardown of this scope within the
        // controller: a new member waits instead of joining a stopping scope.
        let mut slot_guard = lock(&slot, cancel)?;
        if let Some(shared) = slot_guard.upgrade() {
            shared.compatible(launch)?;
            return Ok(shared);
        }
        // A previous instance may still be stopping in another thread.
        let deadline = std::time::Instant::now() + Duration::from_secs(60);
        let storage = loop {
            if let Some(storage) = Storage::open(&selected.name, &selected.definition)? {
                break storage;
            }
            if std::time::Instant::now() >= deadline {
                return Err(format!("scope '{}' did not finish stopping", selected.name).into());
            }
            cancel.check()?;
            std::thread::sleep(Duration::from_millis(50));
        };
        static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
        let path = directory.join(format!(
            "scope-runtime-{}",
            NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        ));
        unix::private_directory(&path)?;
        fs::create_dir(path.join("docker-socket"))?;
        // Linux PDEATHSIG follows the creating THREAD. A session worker may
        // exit while other members still use this scope; keep the creator alive
        // independently until the shared namespaces and pasta have been stopped.
        let (ready_tx, ready_rx) = std::sync::mpsc::sync_channel(1);
        let (stop_tx, stop_rx) = std::sync::mpsc::channel();
        let setup_launch = launch.clone();
        let setup_path = path.clone();
        let setup_cancel = cancel.clone();
        let pid = selected.definition.pid;
        let thread = std::thread::spawn(move || {
            let setup = || -> Result<(Namespaces, Option<Network>)> {
                let namespaces = Namespaces::start(&setup_launch, &setup_path, pid, &setup_cancel)?;
                let network = setup_launch
                    .pasta
                    .as_ref()
                    .map(|pasta| {
                        Network::attach(
                            pasta,
                            namespaces.user.try_clone()?,
                            namespaces.net.try_clone()?,
                            &namespaces.net_path,
                            &setup_path,
                            &setup_cancel,
                        )
                    })
                    .transpose()?;
                Ok((namespaces, network))
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
        let (namespaces, network) = ready_rx.recv().map_err(|_| "scope owner exited")??;
        let shared = Arc::new(Self {
            name: selected.name.clone(),
            definition: selected.definition.clone(),
            directory: path,
            user: namespaces.user.try_clone()?,
            net: namespaces.net.try_clone()?,
            pid: namespaces.pid.as_ref().map(File::try_clone).transpose()?,
            online: launch.pasta.is_some(),
            state: Mutex::new(State {
                engine: None,
                users: 0,
                grants: vec![],
                owners: vec![],
                store: None,
                network,
                namespaces,
                _owner: owner,
            }),
            _keeper: keeper,
            storage,
        });
        *slot_guard = Arc::downgrade(&shared);
        Ok(shared)
    }
    pub fn compatible(&self, launch: &Launch) -> Result<()> {
        if self.online != launch.pasta.is_some() {
            return Err(format!(
                "scope '{}' has a different network policy (online/offline)",
                self.name
            )
            .into());
        }
        if launch
            .scope
            .as_ref()
            .is_none_or(|s| s.name != self.name || s.definition != self.definition)
        {
            return Err(format!(
                "scope '{}' is running with a different definition; stop its members first",
                self.name
            )
            .into());
        }
        Ok(())
    }
    pub fn alive(&self) -> bool {
        let Ok(mut state) = self.state.try_lock() else {
            return true;
        };
        state.namespaces.alive() && state.engine.as_mut().is_none_or(Engine::alive)
    }
    /// User, network and (when shared) PID namespace descriptors for members.
    pub fn fds(&self) -> (RawFd, RawFd, Option<RawFd>) {
        (
            self.user.as_raw_fd(),
            self.net.as_raw_fd(),
            self.pid.as_ref().map(AsRawFd::as_raw_fd),
        )
    }
    pub fn storage(&self, name: &str) -> Result<&File> {
        self.storage.directory(name)
    }
    pub fn socket(&self) -> PathBuf {
        self.directory.join("docker-socket/docker.sock")
    }
    /// Kill a departed member's processes that remain in the shared PID
    /// namespace. Payload code cannot leave its mount namespace (seccomp
    /// denies unshare, setns and mount), so it identifies the member.
    pub fn kill_member(&self, mountns: u64) {
        let Some(pidns) = self
            .pid
            .as_ref()
            .and_then(|f| f.metadata().ok())
            .map(|m| m.ino())
        else {
            return;
        };
        for _ in 0..100 {
            let mut found = false;
            for entry in fs::read_dir("/proc").into_iter().flatten().flatten() {
                let Some(id) = entry
                    .file_name()
                    .to_str()
                    .and_then(|n| n.parse::<i32>().ok())
                else {
                    continue;
                };
                // Pin the process before checking it, so a reused PID is
                // never signalled on the strength of another process's check.
                let Ok(pidfd) =
                    unix::owned(unsafe { libc::syscall(libc::SYS_pidfd_open, id, 0) as i32 })
                else {
                    continue;
                };
                let member = |kind: &str, inode: u64| {
                    fs::metadata(format!("/proc/{id}/ns/{kind}")).is_ok_and(|m| m.ino() == inode)
                };
                if member("pid", pidns) && member("mnt", mountns) {
                    found = true;
                    unsafe {
                        libc::syscall(
                            libc::SYS_pidfd_send_signal,
                            pidfd.as_raw_fd(),
                            libc::SIGKILL,
                            std::ptr::null::<libc::siginfo_t>(),
                            0,
                        );
                    }
                }
            }
            if !found {
                return;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        eprintln!("scope '{}': member processes survived cleanup", self.name);
    }
    pub fn acquire(
        self: &Arc<Self>,
        launch: &Launch,
        filesystem: crate::docker::Filesystem,
        grants: Vec<Grant>,
        owner: Inheritance,
        store: PathBuf,
        cancel: &Cancellation,
    ) -> Result<Lease> {
        self.compatible(launch)?;
        if !self.definition.docker {
            return Err(format!("scope '{}' does not enable Docker", self.name).into());
        }
        let mut state = lock(&self.state, cancel)?;
        // Reject ambiguous aliases and access-mode conflicts before changing a
        // live engine. Trust groups share grants, not pathname interpretation.
        for (source, dest, ro) in &grants {
            state.check(identity(source)?, dest, *ro)?;
        }
        if state.engine.is_none() {
            // Only the explicitly granted paths and runtime closure enter the
            // engine. Session control sockets and package profiles do not.
            let mut args = vec![];
            let mut i = 0;
            while i < filesystem.args.len() {
                let count = match filesystem.args[i].as_str() {
                    "--setenv" | "--symlink" | "--bind-fd" | "--ro-bind-fd" | "--bind"
                    | "--ro-bind" => 3,
                    _ => 2,
                };
                if i + count > filesystem.args.len() {
                    return Err("invalid engine filesystem plan".into());
                }
                let private = matches!(filesystem.args[i].as_str(), "--bind" | "--ro-bind")
                    && matches!(
                        filesystem.args[i + 2].as_str(),
                        "/run/goblins/request.sock" | "/run/goblins/packages" | "/run/goblins/bin"
                    );
                if !private {
                    args.extend_from_slice(&filesystem.args[i..i + count]);
                }
                i += count;
            }
            state.engine = Some(Engine::start(
                launch,
                crate::docker::Filesystem {
                    args: &args,
                    fds: filesystem.fds,
                },
                &storage_root(true)?,
                &self.directory,
                cancel,
                &state.namespaces,
                self.definition.persistent.then_some(self.name.as_str()),
            )?);
            state.store = Some(store);
        } else {
            state.owners.push(owner.clone());
            for (source, dest, ro) in &grants {
                state.bind(launch, &self.directory, source, dest, *ro, cancel)?;
            }
        }
        for (source, dest, ro) in grants {
            if !state.grants.iter().any(|(_, old, _)| *old == dest) {
                state.grants.push((identity(&source)?, dest, ro));
            }
        }
        state.owners.push(owner);
        state.users += 1;
        Ok(Lease {
            shared: self.clone(),
        })
    }
}
impl State {
    fn check(&self, source: (u64, u64), dest: &Path, ro: bool) -> Result<()> {
        if self
            .grants
            .iter()
            .any(|(old, old_dest, old_ro)| old_dest == dest && (*old, *old_ro) == (source, ro))
        {
            return Ok(());
        }
        for (old_source, old_dest, old_ro) in &self.grants {
            if dest == old_dest {
                if (source, ro) != (*old_source, *old_ro) {
                    return Err(format!("scope Docker mount conflict at {}: one engine cannot give members different sources or access modes at the same path", dest.display()).into());
                }
            } else if dest.starts_with(old_dest) || old_dest.starts_with(dest) {
                return Err(format!(
                    "scope Docker engine has overlapping grants at {} and {}",
                    dest.display(),
                    old_dest.display()
                )
                .into());
            }
        }
        Ok(())
    }
    /// Mount `source` at `dest` in the running engine, unless it is there.
    fn bind(
        &mut self,
        launch: &Launch,
        directory: &Path,
        source: &File,
        dest: &Path,
        ro: bool,
        cancel: &Cancellation,
    ) -> Result<()> {
        if self.grants.iter().any(|(_, old, _)| old == dest) {
            return Ok(());
        }
        if dest.starts_with("/nix/store") {
            let target = self
                .store
                .as_ref()
                .ok_or("scope Docker engine has no store")?
                .join(dest.file_name().ok_or("invalid store grant")?);
            if source.metadata()?.is_dir() {
                fs::create_dir_all(&target)?;
            } else if !target.exists() {
                File::create(&target)?;
            }
        }
        let engine = self.engine.as_ref().ok_or("scope Docker engine stopped")?;
        let keep = [
            self.namespaces.outer.as_raw_fd(),
            engine.mounts.as_raw_fd(),
            engine.root.as_raw_fd(),
            source.as_raw_fd(),
        ];
        let mut command = Command::new(&launch.helper);
        command
            .arg("--docker-bind")
            .args(keep.map(|fd| fd.to_string()))
            .arg(dest)
            .arg(if ro { "ro" } else { "rw" });
        process::command_with_fds(&mut command, directory, cancel, &keep)?;
        self.grants
            .push((identity(source)?, dest.to_path_buf(), ro));
        Ok(())
    }
}
pub(crate) struct Lease {
    pub shared: Arc<Shared>,
}
impl Lease {
    /// Give the engine store paths mounted into a member after it attached:
    /// package grants, dev shell refreshes and flake apps. Each path is
    /// opened only while it is bound, so the controller holds no descriptor
    /// per path.
    pub fn extend(
        &self,
        launch: &Launch,
        paths: &std::collections::BTreeSet<PathBuf>,
        cancel: &Cancellation,
    ) -> Result<()> {
        let mut state = lock(&self.shared.state, cancel)?;
        for path in paths {
            cancel.check()?;
            let source = File::open(path)?;
            state.check(identity(&source)?, path, true)?;
            state.bind(launch, &self.shared.directory, &source, path, true, cancel)?;
        }
        Ok(())
    }
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
