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
    sync::{
        Arc, Weak,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};

fn canonical_store_output(path: &Path) -> bool {
    let name = path.file_name().and_then(|p| p.to_str());
    let Some(name) = name else {
        return false;
    };
    path.as_os_str() == std::ffi::OsStr::new(&format!("/nix/store/{name}"))
        && path.parent() == Some(Path::new("/nix/store"))
        && (34..=211).contains(&name.len())
        && name.as_bytes()[32] == b'-'
        && name.bytes().enumerate().all(|(i, b)| {
            if i < 32 {
                b"0123456789abcdfghijklmnpqrsvwxyz".contains(&b)
            } else {
                b.is_ascii_alphanumeric() || b"+-._?=".contains(&b)
            }
        })
}

fn containing_store_output(path: &Path) -> Option<PathBuf> {
    let name = path.strip_prefix("/nix/store").ok()?.components().next()?;
    let root = Path::new("/nix/store").join(name);
    canonical_store_output(&root).then_some(root)
}

pub fn store_path(path: &Path) -> Result<PathBuf> {
    if !canonical_store_output(path) {
        return Err("not a canonical store output".into());
    }
    let meta = fs::symlink_metadata(path)?;
    if meta.is_file() || meta.is_dir() {
        return Ok(path.to_path_buf());
    }
    if meta.is_symlink() {
        // A Nix output may itself be a symlink (for example, a package's
        // helper executable output). Bubblewrap follows the source when it is
        // mounted, so permit it only when canonical resolution remains within
        // another well-formed store output. This preserves the host boundary
        // while accepting valid Nix closures.
        let resolved = fs::canonicalize(path)?;
        let root = containing_store_output(&resolved)
            .ok_or("store output symlink resolves outside the Nix store")?;
        let root_meta = fs::symlink_metadata(root)?;
        let resolved_meta = fs::metadata(&resolved)?;
        if (root_meta.is_file() || root_meta.is_dir())
            && (resolved_meta.is_file() || resolved_meta.is_dir())
        {
            return Ok(path.to_path_buf());
        }
    }
    Err("store output must be a regular file, directory, or store-contained symlink".into())
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
    // Weak: a stopped parent's record must not keep its scope alive.
    scope: Weak<crate::scope::Shared>,
    docker: Arc<AtomicBool>,
    _owner: Arc<SessionDirectory>,
}
impl Inheritance {
    pub(crate) fn scope_name(&self) -> Option<&str> {
        self.launch.scope.as_ref().map(|s| s.name.as_str())
    }
    pub(crate) fn docker_available(&self) -> bool {
        self.launch
            .scope
            .as_ref()
            .is_some_and(|s| s.definition.docker)
    }
    pub(crate) fn description(&self) -> &str {
        &self.launch.description
    }
    pub(crate) fn integration(&self) -> Option<&str> {
        self.launch.integration.as_deref()
    }
    pub(crate) fn dev_shell(&self) -> Option<&crate::devshell::DevShell> {
        self.launch.dev_shell.as_ref()
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
    scope: Option<Arc<crate::scope::Shared>>,
    docker: Option<crate::scope::Lease>,
    // Shared with children's inheritance: they start with Docker attached
    // when their parent had it at the time they were created.
    docker_attached: Arc<AtomicBool>,
    docker_inherit_enabled: Option<bool>,
    docker_filesystem: Vec<String>,
    pub exit_code: Option<i32>,
    helper_input: Option<File>,
    helper_output: Option<File>,
    cancel: Cancellation,
    directory_owner: Arc<SessionDirectory>,
    binds: Option<Arc<crate::binds::Plan>>,
    workspace: Option<Arc<SharedWorkspace>>,
    candidate: Option<crate::devshell::Candidate>,
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
            scope: None,
            docker: None,
            docker_attached: Arc::new(AtomicBool::new(false)),
            docker_inherit_enabled: None,
            docker_filesystem: Vec::new(),
            exit_code: None,
            helper_input: None,
            helper_output: None,
            cancel,
            binds: None,
            workspace: None,
            candidate: None,
        };
        for name in ["store", "packages", "roots", "workspace"] {
            fs::create_dir(session.directory.join(name))?;
        }
        if let Some(source) = workspace {
            let resolved = fs::canonicalize(source)?;
            for storage in [
                crate::docker::storage_root(false)?,
                crate::scope_storage::root(false)?,
            ] {
                if resolved.starts_with(&storage) || storage.starts_with(&resolved) {
                    return Err("workspace snapshot overlaps protected Goblins storage".into());
                }
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
        if session.launch.scope.is_some() {
            session.scope = Some(
                inherited
                    .scope
                    .upgrade()
                    .ok_or("the parent's scope has stopped")?,
            );
        }
        let enabled = inherited.docker.load(Ordering::SeqCst);
        session.docker_inherit_enabled = Some(enabled);
        if enabled
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
        session.binds = Some(inherited.binds);
        session.workspace = Some(inherited.workspace);
        Ok(session)
    }
    pub(crate) fn inheritance(&self) -> Inheritance {
        Inheritance {
            launch: Arc::new(self.launch.clone()),
            binds: self.binds.as_ref().unwrap().clone(),
            workspace: self.workspace.as_ref().unwrap().clone(),
            scope: self.scope.as_ref().map(Arc::downgrade).unwrap_or_default(),
            docker: self.docker_attached.clone(),
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
        private.push(crate::docker::storage_root(false)?);
        private.push(crate::scope_storage::root(false)?);
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
        let docker = self
            .launch
            .scope
            .as_ref()
            .is_some_and(|s| s.definition.docker);
        if let Some(docker) = self.launch.docker.as_ref().filter(|_| docker) {
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
        if let Some(dev) = &self.launch.dev_shell {
            // Retain the evaluated source for refresh diffs; it is never mounted.
            self.root(&dev.source)?;
            roots.push(dev.profile.clone());
        }
        let initial = self.closure(&roots)?;
        self.placeholders(&initial)?;
        let listener = UnixListener::bind(self.directory.join("request.sock"))?;
        fs::set_permissions(
            self.directory.join("request.sock"),
            fs::Permissions::from_mode(0o600),
        )?;
        listener.set_nonblocking(true)?;
        self.listener = Some(listener);
        if self.launch.scope.is_some() && self.scope.is_none() {
            self.scope = Some(crate::scope::Shared::prepare(
                &self.launch,
                self.inheritance(),
                &self.directory,
                &self.cancel,
            )?);
        }
        if docker {
            fs::create_dir(self.directory.join("docker-socket"))?;
        }
        if let Some(pasta) = &self.launch.pasta {
            if self.scope.is_none() {
                self.network = Some(crate::network::Network::start(
                    pasta,
                    &self.launch.posix_shell,
                    &self.directory,
                    &self.cancel,
                )?);
            }
            fs::write(self.directory.join("resolv.conf"), "nameserver 10.0.2.3\n")?;
        }
        let shared_pid = self.scope.as_ref().is_some_and(|s| s.pid.is_some());
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
            "--unshare-user",
            "--unshare-ipc",
            "--unshare-uts",
            "--unshare-cgroup-try",
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
        // The helper joins the scope's (or pasta's) network namespace, and the
        // scope's PID namespace when shared; otherwise Bubblewrap creates them.
        if self.scope.is_none() && self.network.is_none() {
            namespace_args.push("--unshare-net".into());
        }
        if !shared_pid {
            namespace_args.push("--unshare-pid".into());
        }
        let mut args = Vec::new();
        if self.launch.pasta.is_some() {
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
        args.extend(["--dev", "/dev"].map(String::from));
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
        if let Some(dev) = &self.launch.dev_shell {
            let directory = self.directory.join("devshell");
            fs::create_dir_all(&directory)?;
            fs::write(directory.join("env.sh"), &dev.script)?;
            args.extend([
                "--ro-bind".into(),
                directory.display().to_string(),
                "/run/goblins/devshell".into(),
                "--setenv".into(),
                "GOBLINS_DEV_SHELL".into(),
                dev.reference.clone(),
            ]);
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
        let mut sources: Vec<_> = binds
            .source_fds()
            .chain(std::iter::once(workspace_fd))
            .collect();
        self.docker_filesystem = args.clone();
        // The engine has its own PID namespace and mounts a fresh /proc.
        self.docker_filesystem
            .extend(["--proc", "/proc"].map(String::from));
        if shared_pid {
            // The helper mounted the scope's /proc; a member's own user
            // namespace could not. Keep Bubblewrap's read-only /proc hardening.
            args.extend(["--bind", "/proc", "/proc"].map(String::from));
            for path in ["/proc/sys", "/proc/sysrq-trigger", "/proc/irq", "/proc/bus"] {
                args.extend(["--ro-bind-try".into(), path.into(), path.into()]);
            }
        } else {
            args.extend(["--proc", "/proc"].map(String::from));
        }
        // Scope storage is for members only, not the shared engine.
        if let (Some(scope), Some(selected)) = (&self.scope, &self.launch.scope) {
            for (name, destination) in &selected.mounts {
                crate::binds::validate(destination, &private)?;
                if let Some(grant) = binds
                    .destinations()
                    .find(|d| d.starts_with(destination) || destination.starts_with(d))
                {
                    return Err(format!(
                        "scope storage '{name}' at {} overlaps file grant {}",
                        destination.display(),
                        grant.display()
                    )
                    .into());
                }
                let directory = scope.storage(name)?.as_raw_fd();
                sources.push(directory);
                args.extend([
                    "--bind-fd".into(),
                    directory.to_string(),
                    destination.display().to_string(),
                ]);
            }
        }
        // Eager Docker joins need the complete initial package view too; the
        // shell helper is launched only after the engine is ready.
        self.mounted = initial;
        if self
            .docker_inherit_enabled
            .unwrap_or_else(|| self.launch.docker.as_ref().is_some_and(|d| d.enabled))
        {
            self.enable_docker()?;
        }
        if docker {
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
        let namespaces = self
            .network
            .as_ref()
            .map(|network| {
                let [user, net] = network.fds();
                (user, net, None)
            })
            .or_else(|| self.scope.as_ref().map(|scope| scope.fds()));
        if let Some((user, net, pid)) = namespaces {
            command.args([
                "--network-namespaces".into(),
                user.to_string(),
                net.to_string(),
                pid.map_or_else(|| "-".into(), |pid| pid.to_string()),
            ]);
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
            .args(["--seccomp", &filter.as_raw_fd().to_string(), "--"]);
        if self.launch.dev_shell.is_some() {
            // Enter the dev shell like `nix develop -c`: its variables and
            // shellHook apply inside the sandbox, then the entry program
            // inherits the resulting environment.
            command.arg(&self.launch.posix_shell).args([
                "-c",
                ". /run/goblins/devshell/env.sh; exec \"$@\"",
                "goblins-dev-shell",
            ]);
        }
        command
            .arg(&self.launch.shell)
            .args(&self.launch.shell_args)
            .stdin(Stdio::from(control_r))
            .stdout(Stdio::from(reply_w))
            .stderr(File::create(self.directory.join("helper.log"))?);
        let parent = unsafe { libc::getpid() };
        let payload_limit = unix::payload_descriptor_limit();
        let mut keep = vec![input.as_raw_fd(), output.as_raw_fd(), filter.as_raw_fd()];
        if let Some((user, net, pid)) = namespaces {
            keep.extend([user, net]);
            keep.extend(pid);
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
                // Payloads keep the limit Goblins was started with.
                if let Some(limit) = &payload_limit {
                    unix::cvt(libc::setrlimit(libc::RLIMIT_NOFILE, limit))?;
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
    fn profile(&self, package: &Path) -> Result<PathBuf> {
        let mut entries = BTreeMap::new();
        // Match the startup PATH, then append grants in publication order.
        // Duplicate command names are ordinary PATH shadowing, not a reason
        // to reject otherwise independent Nix store closures.
        let current = self.directory.join("packages/current/bin");
        let bins = [
            self.launch.client_package.join("bin"),
            self.launch
                .shell
                .parent()
                .ok_or("invalid shell path")?
                .to_path_buf(),
        ];
        for bin in bins
            .into_iter()
            .chain(self.launch.initial_packages.iter().map(|p| p.join("bin")))
            .chain((!self.packages.is_empty()).then_some(current.clone()))
            .chain(std::iter::once(package.join("bin")))
        {
            if !bin.is_dir() {
                continue;
            }
            for entry in fs::read_dir(&bin)? {
                let entry = entry?;
                // Copy the store target, never a link through mutable `current`.
                let target = if bin == current {
                    fs::read_link(entry.path())?
                } else {
                    entry.path()
                };
                entries.entry(entry.file_name()).or_insert(target);
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
        let generation = self.profile(&path)?;
        let missing = closure
            .difference(&self.mounted)
            .cloned()
            .collect::<BTreeSet<_>>();
        // From placeholder preparation through publication, any uncertainty
        // terminates this session. Direct store paths appear before publication.
        let mount = (|| -> Result<()> {
            progress("mounting")?;
            self.mount_paths(&missing, &mut before_mount)?;
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
    /// Mount store paths into the running sandbox through the helper.
    fn mount_paths(
        &mut self,
        missing: &BTreeSet<PathBuf>,
        before_mount: &mut impl FnMut(usize, &Path) -> Result<()>,
    ) -> Result<()> {
        self.placeholders(missing)?;
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
        Ok(())
    }
    /// Evaluation-time network for a refresh: the goblin's own.
    fn evaluation_network(&self) -> crate::evaluator::Network {
        use crate::evaluator::Network;
        let namespaces = self
            .network
            .as_ref()
            .map(|n| {
                let [user, net] = n.fds();
                (user, net)
            })
            .or_else(|| self.scope.as_ref().map(|s| (s.fds().0, s.fds().1)));
        match namespaces {
            Some((user, net)) if self.launch.pasta.is_some() => Network::Goblin {
                user,
                net,
                resolv: self.directory.join("resolv.conf"),
            },
            _ => Network::None,
        }
    }
    fn evaluator(&self) -> Result<crate::evaluator::Evaluator> {
        crate::evaluator::Evaluator::new(
            &self.directory,
            &self.launch.bwrap,
            self.launch.env.get("SSL_CERT_FILE").map(String::as_str),
            self.evaluation_network(),
        )
    }
    /// Evaluate the workspace flake as the next dev shell generation. Returns
    /// the structured approval preview and the candidate's identity.
    pub fn prepare_refresh(
        &mut self,
        changes: &crate::devshell::Changes,
        cancel: &Cancellation,
    ) -> Result<(serde_json::Value, String)> {
        self.candidate = None;
        let current = self
            .launch
            .dev_shell
            .as_ref()
            .ok_or("this sandbox was not started with a dev shell")?;
        // Evaluation may see only what the sandbox itself can see.
        let cwd = self
            .launch
            .cwd
            .as_ref()
            .ok_or("dev shell refresh is not supported with a daemon --workspace snapshot")?;
        let tree = crate::evaluator::source_tree(&current.path);
        if !tree.starts_with(cwd) {
            return Err(format!(
                "the flake's source tree {} is outside this sandbox's working directory",
                tree.display()
            )
            .into());
        }
        let evaluator = self.evaluator()?;
        let candidate = crate::devshell::candidate(
            current,
            changes,
            &evaluator,
            &[tree],
            evaluator.git(),
            &self.directory,
            cancel,
        )?;
        let result = (
            serde_json::to_value(&candidate.preview)?,
            candidate.identity(&current.attr),
        );
        self.candidate = Some(candidate);
        Ok(result)
    }
    /// Build, mount and publish the prepared candidate as the current
    /// generation. Running processes keep their environment; the agent
    /// sources the new `env.sh`.
    pub fn apply_refresh(&mut self) -> Result<()> {
        let candidate = self
            .candidate
            .take()
            .ok_or("no prepared dev shell refresh")?;
        let current = self.launch.dev_shell.as_ref().ok_or("no dev shell")?;
        let evaluator = self.evaluator()?;
        let next = crate::devshell::apply(
            current,
            candidate,
            &evaluator,
            &self.directory,
            &self.cancel,
        )?;
        self.root(&next.source)?;
        let closure = self.closure(std::slice::from_ref(&next.profile))?;
        let missing = closure
            .difference(&self.mounted)
            .cloned()
            .collect::<BTreeSet<_>>();
        if let Err(error) = self.mount_paths(&missing, &mut |_, _| Ok(())) {
            self.event(
                "mount-failure",
                serde_json::json!({"devshell":next.generation}),
            );
            self.stop();
            return Err(error);
        }
        let directory = self.directory.join("devshell");
        fs::write(directory.join("env.sh.next"), &next.script)?;
        fs::rename(directory.join("env.sh.next"), directory.join("env.sh"))?;
        self.event(
            "devshell",
            serde_json::json!({"generation":next.generation,"profile":next.profile}),
        );
        self.launch.dev_shell = Some(next);
        Ok(())
    }
    pub fn docker_enabled(&self) -> bool {
        self.docker.is_some()
    }
    pub fn scope_name(&self) -> Option<String> {
        self.launch.scope.as_ref().map(|s| s.name.clone())
    }
    /// Attach this sandbox to its scope's shared Docker engine.
    pub fn enable_docker(&mut self) -> Result<()> {
        if self.docker_enabled() {
            return Ok(());
        }
        let selected = self
            .scope
            .clone()
            .ok_or("Docker requires a scope; start this goblin in a scope with docker.enable")?;
        if !selected.definition.docker {
            return Err(format!("scope '{}' does not enable Docker", selected.name).into());
        }
        let config = self
            .launch
            .docker
            .clone()
            .ok_or("Docker runtime unavailable")?;
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
        let mut grants: Vec<crate::scope::Grant> = self
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
        fs::hard_link(selected.socket(), &staged)?;
        fs::rename(staged, socket)?;
        self.docker = Some(lease);
        self.docker_attached.store(true, Ordering::SeqCst);
        self.event("docker-enabled", serde_json::json!({"scope":selected.name}));
        Ok(())
    }
    pub fn alive(&mut self) -> bool {
        if self.scope.as_ref().is_some_and(|scope| !scope.alive())
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
        // Daemons this member started must not outlive it in the shared PID
        // namespace, retaining its mounts and writable grants.
        if let (Some(scope), Some(identity)) = (&self.scope, &self.identity) {
            scope.kill_member(identity.mountns);
        }
        self.docker.take();
        self.scope.take();
        self.docker_attached.store(false, Ordering::SeqCst);
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
