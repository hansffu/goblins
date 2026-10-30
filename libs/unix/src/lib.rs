//! Shared POSIX descriptor and filesystem helpers; no workspace dependencies.
use std::{
    ffi::CString,
    fs::{self, File},
    io,
    os::{
        fd::{AsRawFd, FromRawFd, OwnedFd, RawFd},
        unix::fs::{MetadataExt, OpenOptionsExt},
    },
    path::{Path, PathBuf},
};
pub type Result<T> = std::result::Result<T, Box<dyn std::error::Error + Send + Sync>>;
pub fn cvt(n: i32) -> io::Result<i32> {
    if n < 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(n)
    }
}
pub fn owned(n: i32) -> io::Result<OwnedFd> {
    cvt(n).map(|n| unsafe { OwnedFd::from_raw_fd(n) })
}
pub fn nonblocking(fd: RawFd) -> io::Result<()> {
    let flags = cvt(unsafe { libc::fcntl(fd, libc::F_GETFL) })?;
    cvt(unsafe { libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) })?;
    Ok(())
}
pub fn pipe() -> io::Result<(OwnedFd, OwnedFd)> {
    let mut fds = [0; 2];
    cvt(unsafe { libc::pipe2(fds.as_mut_ptr(), libc::O_CLOEXEC) })?;
    Ok((owned(fds[0])?, owned(fds[1])?))
}
pub fn readable(fd: RawFd, millis: i32) -> io::Result<bool> {
    let mut p = libc::pollfd {
        fd,
        events: libc::POLLIN,
        revents: 0,
    };
    match cvt(unsafe { libc::poll(&mut p, 1, millis) }) {
        Ok(n) => Ok(n > 0),
        Err(e) if e.kind() == io::ErrorKind::Interrupted => Ok(false),
        Err(e) => Err(e),
    }
}
pub fn recv(fd: RawFd, bytes: &mut [u8]) -> io::Result<usize> {
    let n = unsafe {
        libc::recv(
            fd,
            bytes.as_mut_ptr().cast(),
            bytes.len(),
            libc::MSG_DONTWAIT | libc::MSG_TRUNC,
        )
    };
    if n < 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(n as usize)
    }
}
pub fn send(fd: RawFd, bytes: &[u8]) -> io::Result<()> {
    let n = unsafe {
        libc::send(
            fd,
            bytes.as_ptr().cast(),
            bytes.len(),
            libc::MSG_NOSIGNAL | libc::MSG_DONTWAIT,
        )
    };
    if n < 0 {
        return Err(io::Error::last_os_error());
    }
    if n as usize != bytes.len() {
        return Err(io::Error::other("short socket response"));
    }
    Ok(())
}
pub fn pty(rows: u16, cols: u16) -> io::Result<(OwnedFd, OwnedFd)> {
    let mut master = -1;
    let mut slave = -1;
    let size = libc::winsize {
        ws_row: rows,
        ws_col: cols,
        ws_xpixel: 0,
        ws_ypixel: 0,
    };
    cvt(unsafe {
        libc::openpty(
            &mut master,
            &mut slave,
            std::ptr::null_mut(),
            std::ptr::null(),
            &size,
        )
    })?;
    let master = owned(master)?;
    let slave = owned(slave)?;
    for fd in [&master, &slave] {
        cvt(unsafe { libc::fcntl(fd.as_raw_fd(), libc::F_SETFD, libc::FD_CLOEXEC) })?;
    }
    Ok((master, slave))
}
pub fn private_directory(path: &Path) -> Result<()> {
    use std::os::unix::fs::DirBuilderExt;
    fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(path)?;
    let meta = fs::symlink_metadata(path)?;
    if !meta.is_dir() || meta.uid() != unsafe { libc::getuid() } || meta.mode() & 0o7777 != 0o700 {
        return Err(
            "control directory must be a non-symlink directory owned by you with mode 0700".into(),
        );
    }
    Ok(())
}
pub fn lock(path: &Path) -> Result<File> {
    let f = fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(path)?;
    cvt(unsafe { libc::flock(f.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) })
        .map_err(|_| format!("another Goblins process holds {}", path.display()))?;
    Ok(f)
}
pub fn temp_directory() -> Result<PathBuf> {
    let mut path = CString::new("/tmp/goblins-XXXXXX")?.into_bytes_with_nul();
    if unsafe { libc::mkdtemp(path.as_mut_ptr().cast()) }.is_null() {
        return Err(io::Error::last_os_error().into());
    }
    path.pop();
    Ok(PathBuf::from(std::ffi::OsString::from_vec(path)))
}
use std::os::unix::ffi::OsStringExt;

