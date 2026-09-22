//! Trusted bootstrap, invoked only after Bubblewrap removed host paths and FDs.
//! It enters the scope's mapped child user namespace before exec, preserving
//! no_new_privs. The child clones the mount namespace to lock every inherited
//! host bind, then creates private IPC and PID namespaces. Networking belongs
//! to the pre-existing scope, shared with the shell.
use super::{c, cvt};
use std::{
    env, fs,
    io::{self, Write},
    os::fd::{AsRawFd, FromRawFd, OwnedFd},
    path::Path,
    process::Command,
};

fn wait(child: i32) -> io::Result<i32> {
    loop {
        let mut status = 0;
        let pid = unsafe { libc::waitpid(-1, &mut status, 0) };
        if pid == child {
            return Ok(if libc::WIFEXITED(status) {
                libc::WEXITSTATUS(status)
            } else {
                128 + libc::WTERMSIG(status)
            });
        }
        if pid < 0 && io::Error::last_os_error().kind() != io::ErrorKind::Interrupted {
            return Err(io::Error::last_os_error());
        }
    }
}

fn mount(kind: &str, target: &str) -> io::Result<()> {
    cvt(unsafe {
        libc::mount(
            c(kind).as_ptr(),
            c(target).as_ptr(),
            c(kind).as_ptr(),
            libc::MS_NOSUID
                | libc::MS_NODEV
                | libc::MS_NOEXEC
                | if kind != "proc" { libc::MS_RDONLY } else { 0 },
            std::ptr::null(),
        )
    })
    .map_err(|e| io::Error::other(format!("Docker {kind} mount: {e}")))?;
    Ok(())
}

fn supervise(child: i32, storage: &Path, keep: bool) -> io::Result<i32> {
    let result = (|| {
        loop {
            let mut status = 0;
            let ended = cvt(unsafe { libc::waitpid(child, &mut status, libc::WNOHANG) })?;
            if ended == child {
                return Ok(if libc::WIFEXITED(status) {
                    libc::WEXITSTATUS(status)
                } else {
                    128 + libc::WTERMSIG(status)
                });
            }
            let mut poll = libc::pollfd {
                fd: 0,
                events: libc::POLLIN,
                revents: 0,
            };
            let n = unsafe { libc::poll(&mut poll, 1, 50) };
            if n > 0 {
                return Ok(0);
            } // Host dropped its private lifetime pipe.
            if n < 0 && io::Error::last_os_error().kind() != io::ErrorKind::Interrupted {
                return Err(io::Error::last_os_error());
            }
        }
    })();
    // We are in the outer engine PID namespace. kill(-1) cannot reach host or
    // sibling sessions, excludes this guardian and Bubblewrap's PID 1, and
    // kills the nested PID-namespace init (which tears down all OCI processes).
    unsafe {
        libc::kill(-1, libc::SIGKILL);
    }
    loop {
        let pid = unsafe { libc::waitpid(-1, std::ptr::null_mut(), 0) };
        if pid < 0 {
            let error = io::Error::last_os_error();
            if error.raw_os_error() == Some(libc::ECHILD) {
                break;
            }
            if error.kind() != io::ErrorKind::Interrupted {
                return Err(error);
            }
        }
    }
    // No untrusted writer remains. This namespace retains the full UID/GID
    // mapping and a private parent-directory mount that Docker never sees.
    // remove_dir_all does not follow symlinks within the data tree.
    if !keep {
        fs::remove_dir_all(storage)
            .map_err(|error| io::Error::other(format!("Docker disk cleanup failed: {error}")))?;
    }
    result
}

