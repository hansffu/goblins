//! A scope-owned engine, initially launched from a member's confined bind plan.
//! No host Docker socket, user service or global daemon is involved.
use crate::{Result, config::Launch, process, unix};
use std::{
    ffi::{CStr, CString},
    fs::{self, File},
    io::{Read, Write},
    os::{
        fd::{AsRawFd, RawFd},
        unix::{
            ffi::{OsStrExt, OsStringExt},
            net::UnixStream,
            process::CommandExt,
        },
    },
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    time::{Duration, Instant},
};

pub struct Engine {
    process: Child,
    lifetime: Option<File>,
    _storage: Storage,
    pub root: File,
    pub mounts: File,
}

/// Resolve even a not-yet-created cache through its existing ancestors, so
/// bind validation protects this host-only tree for Docker-disabled shells too.
pub fn storage_root(create: bool) -> Result<PathBuf> {
    let cache = std::env::var_os("XDG_CACHE_HOME")
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".cache")))
        .ok_or("Docker storage requires HOME or an absolute XDG_CACHE_HOME")?;
    if !cache.is_absolute() {
        return Err("Docker cache directory must be absolute".into());
    }
    let root = cache.join("goblins/docker");
    if create {
        unix::private_directory(&root)?;
    }
    let mut ancestor = root.as_path();
    let mut suffix = Vec::new();
    loop {
        match fs::canonicalize(ancestor) {
            Ok(mut path) => {
                for name in suffix.into_iter().rev() {
                    path.push(name);
                }
                return Ok(path);
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                suffix.push(ancestor.file_name().ok_or("invalid Docker cache path")?);
                ancestor = ancestor.parent().ok_or("invalid Docker cache path")?;
            }
            Err(e) => return Err(e.into()),
        }
    }
}

struct Storage {
    path: PathBuf,
    parent: File,
    directory: File,
    lock: Option<File>,
}
impl Storage {
    fn new(root: &Path, name: Option<&str>) -> Result<Self> {
        let parent = File::open(root)?;
        let mut stat: libc::statfs = unsafe { std::mem::zeroed() };
        unix::cvt(unsafe { libc::fstatfs(parent.as_raw_fd(), &mut stat) })?;
        if stat.f_type == libc::TMPFS_MAGIC || stat.f_type as u64 == 0x858458f6 {
            // RAMFS_MAGIC
            return Err("Docker storage must be disk-backed; set XDG_CACHE_HOME to a disk filesystem before starting Goblins".into());
        }
        if let Some(name) = name {
            if !goblins_protocol::docker_scope_name(name) {
                return Err("invalid scope name".into());
            }
            let locks = root.join("locks");
            unix::private_directory(&locks)?;
            let lock = File::options()
                .read(true)
                .write(true)
                .create(true)
                .truncate(false)
                .open(locks.join(name))?;
            if unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
                return Err(
                    format!("Docker scope '{name}' is in use by another controller").into(),
                );
            }
            let path = root.join(format!("scope-{name}"));
            // Dockerd changes its data root to 0710/0711. Its parent remains
            // host-private; do not mistake Docker's normal mode for corruption.
            match fs::symlink_metadata(&path) {
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                    unix::private_directory(&path)?
                }
                Ok(meta) => {
                    use std::os::unix::fs::MetadataExt;
                    if !meta.is_dir() || meta.uid() != unsafe { libc::getuid() } {
                        return Err("unsafe named Docker data directory".into());
                    }
                }
                Err(e) => return Err(e.into()),
            }
            let directory = File::open(&path)?;
            return Ok(Self {
                path,
                parent,
                directory,
                lock: Some(lock),
            });
        }
        let mut template =
            CString::new(root.join("session-XXXXXX").as_os_str().as_bytes())?.into_bytes_with_nul();
        if unsafe { libc::mkdtemp(template.as_mut_ptr().cast()) }.is_null() {
            return Err(std::io::Error::last_os_error().into());
        }
        template.pop();
        let path = PathBuf::from(std::ffi::OsString::from_vec(template));
        let directory = File::open(&path)?;
        Ok(Self {
            path,
            parent,
            directory,
            lock: None,
        })
    }
}
impl Drop for Storage {
    fn drop(&mut self) {
        if self.lock.is_some() {
            return;
        }
        // Only an EMPTY directory may be removed by the unmapped host user.
        // The mapped guardian removes populated trees, without following links.
        if let Err(error) = fs::remove_dir(&self.path) {
            if error.kind() != std::io::ErrorKind::NotFound {
                eprintln!(
                    "Docker storage cleanup incomplete at {}: {error}",
                    self.path.display()
                );
            }
        }
    }
}

