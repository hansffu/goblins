//! Transitional daemon-side Docker bridge. Namespace/storage ownership lives
//! in the scope process; phase 4B moves this engine state there as well.
use crate::{
    Result,
    config::Launch,
    docker::{Engine, storage_root},
    process::{self, Cancellation},
    scope_client::{Broker, Client},
    session::Inheritance,
};
use std::{
    collections::BTreeMap,
    fs::{self, File},
    os::{fd::AsRawFd, unix::fs::MetadataExt},
    path::{Path, PathBuf},
    process::Command,
    sync::{Arc, Mutex, MutexGuard, OnceLock, Weak},
    time::{Duration, Instant},
};
pub(crate) type Grant = (File, PathBuf, bool);
type Granted = ((u64, u64), PathBuf, bool);
fn identity(file: &File) -> std::io::Result<(u64, u64)> {
    let meta = file.metadata()?;
    Ok((meta.dev(), meta.ino()))
}
pub(crate) struct Shared {
    pub name: String,
    definition: goblins_runtime::config::ScopeDefinition,
    directory: PathBuf,
    state: Mutex<State>,
}
struct State {
    engine: Option<Engine>,
    // An independent scope membership belongs to the engine, never its donor.
    membership: Option<Client>,
    users: usize,
    grants: Vec<Granted>,
    owners: Vec<Inheritance>,
    store: Option<PathBuf>,
}
impl Drop for State {
    fn drop(&mut self) {
        self.engine.take();
        self.membership.take();
    }
}
fn lock<'a, T>(mutex: &'a Mutex<T>, cancel: &Cancellation) -> Result<MutexGuard<'a, T>> {
    loop {
        cancel.check()?;
        match mutex.try_lock() {
            Ok(guard) => return Ok(guard),
            Err(std::sync::TryLockError::Poisoned(_)) => {
                return Err("scope Docker state poisoned".into());
            }
            Err(_) => std::thread::sleep(Duration::from_millis(20)),
        }
    }
}
type Slot = Arc<Mutex<Weak<Shared>>>;
static DOCKER: OnceLock<Mutex<BTreeMap<(PathBuf, String), Slot>>> = OnceLock::new();
impl Shared {
    pub fn prepare(launch: &Launch, member: &Client, cancel: &Cancellation) -> Result<Arc<Self>> {
        let realm = launch
            .protected_paths
            .last()
            .cloned()
            .ok_or("missing daemon realm")?;
        let slot = lock(DOCKER.get_or_init(Mutex::default), cancel)?
            .entry((realm, member.name.clone()))
            .or_insert_with(|| Arc::new(Mutex::new(Weak::new())))
            .clone();
        let mut slot = lock(&slot, cancel)?;
        if let Some(shared) = slot.upgrade() {
            return Ok(shared);
        }
        fs::create_dir_all(member.directory.join("docker-socket"))?;
        let shared = Arc::new(Self {
            name: member.name.clone(),
            definition: member.definition.clone(),
            directory: member.directory.clone(),
            state: Mutex::new(State {
                engine: None,
                membership: None,
                users: 0,
                grants: vec![],
                owners: vec![],
                store: None,
            }),
        });
        *slot = Arc::downgrade(&shared);
        Ok(shared)
    }
    pub fn alive(&self) -> bool {
        let Ok(mut state) = self.state.try_lock() else {
            return true;
        };
        state.engine.as_mut().is_none_or(Engine::alive)
            && state.membership.as_ref().is_none_or(Client::alive)
    }
    pub fn socket(&self) -> PathBuf {
        self.directory.join("docker-socket/docker.sock")
    }
    pub fn acquire(
        self: &Arc<Self>,
        launch: &Launch,
        filesystem: crate::docker::Filesystem,
        grants: Vec<Grant>,
        owner: Inheritance,
        store: PathBuf,
        broker: &Broker,
        cancel: &Cancellation,
    ) -> Result<Lease> {
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
            let membership = broker.request(launch, cancel)?;
            state.engine = Some(Engine::start(
                launch,
                crate::docker::Filesystem {
                    args: &args,
                    fds: filesystem.fds,
                },
                &storage_root(true)?,
                &self.directory,
                owner.resources(),
                cancel,
                &membership.namespaces,
                self.definition.persistent.then_some(self.name.as_str()),
            )?);
            state.membership = Some(membership);
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
            stop_deadline: None,
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
            self.membership
                .as_ref()
                .ok_or("scope Docker membership lost")?
                .namespaces
                .outer
                .as_raw_fd(),
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
    stop_deadline: Option<Instant>,
}
impl Lease {
    pub fn stop(mut self, deadline: Instant) {
        self.stop_deadline = Some(deadline);
    }
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
                if let Some(membership) = state.membership.take() {
                    let deadline = self.stop_deadline.unwrap_or_else(|| {
                        Instant::now() + goblins_runtime::scope::RELEASE_TIMEOUT
                    });
                    if let Err(error) = membership.leave(deadline) {
                        eprintln!("scope Docker bridge shutdown: {error}");
                    }
                }
                state.owners.clear();
                state.grants.clear();
                state.store = None;
                let _ = fs::remove_file(self.shared.socket());
            }
        }
    }
}
