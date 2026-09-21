//! Trusted bootstrap, invoked only after Bubblewrap removed host paths and FDs.
//! It maps a child user namespace before exec (unlike RootlessKit's re-exec),
//! preserving no_new_privs. The child clones the mount namespace to lock every
//! inherited host bind, then creates its own network, IPC and PID namespaces.
use super::{c, cvt, pipe};
use std::{
    env, fs,
    io::{self, Read, Write},
    os::fd::AsRawFd,
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

fn supervise(child: i32, storage: &Path) -> io::Result<i32> {
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
    fs::remove_dir_all(storage)
        .map_err(|error| io::Error::other(format!("Docker disk cleanup failed: {error}")))?;
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
    if name.len() != 14
        || !name.starts_with("session-")
        || !name[8..].bytes().all(|b| b.is_ascii_alphanumeric())
    {
        return Err(io::Error::other("invalid Docker storage name"));
    }
    let storage = Path::new("/run/goblins/docker-storage-parent").join(name);
    use std::os::unix::fs::MetadataExt;
    let expected = fs::metadata("/run/docker/data")?;
    let actual = fs::symlink_metadata(&storage)?;
    if !actual.is_dir() || actual.ino() != expected.ino() || actual.dev() != expected.dev() {
        return Err(io::Error::other("Docker storage identity mismatch"));
    }
    cvt(unsafe { libc::prctl(libc::PR_SET_CHILD_SUBREAPER, 1) })?;
    let (ready_r, ready_w) = pipe()?;
    let (go_r, go_w) = pipe()?;
    let child = cvt(unsafe { libc::fork() })?;
    if child == 0 {
        drop(ready_r);
        drop(go_w);
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
        cvt(unsafe { libc::unshare(libc::CLONE_NEWUSER) })?;
        fs::File::from(ready_w).write_all(b"x")?;
        fs::File::from(go_r).read_exact(&mut [0])?;
        // The mount namespace becomes less privileged here: existing mounts
        // and their readonly/nosuid/nodev flags are locked by the kernel.
        cvt(unsafe {
            libc::unshare(
                libc::CLONE_NEWNS
                    | libc::CLONE_NEWNET
                    | libc::CLONE_NEWIPC
                    | libc::CLONE_NEWUTS
                    | libc::CLONE_NEWPID,
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
                .nth(3)
                .ok_or_else(|| io::Error::other("missing Docker daemon"))?,
        )
        .args(env::args().skip(4))
        .spawn()?;
        return wait(process.id() as i32);
    }
    drop(ready_w);
    drop(go_r);
    let setup = (|| -> io::Result<()> {
        fs::File::from(ready_r).read_exact(&mut [0])?;
        // We have CAP_SETGID in the mapped parent namespace, so gid_map can
        // be installed without disabling setgroups. A "deny" here is inherited
        // irreversibly by containers and breaks entrypoints such as gosu/su.
        // The mapping still limits groups to the host user's authorized IDs.
        for kind in ["uid", "gid"] {
            // Preserve both extents; they map to noncontiguous host IDs.
            fs::write(format!("/proc/{child}/{kind}_map"), "0 0 1\n1 1 65536\n")?;
        }
        fs::write("/run/docker/namespace-pid", child.to_string())?;
        fs::File::from(go_w).write_all(b"x")?;
        Ok(())
    })();
    if setup.is_err() {
        unsafe {
            libc::kill(child, libc::SIGKILL);
        }
    }
    let result = supervise(child, &storage);
    setup?;
    result
}
