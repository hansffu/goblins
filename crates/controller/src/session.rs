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
use serde::Serialize;
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
#[derive(Clone, Debug, Serialize)]
pub struct Identity {
    pub pid: u32,
    pub helper_userns: u64,
    pub mount_owner: u64,
    pub source_mountns: u64,
    pub mountns: u64,
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
    helper_input: Option<File>,
    helper_output: Option<File>,
    cancel: Cancellation,
}
impl Session {
    pub fn new(launch: Launch, workspace: Option<&Path>, cancel: Cancel) -> Result<Self> {
        let session = Self {
            directory: unix::temp_directory()?,
            launch,
            mounted: BTreeSet::new(),
            packages: BTreeMap::new(),
            identity: None,
            input: None,
            output: None,
            listener: None,
            helper: None,
            helper_input: None,
            helper_output: None,
            cancel,
        };
        for name in ["store", "packages", "roots", "workspace"] {
            fs::create_dir(session.directory.join(name))?;
        }
        if let Some(source) = workspace {
            snapshot::snapshot(
                source,
                &session.directory.join("workspace"),
                &session.cancel,
            )?;
        }
        Ok(session)
    }
    pub fn event(&self, event: &str, fields: serde_json::Value) {
        // Logging must never interrupt teardown or turn a completed grant into
        // an unreported mount failure. Directory is host-only, mode 0700.
        if let Ok(mut file) = OpenOptions::new()
            .create(true)
            .append(true)
            .open(self.directory.join("events.jsonl"))
        {
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
        let mut private = self.launch.protected_paths.clone();
        private.push(self.directory.clone());
        let binds = self.launch.binds.plan(&private, &self.cancel)?;
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
        let mut roots = vec![
            package(&self.launch.shell)?,
            package(&self.launch.posix_shell)?,
            self.launch.client_package.clone(),
        ];
        roots.extend(self.launch.initial_packages.clone());
        roots.extend(self.launch.initial_closure.clone());
        let initial = self.closure(&roots)?;
        self.placeholders(&initial)?;
        let listener = UnixListener::bind(self.directory.join("request.sock"))?;
        fs::set_permissions(
            self.directory.join("request.sock"),
            fs::Permissions::from_mode(0o600),
        )?;
        listener.set_nonblocking(true)?;
        self.listener = Some(listener);
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
        let mut args: Vec<String> = [
            "--unshare-all",
            "--hostname",
            "sandbox",
            "--cap-drop",
            "ALL",
            "--die-with-parent",
            "--clearenv",
        ]
        .map(String::from)
        .to_vec();
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
        args.extend(["--proc", "/proc", "--dev", "/dev", "--tmpfs", "/tmp"].map(String::from));
        args.extend(["--tmpfs".into(), binds.home.display().to_string()]);
        for (flag, source, dest) in [
            ("--bind", self.directory.join("workspace"), "/workspace"),
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
            "/workspace".into(),
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
        command
            .args([
                input.as_raw_fd().to_string(),
                output.as_raw_fd().to_string(),
                filter.as_raw_fd().to_string(),
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
        let keep = [input.as_raw_fd(), output.as_raw_fd(), filter.as_raw_fd()];
        unsafe {
            command.pre_exec(move || {
                unix::cvt(libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL))?;
                if libc::getppid() != parent {
                    return Err(std::io::Error::from_raw_os_error(libc::ESRCH));
                }
                // The worker may coexist with frontend threads. Only async-signal
                // safe syscalls run between fork and exec; no namespace work here.
                unix::cvt(libc::syscall(libc::SYS_close_range, 3u32, u32::MAX, 4u32) as i32)?;
                for fd in keep {
                    unix::cvt(libc::fcntl(fd, libc::F_SETFD, 0))?;
                }
                Ok(())
            });
        }
        self.cancel.check()?;
        self.helper = Some(command.spawn()?);
        self.helper_input = Some(File::from(control_w));
        self.helper_output = Some(File::from(reply_r));
        let ready = self.helper_reply(Duration::from_secs(15))?;
        let fields: Vec<_> = ready.split_whitespace().collect();
        if fields.len() != 6 || fields[0] != "READY" {
            return Err(format!("invalid helper startup reply: {ready}").into());
        }
        self.identity = Some(Identity {
            pid: fields[1].parse()?,
            helper_userns: fields[2].parse()?,
            mount_owner: fields[3].parse()?,
            source_mountns: fields[4].parse()?,
            mountns: fields[5].parse()?,
        });
        self.mounted = initial;
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
        if !package_name(name) {
            return Err("invalid package attribute".into());
        }
        let attr = format!("{}#legacyPackages.x86_64-linux.{name}", self.launch.flake);
        let nix = || {
            let mut c = Command::new("nix");
            c.args(["--extra-experimental-features", "nix-command flakes"]);
            c
        };
        let select = "p: if builtins.isAttrs p && (p.type or null) == \"derivation\" then (p.bin or p).outputName else throw \"attribute is not a package derivation\"";
        let raw = self
            .command(nix().args([
                "eval",
                "--no-write-lock-file",
                "--json",
                &attr,
                "--apply",
                select,
            ]))
            .map_err(|e| format!("cannot resolve package '{name}' from pinned nixpkgs: {e}"))?;
        let output: String = serde_json::from_str(&raw)?;
        if output.is_empty()
            || !output
                .bytes()
                .enumerate()
                .all(|(i, b)| b.is_ascii_alphabetic() || b == b'_' || i > 0 && b.is_ascii_digit())
        {
            return Err("package has an unsupported output name".into());
        }
        let result = self
            .command(
                nix()
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
        mut before_mount: impl FnMut(usize, &Path) -> Result<()>,
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
    pub fn alive(&mut self) -> bool {
        self.helper
            .as_mut()
            .is_some_and(|p| p.try_wait().is_ok_and(|status| status.is_none()))
    }
    pub fn helper_pid(&self) -> Option<u32> {
        self.helper.as_ref().map(Child::id)
    }
    pub fn stop(&mut self) {
        // SIGKILL is intentional: helper parent-death/PID namespace teardown
        // ends all sandbox descendants, even while a mount command is pending.
        if let Some(mut helper) = self.helper.take() {
            let _ = helper.kill();
            let _ = helper.wait();
        }
        self.helper_input.take();
        self.helper_output.take();
        self.event("stopped", serde_json::json!({}));
    }
}
impl Drop for Session {
    fn drop(&mut self) {
        self.stop();
        let _ = fs::remove_dir_all(&self.directory);
    }
}

#[cfg(test)]
mod tests;