pub fn run() -> io::Result<i32> {
    if unsafe { libc::getuid() } != 0
        || fs::read_to_string("/proc/self/uid_map")?
            .split_whitespace()
            .count()
            != 6
    {
        return Err(io::Error::other(
            "Docker bootstrap requires the launcher-provided subordinate UID mapping",
        ));
    }
    // Teardown uses namespace-wide signaling: require that this mapped user
    // namespace owns the PID namespace. Never run it while merely joining a
    // user namespace and still seeing the host's processes.
    let pid_namespace = fs::File::open("/proc/self/ns/pid")?;
    let pid_owner =
        super::fd(unsafe { libc::ioctl(pid_namespace.as_raw_fd(), super::NS_GET_USERNS) })?;
    let user_namespace = fs::File::open("/proc/self/ns/user")?;
    if super::inode(&pid_owner)? != super::inode(&user_namespace.into())? {
        return Err(io::Error::other(
            "Docker guardian must own its PID namespace",
        ));
    }
    drop(pid_namespace);
    drop(pid_owner);
    let name = env::args()
        .nth(2)
        .ok_or_else(|| io::Error::other("missing Docker storage name"))?;
    let keep = match env::args().nth(3).as_deref() {
        Some("keep") => true,
        Some("delete") => false,
        _ => return Err(io::Error::other("invalid Docker storage policy")),
    };
    if !(if keep {
        name.starts_with("scope-") && (7..=70).contains(&name.len())
    } else {
        name.starts_with("session-") && name.len() == 14
    }) || !name
        .bytes()
        .all(|b| b.is_ascii_alphanumeric() || b"_-".contains(&b))
    {
        return Err(io::Error::other("invalid Docker storage name"));
    }
    let lock_fd: i32 = env::args()
        .nth(4)
        .ok_or_else(|| io::Error::other("missing lock descriptor"))?
        .parse()
        .map_err(io::Error::other)?;
    let lock = if keep && lock_fd >= 3 {
        Some(unsafe { OwnedFd::from_raw_fd(lock_fd) })
    } else if !keep && lock_fd == -1 {
        None
    } else {
        return Err(io::Error::other("invalid storage lock"));
    };
    let ready_fd: i32 = env::args()
        .nth(5)
        .ok_or_else(|| io::Error::other("missing readiness descriptor"))?
        .parse()
        .map_err(io::Error::other)?;
    if ready_fd < 3 || ready_fd == lock_fd {
        return Err(io::Error::other("invalid readiness descriptor"));
    }
    let ready = unsafe { OwnedFd::from_raw_fd(ready_fd) };
    let storage = Path::new("/run/goblins/docker-storage-parent").join(name);
    use std::os::unix::fs::MetadataExt;
    let expected = fs::metadata("/run/docker/data")?;
    let actual = fs::symlink_metadata(&storage)?;
    if !actual.is_dir() || actual.ino() != expected.ino() || actual.dev() != expected.dev() {
        return Err(io::Error::other("Docker storage identity mismatch"));
    }
    cvt(unsafe { libc::prctl(libc::PR_SET_CHILD_SUBREAPER, 1) })?;
    let network = fs::File::open("/proc/self/ns/net")?;
    let engine_user = super::fd(unsafe { libc::ioctl(network.as_raw_fd(), super::NS_GET_USERNS) })?;
    drop(network);
    let engine_parent =
        super::fd(unsafe { libc::ioctl(engine_user.as_raw_fd(), super::NS_GET_PARENT) })?;
    let own_user = fs::File::open("/proc/self/ns/user")?;
    if super::inode(&engine_parent)? != super::inode(&own_user.into())? {
        return Err(io::Error::other("Docker scope ancestry mismatch"));
    }
    drop(engine_parent);
    let child = cvt(unsafe { libc::fork() })?;
    if child == 0 {
        drop((ready, lock));
        // Hide the cleanup authority BEFORE entering the less-privileged user
        // namespace that locks inherited mounts. The guardian alone retains it.
        cvt(unsafe { libc::unshare(libc::CLONE_NEWNS) })?;
        cvt(unsafe {
            libc::umount2(
                c("/run/goblins/docker-storage-parent").as_ptr(),
                libc::MNT_DETACH,
            )
        })?;
        let null = fs::File::open("/dev/null")?;
        cvt(unsafe { libc::dup2(null.as_raw_fd(), 0) })?;
        drop(null);
        cvt(unsafe { libc::setns(engine_user.as_raw_fd(), libc::CLONE_NEWUSER) })?;
        drop(engine_user);
        // The mount namespace becomes less privileged here: existing mounts
        // and their readonly/nosuid/nodev flags are locked by the kernel.
        cvt(unsafe {
            libc::unshare(
                libc::CLONE_NEWNS | libc::CLONE_NEWIPC | libc::CLONE_NEWUTS | libc::CLONE_NEWPID,
            )
        })?;
        let init = cvt(unsafe { libc::fork() })?;
        if init != 0 {
            return wait(init);
        }
        // These virtual filesystems describe only the NEW namespaces. The
        // daemon can configure its network without touching host networking.
        mount("proc", "/proc")?;
        mount("sysfs", "/sys")?;
        // Reuse the inherited, kernel-locked readonly mount. A fresh cgroup2
        // mount here could be remounted writable by the engine, exposing the
        // host user's delegated cgroups outside this session.
        cvt(unsafe {
            libc::mount(
                c("/run/docker/cgroup").as_ptr(),
                c("/sys/fs/cgroup").as_ptr(),
                std::ptr::null(),
                libc::MS_BIND,
                std::ptr::null(),
            )
        })?;
        let socket = super::fd(unsafe {
            libc::socket(libc::AF_INET, libc::SOCK_DGRAM | libc::SOCK_CLOEXEC, 0)
        })?;
        let mut iface: libc::ifreq = unsafe { std::mem::zeroed() };
        iface.ifr_name[0] = b'l' as _;
        iface.ifr_name[1] = b'o' as _;
        cvt(unsafe { libc::ioctl(socket.as_raw_fd(), libc::SIOCGIFFLAGS, &mut iface) })?;
        unsafe {
            iface.ifr_ifru.ifru_flags |= libc::IFF_UP as libc::c_short;
        }
        cvt(unsafe { libc::ioctl(socket.as_raw_fd(), libc::SIOCSIFFLAGS, &iface) })?;
        drop(socket);
        let process = Command::new(
            env::args()
                .nth(6)
                .ok_or_else(|| io::Error::other("missing Docker daemon"))?,
        )
        .args(env::args().skip(7))
        .spawn()?;
        return wait(process.id() as i32);
    }
    drop(engine_user);
    writeln!(fs::File::from(ready), "{child}")?;
    let result = supervise(child, &storage, keep);
    drop(lock);
    result
}
