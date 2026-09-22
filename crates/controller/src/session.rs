//! The host worker owns one session, including Nix, roots and private helper
//! pipes. Namespace operations themselves always run in the separate helper.
pub use crate::process::Cancellation as Cancel;
use crate::{
    Result,
    config::Launch,
    process::{self, Cancellation},
    seccomp, snapshot, unix,
};
use goblins_protocol::package_name;
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs::{self, File, OpenOptions},
    io::Write,
    os::{
        fd::{AsRawFd, OwnedFd},
        unix::{
            fs::{PermissionsExt, symlink},
            net::UnixListener,
            process::CommandExt,
        },
    },
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    sync::{Arc, Mutex},
    time::Duration,
};

pub fn store_path(path: &Path) -> Result<PathBuf> {
    let name = path
        .file_name()
        .and_then(|p| p.to_str())
        .ok_or("invalid store path")?;
    if path.as_os_str() != std::ffi::OsStr::new(&format!("/nix/store/{name}"))
        || path.parent() != Some(Path::new("/nix/store"))
        || !(34..=211).contains(&name.len())
        || name.as_bytes()[32] != b'-'
        || !name.bytes().enumerate().all(|(i, b)| {
            if i < 32 {
                b"0123456789abcdfghijklmnpqrsvwxyz".contains(&b)
            } else {
                b.is_ascii_alphanumeric() || b"+-._?=".contains(&b)
            }
        })
    {
        return Err("not a canonical store output".into());
    }
    let meta = fs::symlink_metadata(path)?;
    if !meta.is_file() && !meta.is_dir() {
        return Err("store output must be a regular file or directory".into());
    }
    Ok(path.to_path_buf())
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Identity {
    pub pid: u32,
    pub helper_userns: u64,
    pub mount_owner: u64,
    pub source_mountns: u64,
    pub mountns: u64,
}
struct SessionDirectory(PathBuf);
impl Drop for SessionDirectory {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
        if self.0.file_name().is_some_and(|n| n == "resources") {
            if let Some(parent) = self.0.parent() {
                let _ = fs::remove_dir(parent);
            }
        }
    }
}
struct SharedWorkspace {
    file: File,
    // Keep snapshot contents alive until every descendant has stopped.
    _owner: Arc<SessionDirectory>,
}
/// Host-owned launch capability. Children reuse opened mounts, never re-resolve
/// host paths or read a caller-selected manifest.
#[derive(Clone)]
pub(crate) struct Inheritance {
    launch: Arc<Launch>,
    binds: Arc<crate::binds::Plan>,
    workspace: Arc<SharedWorkspace>,
    selection: Arc<Mutex<crate::docker::Selection>>,
    _owner: Arc<SessionDirectory>,
}
impl Inheritance {
    pub(crate) fn docker_allowed(&self, name: &str) -> bool {
        self.launch.docker.as_ref().is_some_and(|d| {
            d.default_scope.as_deref() == Some(name) || d.allowed_scopes.iter().any(|s| s == name)
        })
    }
    pub(crate) fn docker_selection(&self) -> crate::docker::Selection {
        self.selection.lock().unwrap().clone()
    }
    pub(crate) fn description(&self) -> &str {
        &self.launch.description
    }
    pub(crate) fn integration(&self) -> Option<&str> {
        self.launch.integration.as_deref()
    }
    pub(crate) fn initially_available(&self, path: &Path) -> bool {
        self.launch.initial_packages.iter().any(|p| p == path)
    }
}
pub struct Session {
    pub directory: PathBuf,
    pub launch: Launch,
    pub mounted: BTreeSet<PathBuf>,
    pub packages: BTreeMap<String, PathBuf>,
    pub identity: Option<Identity>,
    pub input: Option<File>,
    pub output: Option<File>,
    pub listener: Option<UnixListener>,
    helper: Option<Child>,
    network: Option<crate::network::Network>,
    docker: Option<crate::docker::Lease>,
    docker_home: Option<Arc<crate::docker::Shared>>,
    docker_selection: Arc<Mutex<crate::docker::Selection>>,
    docker_inherit_enabled: Option<bool>,
    docker_forward: Option<crate::docker_forward::Forward>,
    docker_filesystem: Vec<String>,
    docker_error: Option<String>,
    pub exit_code: Option<i32>,
    helper_input: Option<File>,
    helper_output: Option<File>,
    cancel: Cancellation,
    directory_owner: Arc<SessionDirectory>,
    binds: Option<Arc<crate::binds::Plan>>,
    workspace: Option<Arc<SharedWorkspace>>,
}
impl Session {
    pub fn new(launch: Launch, workspace: Option<&Path>, cancel: Cancel) -> Result<Self> {
        Self::new_in(launch, workspace, cancel, unix::temp_directory()?)
    }
    pub fn new_in(
        mut launch: Launch,
        workspace: Option<&Path>,
        cancel: Cancel,
        directory: PathBuf,
    ) -> Result<Self> {
        // An explicit daemon workspace retains the existing snapshot behavior.
        launch.cwd = if workspace.is_some() {
            None
        } else {
            launch.cwd.map(fs::canonicalize).transpose()?
        };
        unix::private_directory(&directory)?;
        let selection = crate::docker::Selection {
            name: launch
                .docker
                .as_ref()
                .and_then(|docker| docker.default_scope.clone()),
            ..Default::default()
        };
        let session = Self {
            directory_owner: Arc::new(SessionDirectory(directory.clone())),
            directory,
            launch,
            mounted: BTreeSet::new(),
            packages: BTreeMap::new(),
            identity: None,
            input: None,
            output: None,
            listener: None,
            helper: None,
            network: None,
            docker: None,
            docker_home: None,
            docker_selection: Arc::new(Mutex::new(selection)),
            docker_inherit_enabled: None,
            docker_forward: None,
            docker_filesystem: Vec::new(),
            docker_error: None,
            exit_code: None,
            helper_input: None,
            helper_output: None,
            cancel,
            binds: None,
            workspace: None,
        };
        for name in ["store", "packages", "roots", "workspace"] {
            fs::create_dir(session.directory.join(name))?;
        }
        if let Some(source) = workspace {
            let resolved = fs::canonicalize(source)?;
            let storage = crate::docker::storage_root(false)?;
            if resolved.starts_with(&storage) || storage.starts_with(&resolved) {
                return Err("workspace snapshot overlaps protected Docker storage".into());
            }
            snapshot::snapshot(
                source,
                &session.directory.join("workspace"),
                &session.cancel,
            )?;
        }
        Ok(session)
    }
    pub(crate) fn child(
        inherited: Inheritance,
        cancel: Cancel,
        directory: PathBuf,
    ) -> Result<Self> {
        let mut launch = (*inherited.launch).clone();
        launch.cwd = None;
        let mut session = Self::new_in(launch, None, cancel, directory)?;
        // new_in canonicalizes cwd for root launches only. Preserve the pinned
        // parent's namespace path even when its host pathname was renamed.
        session.launch = (*inherited.launch).clone();
        let selection = inherited.docker_selection();
        session.docker_home = selection.scope.upgrade();
        session.docker_inherit_enabled = Some(selection.enabled);
        if selection.enabled
            && let Some(client) = session
                .launch
                .docker
                .as_ref()
                .and_then(|d| d.client.clone())
        {
            if !session.launch.initial_packages.contains(&client) {
                session.launch.initial_packages.push(client);
            }
        }
        *session.docker_selection.lock().unwrap() = selection;
        session.binds = Some(inherited.binds);
        session.workspace = Some(inherited.workspace);
        Ok(session)
    }
    pub(crate) fn inheritance(&self) -> Inheritance {
        Inheritance {
            launch: Arc::new(self.launch.clone()),
            binds: self.binds.as_ref().unwrap().clone(),
            workspace: self.workspace.as_ref().unwrap().clone(),
            selection: self.docker_selection.clone(),
            _owner: self.directory_owner.clone(),
        }
    }
    pub fn event(&self, event: &str, fields: serde_json::Value) {
        // Logging must never interrupt teardown or turn a completed grant into
        // an unreported mount failure. Directory is host-only, mode 0700.
        if let Ok(mut file) = OpenOptions::new()
            .create(true)
            .append(true)
            .open(self.directory.join("events.jsonl"))
        {
            if file.metadata().is_ok_and(|m| m.len() >= 1024 * 1024) {
                return;
            }
            let time = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs_f64();
            let _ = writeln!(
                file,
                "{}",
                serde_json::json!({"time":time,"event":event,"fields":fields})
            );
        }
    }
    fn command(&self, command: &mut Command) -> Result<String> {
        process::command(command, &self.directory, &self.cancel)
    }
    pub fn root(&self, path: &Path) -> Result<PathBuf> {
        let path = store_path(path)?;
        let link = self.directory.join("roots").join(path.file_name().unwrap());
        if !link.is_symlink() {
            self.command(
                Command::new("nix-store")
                    .arg("--realise")
                    .arg(&path)
                    .arg("--add-root")
                    .arg(link)
                    .arg("--indirect"),
            )?;
        }
        Ok(path)
    }
    pub fn closure(&self, paths: &[PathBuf]) -> Result<BTreeSet<PathBuf>> {
        let roots = paths
            .iter()
            .map(|p| self.root(p))
            .collect::<Result<Vec<_>>>()?;
        let output = self.command(
            Command::new("nix-store")
                .args(["--query", "--requisites"])
                .args(roots),
        )?;
        output.lines().map(|p| store_path(Path::new(p))).collect()
    }
    fn placeholders(&self, paths: &BTreeSet<PathBuf>) -> Result<()> {
        for path in paths {
            self.cancel.check()?;
            let dest = self.directory.join("store").join(path.file_name().unwrap());
            if path.is_dir() {
                fs::create_dir_all(dest)?;
            } else {
                OpenOptions::new()
                    .write(true)
                    .create(true)
                    .truncate(false)
                    .open(dest)?;
            }
        }
        Ok(())
    }
    pub fn start(&mut self, terminal: Option<OwnedFd>) -> Result<()> {
        let etc = crate::sandbox_etc::mounts(&self.launch.sandbox_etc)?;
        let mut private = self.launch.protected_paths.clone();
        let docker_storage = crate::docker::storage_root(false)?;
        private.push(docker_storage.clone());
        private.push(self.directory.clone());
        private.extend(etc.iter().map(|(_, destination)| destination.clone()));
        if self.binds.is_none() {
            let mut binds = self.launch.binds.plan(&private, &self.cancel)?;
            if let Some(cwd) = &self.launch.cwd {
                binds.add_working_directory(cwd, &private)?;
            }
            self.binds = Some(Arc::new(binds));
        }
        let binds = self.binds.as_ref().unwrap().clone();
        if self.workspace.is_none() {
            self.workspace = Some(Arc::new(SharedWorkspace {
                file: File::open(self.directory.join("workspace"))?,
                _owner: self.directory_owner.clone(),
            }));
        }
        let workspace_fd = self.workspace.as_ref().unwrap().file.as_raw_fd();
        // Root selected store-backed config without exposing the full output or
        // its closure. Only the precise declared/linked targets are mounted.
        for root in binds.store_roots()? {
            self.root(&root)?;
        }
        let package = |p: &Path| {
            p.parent()
                .and_then(Path::parent)
                .map(Path::to_path_buf)
                .ok_or("invalid executable path")
        };
        self.root(&package(&self.launch.helper)?)?;
        if let Some(docker) = &self.launch.docker {
            // Preserve late-activation inputs across host configuration rebuilds
            // and GC, without mounting the engine closure into the shell.
            self.root(&package(&docker.daemon)?)?;
            if let Some(client) = &docker.client {
                self.root(client)?;
            }
        }
        if let Some(pasta) = &self.launch.pasta {
            // Keep the host network process available without exposing it as
            // an initially granted sandbox package.
            self.root(&package(pasta)?)?;
        }
        let mut roots = vec![
            package(&self.launch.shell)?,
            package(&self.launch.posix_shell)?,
            self.launch.client_package.clone(),
        ];
        roots.extend(self.launch.initial_packages.clone());
        roots.extend(self.launch.initial_closure.clone());
        roots.extend(etc.iter().map(|(source, _)| source.clone()));
        let initial = self.closure(&roots)?;
        self.placeholders(&initial)?;
        let listener = UnixListener::bind(self.directory.join("request.sock"))?;
        fs::set_permissions(
            self.directory.join("request.sock"),
            fs::Permissions::from_mode(0o600),
        )?;
        listener.set_nonblocking(true)?;
        self.listener = Some(listener);
        if self.launch.docker.is_some() {
            fs::create_dir(self.directory.join("docker-socket"))?;
            let prepared = match &self.docker_home {
                Some(scope) => Ok(scope.clone()),
                None => crate::docker::Shared::prepare(
                    &self.launch,
                    self.inheritance(),
                    &self.directory,
                    self.docker_selection.lock().unwrap().name.clone(),
                    &self.cancel,
                ),
            };
            match prepared {
                Ok(scope) => {
                    self.docker_selection.lock().unwrap().scope = Arc::downgrade(&scope);
                    self.docker_home = Some(scope);
                }
                Err(error) if self.launch.docker.as_ref().is_some_and(|d| d.enabled) => {
                    return Err(error);
                }
                Err(error) => {
                    self.docker_error = Some(error.to_string());
                    // A conflicting named scope must not prevent choosing an
                    // anonymous engine later. Keep the intended selection but
                    // give this shell its own compatible network namespace.
                    if self.docker_selection.lock().unwrap().name.is_some() {
                        self.docker_home = crate::docker::Shared::prepare(
                            &self.launch,
                            self.inheritance(),
                            &self.directory,
                            None,
                            &self.cancel,
                        )
                        .ok();
                    }
                }
            }
        }
        if let Some(pasta) = &self.launch.pasta {
            if self.docker_home.is_none() {
                self.network = Some(crate::network::Network::start(
                    pasta,
                    &self.launch.posix_shell,
                    &self.directory,
                    &self.cancel,
                )?);
            }
            fs::write(self.directory.join("resolv.conf"), "nameserver 10.0.2.3\n")?;
        }
        let mut path = vec![
            "/run/goblins/packages/current/bin".into(),
            "/run/goblins/bin".into(),
            self.launch.shell.parent().unwrap().display().to_string(),
        ];
        path.extend(
            self.launch
                .initial_packages
                .iter()
                .map(|p| p.join("bin").display().to_string()),
        );
        let mut namespace_args: Vec<String> = [
            "--unshare-all",
            // The helper is namespace-root only while assembling mounts. Map
            // that unprivileged host identity to an ordinary payload user so
            // native agents do not mistake the sandbox for host root.
            "--uid",
            "1000",
            "--gid",
            "1000",
            "--hostname",
            "sandbox",
            "--cap-drop",
            "ALL",
            "--die-with-parent",
            "--clearenv",
        ]
        .map(String::from)
        .to_vec();
        let mut args = Vec::new();
        if self.docker_home.is_some() && self.launch.pasta.is_none() {
            namespace_args.push("--share-net".into());
        }
        if self.launch.pasta.is_some() {
            namespace_args.push("--share-net".into());
            args.extend([
                "--ro-bind".into(),
                self.directory.join("resolv.conf").display().to_string(),
                "/etc/resolv.conf".into(),
            ]);
        }
        for (name, value) in [
            ("HOME", binds.home.display().to_string()),
            ("LC_ALL", "C".into()),
            ("USER", "agent".into()),
            ("LOGNAME", "agent".into()),
            ("PATH", path.join(":")),
            ("SHELL", self.launch.posix_shell.display().to_string()),
            ("PYTHONNOUSERSITE", "1".into()),
            ("TERM", "xterm-256color".into()),
        ] {
            args.extend(["--setenv".into(), name.into(), value]);
        }
        args.extend(["--proc", "/proc", "--dev", "/dev"].map(String::from));
        args.extend([
            "--tmpfs".into(),
            "/tmp".into(),
            "--tmpfs".into(),
            binds.home.display().to_string(),
        ]);
        args.extend([
            "--bind-fd".into(),
            workspace_fd.to_string(),
            "/workspace".into(),
        ]);
        for (flag, source, dest) in [
            ("--ro-bind", self.directory.join("store"), "/nix/store"),
            (
                "--ro-bind",
                self.directory.join("packages"),
                "/run/goblins/packages",
            ),
            (
                "--ro-bind",
                self.launch.client_package.join("bin"),
                "/run/goblins/bin",
            ),
            (
                "--ro-bind",
                self.directory.join("request.sock"),
                "/run/goblins/request.sock",
            ),
        ] {
            args.extend([flag.into(), source.display().to_string(), dest.into()]);
        }
        args.extend([
            "--symlink".into(),
            self.launch.posix_shell.display().to_string(),
            "/bin/sh".into(),
            "--chdir".into(),
            self.launch
                .cwd
                .as_deref()
                .unwrap_or(Path::new("/workspace"))
                .display()
                .to_string(),
        ]);
        for (name, value) in &self.launch.env {
            args.extend(["--setenv".into(), name.clone(), value.clone()]);
        }
        for path in &initial {
            args.extend([
                "--ro-bind".into(),
                path.display().to_string(),
                path.display().to_string(),
            ]);
        }
        binds.append_args(&mut args, &self.directory.join("store"), &initial)?;
        for (source, destination) in &etc {
            args.extend([
                "--ro-bind".into(),
                source.display().to_string(),
                destination.display().to_string(),
            ]);
        }
        let sources: Vec<_> = binds
            .source_fds()
            .chain(std::iter::once(workspace_fd))
            .collect();
        self.docker_filesystem = args.clone();
        // Eager Docker joins need the complete initial package view too; the
        // shell helper is launched only after the engine is ready.
        self.mounted = initial;
        if self
            .docker_inherit_enabled
            .unwrap_or_else(|| self.launch.docker.as_ref().is_some_and(|d| d.enabled))
        {
            self.enable_docker(None, false)?;
        }
        if self.launch.docker.is_some() {
            args.extend([
                "--ro-bind".into(),
                self.directory.join("docker-socket").display().to_string(),
                "/run/goblins/docker-socket".into(),
                "--setenv".into(),
                "DOCKER_HOST".into(),
                "unix:///run/goblins/docker-socket/docker.sock".into(),
            ]);
        }
        namespace_args.append(&mut args);
        let mut args = namespace_args;
        args.push("--remount-ro".into());
        args.push("/".into());
        let (input, output) = if let Some(slave) = terminal {
            (slave.try_clone()?, slave)
        } else {
            args.push("--new-session".into());
            let (in_r, in_w) = unix::pipe()?;
            let (out_r, out_w) = unix::pipe()?;
            self.input = Some(File::from(in_w));
            self.output = Some(File::from(out_r));
            (in_r, out_w)
        };
        let filter = seccomp::filter()?;
        let (control_r, control_w) = unix::pipe()?;
        let (reply_r, reply_w) = unix::pipe()?;
        let mut command = Command::new(&self.launch.helper);
        let network_fds = self
            .network
            .as_ref()
            .map(crate::network::Network::fds)
            .or_else(|| self.docker_home.as_ref().map(|scope| scope.fds()));
        if let Some([user, net]) = network_fds {
            command.args(["--network-namespaces", &user.to_string(), &net.to_string()]);
        }
        command
            .args([
                input.as_raw_fd().to_string(),
                output.as_raw_fd().to_string(),
                filter.as_raw_fd().to_string(),
                sources
                    .iter()
                    .map(|fd| fd.to_string())
                    .collect::<Vec<_>>()
                    .join(","),
            ])
            .arg(&self.launch.bwrap)
            .args(args)
            .args(["--seccomp", &filter.as_raw_fd().to_string(), "--"])
            .arg(&self.launch.shell)
            .args(&self.launch.shell_args)
            .stdin(Stdio::from(control_r))
            .stdout(Stdio::from(reply_w))
            .stderr(File::create(self.directory.join("helper.log"))?);
        let parent = unsafe { libc::getpid() };
        let mut keep = vec![input.as_raw_fd(), output.as_raw_fd(), filter.as_raw_fd()];
        if let Some(fds) = network_fds {
            keep.extend(fds);
        }
        keep.extend(sources);
        unsafe {
            command.pre_exec(move || {
                unix::cvt(libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL))?;
                if libc::getppid() != parent {
                    return Err(std::io::Error::from_raw_os_error(libc::ESRCH));
                }
                // The worker may coexist with frontend threads. Only async-signal
                // safe syscalls run between fork and exec; no namespace work here.
                unix::cvt(libc::syscall(libc::SYS_close_range, 3u32, u32::MAX, 4u32) as i32)?;
                for &fd in &keep {
                    unix::cvt(libc::fcntl(fd, libc::F_SETFD, 0))?;
                }
                Ok(())
            });
        }
        self.cancel.check()?;
        self.helper = Some(command.spawn()?);
        // Retain the pinned handles in the inheritance capability for future
        // children; these temporary command references are no longer needed.
        drop(command);
        drop(binds);
        self.helper_input = Some(File::from(control_w));
        self.helper_output = Some(File::from(reply_r));
        let ready = self.helper_reply(Duration::from_secs(15))?;
        let fields: Vec<_> = ready.split_whitespace().collect();
        if fields.len() != 6 || fields[0] != "READY2" {
            return Err(format!("invalid helper startup reply: {ready}").into());
        }
        self.identity = Some(Identity {
            pid: fields[1].parse()?,
            helper_userns: fields[2].parse()?,
            mount_owner: fields[3].parse()?,
            source_mountns: fields[4].parse()?,
            mountns: fields[5].parse()?,
        });
        self.event("started", serde_json::to_value(&self.identity)?);
        Ok(())
    }
    fn helper_reply(&mut self, timeout: Duration) -> Result<String> {
        process::helper_line(
            self.helper_output.as_mut().ok_or("helper not started")?,
            timeout,
            &self.cancel,
        )
        .map_err(|e| {
            format!(
                "{e}: {}",
                process::diagnostic(&self.directory.join("helper.log"))
            )
            .into()
        })
    }
    fn realize(&self, name: &str) -> Result<PathBuf> {
        let selection = crate::catalog::select(
            &self.launch.flake,
            name,
            &self.directory,
            &self.cancel,
            true,
        )?;
        let attr = crate::catalog::attribute(&self.launch.flake, name);
        let output = selection.output;
        let result = self
            .command(
                crate::catalog::nix()
                    .args(["build", "--no-write-lock-file", "--out-link"])
                    .arg(self.directory.join("roots").join(format!("catalog-{name}")))
                    .args(["--print-out-paths", &format!("{attr}^{output}")]),
            )
            .map_err(|e| format!("could not build package '{name}': {e}"))?;
        let paths: Vec<_> = result.lines().collect();
        if paths.len() != 1 {
            return Err("catalog must resolve to exactly one output".into());
        }
        store_path(Path::new(paths[0]))
    }
    fn profile(&self, packages: &BTreeMap<String, PathBuf>) -> Result<PathBuf> {
        let mut entries = BTreeMap::new();
        for path in self.launch.initial_packages.iter().chain(packages.values()) {
            let bin = path.join("bin");
            if !bin.is_dir() {
                continue;
            }
            for entry in fs::read_dir(bin)? {
                let entry = entry?;
                if let Some(previous) = entries.insert(entry.file_name(), entry.path())
                    && previous != entry.path()
                {
                    return Err(format!(
                        "package executable collision: {}",
                        entry.file_name().to_string_lossy()
                    )
                    .into());
                }
            }
        }
        let generation = self
            .directory
            .join("packages")
            .join(format!("generation-{}", self.packages.len() + 1));
        fs::create_dir(&generation)?;
        fs::create_dir(generation.join("bin"))?;
        for (name, entry) in entries {
            symlink(entry, generation.join("bin").join(name))?;
        }
        Ok(generation)
    }
    pub fn grant(&mut self, name: &str, output: Option<&Path>) -> Result<()> {
        self.grant_with(name, output, |_, _| Ok(()))
    }
    // A private hook lets Rust tests inject a failure after a real mount or
    // corrupt a trusted placeholder. It is never a protocol/configuration option.
    fn grant_with(
        &mut self,
        name: &str,
        output: Option<&Path>,
        before_mount: impl FnMut(usize, &Path) -> Result<()>,
    ) -> Result<()> {
        self.grant_observed(name, output, before_mount, |_| Ok(()))
    }
    pub(crate) fn grant_observed(
        &mut self,
        name: &str,
        output: Option<&Path>,
        mut before_mount: impl FnMut(usize, &Path) -> Result<()>,
        mut progress: impl FnMut(&'static str) -> Result<()>,
    ) -> Result<()> {
        if !package_name(name) {
            return Err("invalid package attribute".into());
        }
        if self.packages.contains_key(name) {
            return Ok(());
        }
        let path = self.root(&match output {
            Some(p) => p.to_path_buf(),
            None => self.realize(name)?,
        })?;
        let mut packages = self.packages.clone();
        packages.insert(name.into(), path.clone());
        let closure = self.closure(std::slice::from_ref(&path))?;
        let generation = self.profile(&packages)?;
        let missing = closure
            .difference(&self.mounted)
            .cloned()
            .collect::<BTreeSet<_>>();
        // From placeholder preparation through publication, any uncertainty
        // terminates this session. Direct store paths appear before publication.
        let mount = (|| -> Result<()> {
            progress("mounting")?;
            self.placeholders(&missing)?;
            for (count, item) in missing.iter().enumerate() {
                self.cancel.check()?;
                before_mount(
                    count,
                    &self.directory.join("store").join(item.file_name().unwrap()),
                )?;
                writeln!(
                    self.helper_input.as_mut().ok_or("helper not started")?,
                    "{}",
                    item.file_name().unwrap().to_str().unwrap()
                )?;
                if self.helper_reply(Duration::from_secs(30))? != "OK" {
                    return Err("mount not acknowledged".into());
                }
                self.mounted.insert(item.clone());
            }
            self.cancel.check()?;
            progress("publishing")?;
            let staged = self.directory.join("packages/next");
            symlink(generation.file_name().unwrap(), &staged)?;
            fs::rename(staged, self.directory.join("packages/current"))?;
            Ok(())
        })();
        if let Err(error) = mount {
            self.event("mount-failure", serde_json::json!({"package":name}));
            self.stop();
            return Err(error);
        }
        self.packages = packages;
        self.event(
            "ready",
            serde_json::json!({"package":name,"output":path,"closure":closure}),
        );
        Ok(())
    }
    pub fn docker_enabled(&self) -> bool {
        self.docker.is_some()
    }
    pub fn docker_scope(&self) -> Option<String> {
        self.docker_selection.lock().unwrap().name.clone()
    }
    pub fn enable_docker(&mut self, requested: Option<&str>, anonymous: bool) -> Result<()> {
        if requested.is_some_and(|name| !self.inheritance().docker_allowed(name)) {
            return Err("Docker scope is not allowed by this configuration".into());
        }
        if self.docker_enabled()
            && !anonymous
            && requested.is_none_or(|name| self.docker_scope().as_deref() == Some(name))
        {
            return Ok(());
        }
        if self.docker_home.is_none() {
            return Err(format!(
                "Docker unavailable in this session: {}; fix prerequisites and start a new sandbox",
                self.docker_error
                    .as_deref()
                    .unwrap_or("configuration lacks Docker runtime support")
            )
            .into());
        }
        let config = self
            .launch
            .docker
            .clone()
            .ok_or("Docker runtime unavailable")?;
        let current = self.docker_selection.lock().unwrap().scope.upgrade();
        let intended = self.docker_scope();
        let requested = requested.or(intended.as_deref());
        let selected = if anonymous {
            crate::docker::Shared::prepare(
                &self.launch,
                self.inheritance(),
                &self.directory,
                None,
                &self.cancel,
            )?
        } else if let Some(name) = requested {
            crate::docker::Shared::prepare(
                &self.launch,
                self.inheritance(),
                &self.directory,
                Some(name.into()),
                &self.cancel,
            )?
        } else {
            current
                .or_else(|| self.docker_home.clone())
                .ok_or("missing Docker scope")?
        };
        if let Some(client) = &config.client
            && !self.launch.initial_packages.contains(client)
        {
            self.grant("docker-client", Some(client))?;
        }
        let package = |p: &Path| {
            p.parent()
                .and_then(Path::parent)
                .map(Path::to_path_buf)
                .ok_or("invalid executable path")
        };
        let closure = self.closure(&[package(&config.daemon)?, package(&self.launch.helper)?])?;
        self.placeholders(&closure)?;
        let mut filesystem = self.docker_filesystem.clone();
        let mut grants: Vec<crate::docker_shared::Grant> = self
            .binds
            .as_ref()
            .unwrap()
            .mounts
            .iter()
            .map(crate::binds::Mount::docker_grant)
            .collect::<std::io::Result<_>>()?;
        // With an explicit cwd, /workspace is only an unused scratch alias.
        // Real snapshots must identify the same pinned workspace when sharing.
        if self.launch.cwd.is_none() {
            grants.push((
                self.workspace.as_ref().unwrap().file.try_clone()?,
                "/workspace".into(),
                false,
            ));
        }
        for (source, destination) in crate::sandbox_etc::mounts(&self.launch.sandbox_etc)? {
            grants.push((File::open(source)?, destination, true));
        }
        for path in self.mounted.union(&closure) {
            grants.push((File::open(path)?, path.clone(), true));
        }
        for path in self.mounted.union(&closure) {
            filesystem.extend([
                "--ro-bind".into(),
                path.display().to_string(),
                path.display().to_string(),
            ]);
        }
        let sources: Vec<_> = self
            .binds
            .as_ref()
            .ok_or("missing filesystem plan")?
            .source_fds()
            .chain(std::iter::once(
                self.workspace
                    .as_ref()
                    .ok_or("missing workspace")?
                    .file
                    .as_raw_fd(),
            ))
            .collect();
        let lease = selected.acquire(
            &self.launch,
            &filesystem,
            &sources,
            grants,
            self.inheritance(),
            self.directory.join("store"),
            &self.cancel,
        )?;
        let socket = self.directory.join("docker-socket/docker.sock");
        let staged = self.directory.join("docker-socket/next.sock");
        let forward = crate::docker_forward::Forward::start(
            self.launch.helper.clone(),
            self.docker_home.as_ref().unwrap().clone(),
            selected.clone(),
            &self.directory,
        )?;
        fs::hard_link(selected.socket(), &staged)?;
        fs::rename(staged, socket)?;
        self.docker = Some(lease);
        self.docker_forward = forward;
        *self.docker_selection.lock().unwrap() = crate::docker::Selection {
            scope: Arc::downgrade(&selected),
            name: selected.name.clone(),
            enabled: true,
        };
        self.event("docker-enabled", serde_json::json!({"scope":selected.name}));
        Ok(())
    }
    pub fn alive(&mut self) -> bool {
        if self
            .docker_home
            .as_ref()
            .is_some_and(|scope| !scope.alive())
            || self
                .docker
                .as_ref()
                .is_some_and(|lease| !lease.shared.alive())
        {
            self.exit_code = Some(1);
            return false;
        }
        match self.helper.as_mut().map(Child::try_wait) {
            Some(Ok(None)) => true,
            Some(Ok(Some(status))) => {
                self.exit_code = status.code();
                false
            }
            _ => false,
        }
    }
    pub(crate) fn exit_watch(&self) -> Result<OwnedFd> {
        let pid = self.helper_pid().ok_or("helper not started")?;
        Ok(unix::owned(unsafe {
            libc::syscall(libc::SYS_pidfd_open, pid, 0) as i32
        })?)
    }
    pub fn helper_pid(&self) -> Option<u32> {
        self.helper.as_ref().map(Child::id)
    }
    pub fn stop(&mut self) {
        self.network.take();
        // SIGKILL is intentional: helper parent-death/PID namespace teardown
        // ends all sandbox descendants, even while a mount command is pending.
        if let Some(mut helper) = self.helper.take() {
            if let Ok(Some(status)) = helper.try_wait() {
                self.exit_code = status.code();
            } else {
                let _ = helper.kill();
                let _ = helper.wait();
            }
        }
        self.docker_forward.take();
        self.docker.take();
        self.docker_home.take();
        self.docker_selection.lock().unwrap().enabled = false;
        self.helper_input.take();
        self.helper_output.take();
        self.event("stopped", serde_json::json!({}));
    }
}
impl Drop for Session {
    fn drop(&mut self) {
        self.stop();
    }
}

#[cfg(test)]
mod tests;