/// A host-private directory under `$XDG_CACHE_HOME/goblins`. Resolve even a
/// not-yet-created path through its existing ancestors, so bind validation
/// protects this host-only tree for sessions that never create it.
pub fn cache_directory(component: &str, create: bool) -> Result<PathBuf> {
    xdg_directory("XDG_CACHE_HOME", ".cache", component, create)
}
/// The same under `$XDG_STATE_HOME/goblins` (default `~/.local/state`).
pub fn state_directory(component: &str, create: bool) -> Result<PathBuf> {
    xdg_directory("XDG_STATE_HOME", ".local/state", component, create)
}
fn xdg_directory(variable: &str, fallback: &str, component: &str, create: bool) -> Result<PathBuf> {
    let base = std::env::var_os(variable)
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(fallback)))
        .ok_or_else(|| format!("Goblins storage requires HOME or an absolute {variable}"))?;
    if !base.is_absolute() {
        return Err("Goblins storage directory must be absolute".into());
    }
    let root = base.join("goblins").join(component);
    if create {
        private_directory(&root)?;
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
                suffix.push(ancestor.file_name().ok_or("invalid Goblins storage path")?);
                ancestor = ancestor.parent().ok_or("invalid Goblins storage path")?;
            }
            Err(e) => return Err(e.into()),
        }
    }
}

/// The soft descriptor limit Goblins was started with, restored for sandbox
/// payloads (0 when unchanged).
static PAYLOAD_NOFILE: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// The server holds descriptors for every session's pinned grants and, while
/// members join a scope's Docker engine, for their store closures. The
/// conventional soft limit of 1024 is too small for several large agents.
pub fn raise_descriptor_limit() {
    let mut limit = libc::rlimit {
        rlim_cur: 0,
        rlim_max: 0,
    };
    if unsafe { libc::getrlimit(libc::RLIMIT_NOFILE, &mut limit) } != 0
        || limit.rlim_cur >= limit.rlim_max
    {
        return;
    }
    let original = limit.rlim_cur;
    limit.rlim_cur = limit.rlim_max;
    if unsafe { libc::setrlimit(libc::RLIMIT_NOFILE, &limit) } == 0 {
        PAYLOAD_NOFILE.store(original, std::sync::atomic::Ordering::Relaxed);
    }
}

/// The limit to restore in a sandbox payload, if the server raised its own.
pub fn payload_descriptor_limit() -> Option<libc::rlimit> {
    let original = PAYLOAD_NOFILE.load(std::sync::atomic::Ordering::Relaxed);
    let mut limit = libc::rlimit {
        rlim_cur: 0,
        rlim_max: 0,
    };
    (original != 0 && unsafe { libc::getrlimit(libc::RLIMIT_NOFILE, &mut limit) } == 0).then(|| {
        limit.rlim_cur = original;
        limit
    })
}

/// Forking copies every open descriptor until the child executes, for a
/// moment, and a copy of a lock's descriptor keeps the lock held. Tests that
/// fork take `forking()`, and tests that need a lock to be free the moment
/// its holder lets go take `no_forks()`, so the two never overlap.
#[cfg(any(test, feature = "test-support"))]
static FORKS: std::sync::RwLock<()> = std::sync::RwLock::new(());
#[cfg(any(test, feature = "test-support"))]
pub fn forking() -> std::sync::RwLockWriteGuard<'static, ()> {
    FORKS.write().unwrap_or_else(|e| e.into_inner())
}
#[cfg(any(test, feature = "test-support"))]
pub fn no_forks() -> std::sync::RwLockReadGuard<'static, ()> {
    FORKS.read().unwrap_or_else(|e| e.into_inner())
}