pub(crate) fn subordinate(kind: &str, id: u32) -> Result<u32> {
    let name = unsafe {
        let mut entry: libc::passwd = std::mem::zeroed();
        let mut result = std::ptr::null_mut();
        let mut buffer = vec![0u8; 65536];
        let error = libc::getpwuid_r(
            libc::getuid(),
            &mut entry,
            buffer.as_mut_ptr().cast(),
            buffer.len(),
            &mut result,
        );
        if error != 0 || result.is_null() {
            return Err("cannot resolve current user for Docker UID mappings".into());
        }
        CStr::from_ptr(entry.pw_name).to_string_lossy().into_owned()
    };
    let text = fs::read_to_string(format!("/etc/sub{kind}")).map_err(|e| {
        format!("docker.enable requires /etc/sub{kind} entries and newuidmap/newgidmap: {e}")
    })?;
    let numeric = unsafe { libc::getuid() }.to_string();
    for line in text.lines() {
        let fields: Vec<_> = line.split(':').collect();
        if fields.len() == 3 && (fields[0] == name || fields[0] == numeric) {
            let start: u32 = fields[1].parse()?;
            let count: u32 = fields[2].parse()?;
            if count >= 65536
                && start
                    .checked_add(65536)
                    .is_some_and(|end| id < start || id >= end)
            {
                return Ok(start);
            }
        }
    }
    Err(format!("docker.enable requires at least 65536 subordinate {kind}s for {name}").into())
}

fn ready(path: &Path) -> bool {
    (|| -> std::io::Result<bool> {
        let mut socket = UnixStream::connect(path)?;
        socket.set_read_timeout(Some(Duration::from_millis(100)))?;
        socket.set_write_timeout(Some(Duration::from_millis(100)))?;
        socket.write_all(b"GET /_ping HTTP/1.0\r\nHost: docker\r\n\r\n")?;
        let mut response = String::new();
        socket.take(4096).read_to_string(&mut response)?;
        Ok(response.starts_with("HTTP/1.0 200 ") || response.starts_with("HTTP/1.1 200 "))
    })()
    .unwrap_or(false)
}

pub(crate) fn map_ids(
    kind: &str,
    pid: u32,
    id: u32,
    subordinate: u32,
    directory: &Path,
    cancel: &process::Cancellation,
) -> Result<()> {
    let name = format!("new{kind}map");
    // NixOS's privileged wrappers live outside the store; conventional Linux
    // distributions install these helpers in /usr/bin. Never expose them inside.
    let helper = [
        PathBuf::from("/run/wrappers/bin").join(&name),
        PathBuf::from("/usr/bin").join(&name),
    ]
    .into_iter()
    .find(|p| p.is_file())
    .ok_or_else(|| format!("docker.enable requires installed {name}"))?;
    process::command(
        Command::new(helper).args([
            pid.to_string(),
            "0".into(),
            id.to_string(),
            "1".into(),
            "1".into(),
            subordinate.to_string(),
            "65536".into(),
        ]),
        directory,
        cancel,
    )
    .map_err(|e| {
        format!("Docker {name} failed (launch Goblins outside an existing restricted sandbox): {e}")
    })?;
    Ok(())
}

