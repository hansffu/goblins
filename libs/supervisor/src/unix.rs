//! Small Linux descriptor helpers. Descriptors are close-on-exec by default.
use crate::Result;
use goblins_unix::{cvt, nonblocking, owned};
use std::{
    fs, io, mem,
    os::{
        fd::{AsRawFd, OwnedFd, RawFd},
        unix::{ffi::OsStrExt, fs::PermissionsExt},
    },
    path::Path,
};
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
