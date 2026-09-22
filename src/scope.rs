//! Trusted namespace keeper, independent of Docker and container storage.
use super::{NS_GET_PARENT, NS_GET_USERNS, cvt, fd, inode, pipe};
use std::{
    env, fs,
    io::{self, Read, Write},
    os::{
        fd::{AsRawFd, FromRawFd, OwnedFd},
        unix::process::CommandExt,
    },
    process::Command,
};

pub fn enter() -> io::Result<i32> {
    let args: Vec<_> = env::args().collect();
    if args.len() < 5 {
        return Err(io::Error::other("missing scope entry arguments"));
    }
    let user_fd: i32 = args[2].parse().map_err(io::Error::other)?;
    let net_fd: i32 = args[3].parse().map_err(io::Error::other)?;
    if user_fd < 3 || net_fd < 3 || user_fd == net_fd {
        return Err(io::Error::other("invalid scope descriptors"));
    }
    let user = unsafe { OwnedFd::from_raw_fd(user_fd) };
    let net = unsafe { OwnedFd::from_raw_fd(net_fd) };
    let owner = fd(unsafe { libc::ioctl(net.as_raw_fd(), NS_GET_USERNS) })?;
    let parent = fd(unsafe { libc::ioctl(owner.as_raw_fd(), NS_GET_PARENT) })?;
    if inode(&parent)? != inode(&user)? {
        return Err(io::Error::other("scope namespace ancestry mismatch"));
    }
    cvt(unsafe { libc::setns(user.as_raw_fd(), libc::CLONE_NEWUSER) })?;
    cvt(unsafe { libc::setns(net.as_raw_fd(), libc::CLONE_NEWNET) })?;
    drop((user, net, owner, parent));
    Err(Command::new(&args[4]).args(&args[5..]).exec())
}

pub fn run() -> io::Result<i32> {
    let (ready_r, ready_w) = pipe()?;
    let (go_r, go_w) = pipe()?;
    let child = cvt(unsafe { libc::fork() })?;
    if child == 0 {
        drop(ready_r);
        drop(go_w);
        cvt(unsafe { libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL) })?;
        cvt(unsafe { libc::unshare(libc::CLONE_NEWUSER) })?;
        fs::File::from(ready_w).write_all(b"x")?;
        fs::File::from(go_r).read_exact(&mut [0])?;
        cvt(unsafe { libc::unshare(libc::CLONE_NEWNET) })?;
        let socket =
            fd(unsafe { libc::socket(libc::AF_INET, libc::SOCK_DGRAM | libc::SOCK_CLOEXEC, 0) })?;
        let mut iface: libc::ifreq = unsafe { std::mem::zeroed() };
        iface.ifr_name[0] = b'l' as _;
        iface.ifr_name[1] = b'o' as _;
        cvt(unsafe { libc::ioctl(socket.as_raw_fd(), libc::SIOCGIFFLAGS, &mut iface) })?;
        unsafe {
            iface.ifr_ifru.ifru_flags |= libc::IFF_UP as libc::c_short;
        }
        cvt(unsafe { libc::ioctl(socket.as_raw_fd(), libc::SIOCSIFFLAGS, &iface) })?;
        drop(socket);
        fs::write(
            "/run/goblins-scope/ready",
            unsafe { libc::getpid() }.to_string(),
        )?;
        loop {
            unsafe {
                libc::pause();
            }
        }
    }
    drop(ready_w);
    drop(go_r);
    let result = (|| -> io::Result<()> {
        fs::File::from(ready_r).read_exact(&mut [0])?;
        // The mapped parent has CAP_SETGID: do not disable setgroups here.
        // That irreversible restriction would break container gosu/su entrypoints.
        // Preserve both extents, which map to noncontiguous authorized host IDs.
        for kind in ["uid", "gid"] {
            fs::write(format!("/proc/{child}/{kind}_map"), "0 0 1\n1 1 65536\n")?;
        }
        fs::File::from(go_w).write_all(b"x")?;
        // Only the host holds the write end; EOF tears down this private scope.
        let _ = io::stdin().read(&mut [0])?;
        Ok(())
    })();
    unsafe {
        libc::kill(child, libc::SIGKILL);
        libc::waitpid(child, std::ptr::null_mut(), 0);
    }
    result?;
    Ok(0)
}
