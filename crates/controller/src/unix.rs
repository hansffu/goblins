//! Small Linux descriptor helpers. Descriptors are close-on-exec by default.
use crate::Result;
use std::{
    ffi::CString,
    fs::{self, File},
    io, mem,
    os::{
        fd::{AsRawFd, FromRawFd, OwnedFd, RawFd},
        unix::{
            ffi::OsStrExt,
            fs::{MetadataExt, OpenOptionsExt, PermissionsExt},
        },
    },
    path::{Path, PathBuf},
};

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
pub fn disconnected(fd: RawFd) -> bool {
    let mut byte = 0u8;
    let n = unsafe {
        libc::recv(
            fd,
            (&mut byte as *mut u8).cast(),
            1,
            libc::MSG_PEEK | libc::MSG_DONTWAIT,
        )
    };
    n == 0
        || n < 0
            && !matches!(
                io::Error::last_os_error().kind(),
                io::ErrorKind::WouldBlock | io::ErrorKind::Interrupted
            )
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
fn address(path: &Path) -> Result<libc::sockaddr_un> {
    let mut addr: libc::sockaddr_un = unsafe { mem::zeroed() };
    addr.sun_family = libc::AF_UNIX as _;
    let bytes = path.as_os_str().as_bytes();
    if bytes.len() >= addr.sun_path.len() || bytes.contains(&0) {
        return Err("Unix socket path is too long or contains NUL".into());
    }
    for (dest, byte) in addr.sun_path.iter_mut().zip(bytes) {
        *dest = *byte as _;
    }
    Ok(addr)
}
pub fn seqpacket(path: &Path, listen: bool) -> Result<OwnedFd> {
    let socket = owned(unsafe {
        libc::socket(libc::AF_UNIX, libc::SOCK_SEQPACKET | libc::SOCK_CLOEXEC, 0)
    })?;
    let addr = address(path)?;
    let ptr = (&addr as *const libc::sockaddr_un).cast();
    let len = mem::size_of_val(&addr) as _;
    if listen {
        cvt(unsafe { libc::bind(socket.as_raw_fd(), ptr, len) })?;
        fs::set_permissions(path, fs::Permissions::from_mode(0o600))?;
        cvt(unsafe { libc::listen(socket.as_raw_fd(), 16) })?;
        nonblocking(socket.as_raw_fd())?;
    } else {
        cvt(unsafe { libc::connect(socket.as_raw_fd(), ptr, len) })?;
    }
    Ok(socket)
}
pub fn accept(fd: RawFd) -> io::Result<OwnedFd> {
    owned(unsafe {
        libc::accept4(
            fd,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            libc::SOCK_CLOEXEC | libc::SOCK_NONBLOCK,
        )
    })
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
pub fn send_terminal(socket: RawFd, terminal: RawFd) -> io::Result<()> {
    let bytes = b"{\"v\":1,\"status\":\"attached\"}";
    let mut iov = libc::iovec {
        iov_base: bytes.as_ptr() as *mut _,
        iov_len: bytes.len(),
    };
    // usize storage gives the ancillary header its required alignment.
    let mut control = [0usize; 8];
    let mut msg: libc::msghdr = unsafe { mem::zeroed() };
    msg.msg_iov = &mut iov;
    msg.msg_iovlen = 1;
    msg.msg_control = control.as_mut_ptr().cast();
    msg.msg_controllen = unsafe { libc::CMSG_SPACE(mem::size_of::<RawFd>() as _) } as _;
    unsafe {
        let header = libc::CMSG_FIRSTHDR(&msg);
        (*header).cmsg_level = libc::SOL_SOCKET;
        (*header).cmsg_type = libc::SCM_RIGHTS;
        (*header).cmsg_len = libc::CMSG_LEN(mem::size_of::<RawFd>() as _) as _;
        std::ptr::write_unaligned(libc::CMSG_DATA(header).cast::<RawFd>(), terminal);
        cvt(libc::sendmsg(socket, &msg, libc::MSG_NOSIGNAL | libc::MSG_DONTWAIT) as i32)?;
    }
    Ok(())
}
pub fn receive_terminal(socket: RawFd) -> Result<OwnedFd> {
    let mut bytes = [0; goblins_protocol::MAX_FRAME + 1];
    let mut control = [0usize; 32];
    let mut iov = libc::iovec {
        iov_base: bytes.as_mut_ptr().cast(),
        iov_len: bytes.len(),
    };
    let mut msg: libc::msghdr = unsafe { mem::zeroed() };
    msg.msg_iov = &mut iov;
    msg.msg_iovlen = 1;
    msg.msg_control = control.as_mut_ptr().cast();
    msg.msg_controllen = mem::size_of_val(&control);
    let n =
        cvt(unsafe { libc::recvmsg(socket, &mut msg, libc::MSG_CMSG_CLOEXEC) } as i32)? as usize;
    let mut fds = Vec::new();
    unsafe {
        let mut h = libc::CMSG_FIRSTHDR(&msg);
        while !h.is_null() {
            if (*h).cmsg_level == libc::SOL_SOCKET && (*h).cmsg_type == libc::SCM_RIGHTS {
                let count = ((*h).cmsg_len - libc::CMSG_LEN(0) as usize) / mem::size_of::<RawFd>();
                for i in 0..count {
                    fds.push(owned(std::ptr::read_unaligned(
                        libc::CMSG_DATA(h).cast::<RawFd>().add(i),
                    ))?);
                }
            }
            h = libc::CMSG_NXTHDR(&msg, h);
        }
    }
    if msg.msg_flags & (libc::MSG_TRUNC | libc::MSG_CTRUNC) != 0 || n > goblins_protocol::MAX_FRAME
    {
        return Err("truncated attachment response".into());
    }
    let reply: serde_json::Value = serde_json::from_slice(&bytes[..n])?;
    if reply["v"] != 1 || reply["status"] != "attached" {
        return Err(reply["message"]
            .as_str()
            .unwrap_or("attachment failed")
            .into());
    }
    if fds.len() != 1 {
        return Err("attachment did not provide exactly one terminal".into());
    }
    Ok(fds.pop().unwrap())
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
        .map_err(|_| "goblins serve is already running")?;
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