impl Engine {
    pub fn alive(&mut self) -> bool {
        matches!(self.process.try_wait(), Ok(None))
    }

    pub fn start(
        launch: &Launch,
        filesystem: &[String],
        sources: &[RawFd],
        storage_root: &Path,
        directory: &Path,
        cancel: &process::Cancellation,
        scope: &crate::docker_scope::Scope,
        name: Option<&str>,
    ) -> Result<Self> {
        let config = launch.docker.as_ref().ok_or("Docker not enabled")?;
        let storage = Storage::new(storage_root, name)?;
        // State paths can exceed sockaddr_un's limit (notably under
        // nix develop's nested TMPDIR). Pin the socket directory and connect
        // through a short procfs path; the daemon uses its short sandbox path.
        let socket_directory = File::open(directory.join("docker-socket"))?;
        let socket_path = PathBuf::from(format!(
            "/proc/self/fd/{}/docker.sock",
            socket_directory.as_raw_fd()
        ));
        fs::write(
            directory.join("docker.json"),
            r#"{"features":{"containerd-snapshotter":false}}"#,
        )?;
        fs::write(
            directory.join("docker-passwd"),
            "root:x:0:0:root:/root:/bin/sh\n",
        )?;
        fs::write(directory.join("docker-group"), "root:x:0:\n")?;
        let (lifetime_r, lifetime_w) = unix::pipe()?;
        let (info_r, info_w) = unix::pipe()?;
        let (ready_r, ready_w) = unix::pipe()?;
        let filter = crate::seccomp::docker_filter()?;
        let mut command = Command::new(&launch.helper);
        command
            .args([
                "--docker-enter",
                &scope.outer.as_raw_fd().to_string(),
                &scope.net.as_raw_fd().to_string(),
            ])
            .arg(&launch.bwrap)
            .args([
                "--unshare-pid",
                "--unshare-ipc",
                "--unshare-uts",
                "--unshare-cgroup",
                "--cap-add",
                "ALL",
                "--new-session",
                "--clearenv",
                "--info-fd",
                &info_w.as_raw_fd().to_string(),
            ])
            .args(filesystem)
            .args([
                "--tmpfs",
                "/run/docker",
                "--bind-fd",
                &storage.directory.as_raw_fd().to_string(),
                "/run/docker/data",
                "--bind-fd",
                &storage.parent.as_raw_fd().to_string(),
                "/run/goblins/docker-storage-parent",
                "--tmpfs",
                "/run/containerd",
                "--ro-bind",
                "/sys/fs/cgroup",
                "/run/docker/cgroup",
                "--ro-bind",
                "/sys",
                "/sys",
                "--ro-bind",
                &directory.join("docker-passwd").display().to_string(),
                "/etc/passwd",
                "--ro-bind",
                &directory.join("docker-group").display().to_string(),
                "/etc/group",
                "--bind",
                &directory.join("docker-socket").display().to_string(),
                "/run/goblins/docker-socket",
                "--ro-bind",
                &directory.join("docker.json").display().to_string(),
                "/run/docker/config.json",
                "--setenv",
                "XDG_RUNTIME_DIR",
                "/run/docker",
                "--setenv",
                "GOBLINS_CGROUPNS",
                "host",
                "--seccomp",
                &filter.as_raw_fd().to_string(),
                "--",
            ])
            .arg(&launch.helper)
            .arg("--docker-init")
            .arg(storage.path.file_name().unwrap())
            .arg(if name.is_some() { "keep" } else { "delete" })
            .arg(
                storage
                    .lock
                    .as_ref()
                    .map_or(-1, AsRawFd::as_raw_fd)
                    .to_string(),
            )
            .arg(ready_w.as_raw_fd().to_string())
            .arg(&config.daemon)
            // Keep Docker's bridge, forwarding and NAT defaults. The scope
            // owns the network namespace, so these affect only this
            // session; pasta separately controls its upstream connectivity.
            .args([
                "--config-file=/run/docker/config.json",
                "--log-level=error",
                "--rootless",
                "--host=unix:///run/goblins/docker-socket/docker.sock",
                "--group=0",
                "--data-root=/run/docker/data",
                "--exec-root=/run/docker/exec",
                "--pidfile=/run/docker/docker.pid",
                "--storage-driver=overlay2",
                "--default-cgroupns-mode=host",
            ])
            .stdin(Stdio::from(lifetime_r))
            .stdout(File::create(directory.join("docker.log"))?)
            .stderr(
                File::options()
                    .append(true)
                    .open(directory.join("docker.log"))?,
            );
        let mut keep = sources.to_vec();
        keep.extend([
            scope.outer.as_raw_fd(),
            scope.net.as_raw_fd(),
            filter.as_raw_fd(),
        ]);
        keep.extend([storage.parent.as_raw_fd(), storage.directory.as_raw_fd()]);
        keep.extend([info_w.as_raw_fd(), ready_w.as_raw_fd()]);
        if let Some(lock) = &storage.lock {
            keep.push(lock.as_raw_fd());
        }
        unsafe {
            command.pre_exec(move || {
                // EOF on stdin, rather than SIGKILL, lets the trusted guardian
                // reap containers and clean disk storage even after host death.
                unix::cvt(libc::syscall(libc::SYS_close_range, 3u32, u32::MAX, 4u32) as i32)?;
                for &fd in &keep {
                    unix::cvt(libc::fcntl(fd, libc::F_SETFD, 0))?;
                }
                Ok(())
            });
        }
        let mut child = command.spawn()?;
        drop(command);
        drop((info_w, ready_w));
        let setup = (|| -> Result<(File, File)> {
            let mut info = File::from(info_r);
            let mut json = String::new();
            for _ in 0..20 {
                let line = process::helper_line(&mut info, Duration::from_secs(10), cancel)?;
                json.push_str(&line);
                if line.trim() == "}" {
                    break;
                }
            }
            let info: serde_json::Value = serde_json::from_str(&json)?;
            let pid = info["child-pid"].as_u64().ok_or("missing engine PID")?;
            let nested: u32 =
                process::helper_line(&mut File::from(ready_r), Duration::from_secs(15), cancel)?
                    .trim()
                    .parse()?;
            let deadline = Instant::now() + Duration::from_secs(30);
            loop {
                cancel.check()?;
                if child.try_wait()?.is_some() {
                    return Err("Docker engine exited during startup".into());
                }
                if ready(&socket_path) {
                    break;
                }
                if Instant::now() >= deadline {
                    return Err("Docker engine startup timed out".into());
                }
                std::thread::sleep(Duration::from_millis(20));
            }
            let proc = format!("/proc/{pid}/root/proc/{nested}");
            Ok((
                File::open(format!("{proc}/root"))?,
                File::open(format!("{proc}/ns/mnt"))?,
            ))
        })();
        match setup {
            Ok((root, mounts)) => Ok(Self {
                process: child,
                lifetime: Some(File::from(lifetime_w)),
                _storage: storage,
                root,
                mounts,
            }),
            Err(error) => {
                drop(lifetime_w);
                let _ = child.wait();
                let diagnostic = process::diagnostic(&directory.join("docker.log"));
                let tail: String = diagnostic
                    .chars()
                    .rev()
                    .take(380)
                    .collect::<Vec<_>>()
                    .into_iter()
                    .rev()
                    .collect();
                Err(format!("{error}: {tail}").into())
            }
        }
    }
}
impl Drop for Engine {
    fn drop(&mut self) {
        self.lifetime.take();
        if let Err(error) = self.process.wait().and_then(|status| {
            if status.success() {
                Ok(())
            } else {
                Err(std::io::Error::other(format!(
                    "guardian exited with {status}"
                )))
            }
        }) {
            eprintln!("Docker shutdown: {error}");
        }
    }
}

pub(crate) use crate::docker_shared::{Lease, Selection, Shared};
