//! The private channel between the daemon and a sandbox's `goblins-init`.
//! One JSON message per SOCK_SEQPACKET datagram: the kernel keeps message
//! boundaries and attaches SCM_RIGHTS descriptors to exactly one message.
use serde::{Deserialize, Serialize};
use std::{
    io, mem,
    os::fd::{FromRawFd, OwnedFd, RawFd},
};

pub const MAX_MESSAGE: usize = 64 * 1024;
/// A spawn carries one PTY slave or stdin, stdout and stderr.
pub const MAX_FDS: usize = 3;
/// Bounds on what a spawn may carry, checked by both ends.
pub const MAX_ARGS: usize = 1024;
pub const MAX_ENV: usize = 256;

/// Daemon → init. Exec IDs are chosen by the daemon and never reused.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "method", content = "params", rename_all = "kebab-case")]
pub enum Command {
    /// Run `argv` (the sandbox's `$SHELL` when absent) in its own session and
    /// process group. Descriptors: the PTY slave with `tty`, else stdin,
    /// stdout and stderr.
    Spawn {
        id: u64,
        argv: Option<Vec<String>>,
        cwd: Option<String>,
        env: Vec<(String, String)>,
        tty: bool,
    },
    /// Forward a catchable signal to the exec's process group.
    Signal { id: u64, signal: i32 },
    /// SIGHUP and SIGTERM to the exec's process group, then SIGKILL after a
    /// grace period.
    Cancel { id: u64 },
    /// Cancel every exec. The agent is not affected.
    KillAll {},
}

/// Init → daemon.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "method", content = "params", rename_all = "kebab-case")]
pub enum Event {
    Spawned {
        id: u64,
        pid: i32,
    },
    /// The command could not start. `code` is the exit code a shell would
    /// report: 127 for a missing command, 126 otherwise.
    Failed {
        id: u64,
        code: i32,
        message: String,
    },
    Exited {
        id: u64,
        code: Option<i32>,
        signal: Option<i32>,
    },
}

/// Signals a host client may forward to an exec.
pub fn forwardable(signal: i32) -> bool {
    [libc::SIGHUP, libc::SIGINT, libc::SIGTERM].contains(&signal)
}

/// Environment variable names an exec may set.
pub fn env_name(name: &str) -> bool {
    let mut bytes = name.bytes();
    (1..=256).contains(&name.len())
        && bytes
            .next()
            .is_some_and(|b| b.is_ascii_alphabetic() || b == b'_')
        && bytes.all(|b| b.is_ascii_alphanumeric() || b == b'_')
}

/// Send `bytes` with `fds` attached as SCM_RIGHTS. Never blocks or raises
/// SIGPIPE.
pub fn send(socket: RawFd, bytes: &[u8], fds: &[RawFd]) -> io::Result<()> {
    if fds.len() > MAX_FDS {
        return Err(io::Error::other("too many descriptors"));
    }
    let mut iov = libc::iovec {
        iov_base: bytes.as_ptr() as *mut _,
        iov_len: bytes.len(),
    };
    // usize storage gives the ancillary header its required alignment.
    let mut control = [0usize; 8];
    let mut msg: libc::msghdr = unsafe { mem::zeroed() };
    msg.msg_iov = &mut iov;
    msg.msg_iovlen = 1;
    if !fds.is_empty() {
        let size = mem::size_of_val(fds);
        msg.msg_control = control.as_mut_ptr().cast();
        msg.msg_controllen = unsafe { libc::CMSG_SPACE(size as _) } as _;
        unsafe {
            let header = libc::CMSG_FIRSTHDR(&msg);
            (*header).cmsg_level = libc::SOL_SOCKET;
            (*header).cmsg_type = libc::SCM_RIGHTS;
            (*header).cmsg_len = libc::CMSG_LEN(size as _) as _;
            for (i, fd) in fds.iter().enumerate() {
                std::ptr::write_unaligned(libc::CMSG_DATA(header).cast::<RawFd>().add(i), *fd);
            }
        }
    }
    let n = unsafe { libc::sendmsg(socket, &msg, libc::MSG_NOSIGNAL | libc::MSG_DONTWAIT) };
    if n < 0 {
        return Err(io::Error::last_os_error());
    }
    if n as usize != bytes.len() {
        return Err(io::Error::other("short message"));
    }
    Ok(())
}

/// Receive into `buffer`, returning the length and any descriptors, which are
/// close-on-exec. A length of 0 on a seqpacket socket means end of stream.
/// Truncated data or descriptors are an error; the descriptors that did
/// arrive are closed.
pub fn receive(socket: RawFd, buffer: &mut [u8], flags: i32) -> io::Result<(usize, Vec<OwnedFd>)> {
    let mut control = [0usize; 16];
    let mut iov = libc::iovec {
        iov_base: buffer.as_mut_ptr().cast(),
        iov_len: buffer.len(),
    };
    let mut msg: libc::msghdr = unsafe { mem::zeroed() };
    msg.msg_iov = &mut iov;
    msg.msg_iovlen = 1;
    msg.msg_control = control.as_mut_ptr().cast();
    msg.msg_controllen = mem::size_of_val(&control);
    let n = unsafe { libc::recvmsg(socket, &mut msg, flags | libc::MSG_CMSG_CLOEXEC) };
    if n < 0 {
        return Err(io::Error::last_os_error());
    }
    let mut fds = Vec::new();
    unsafe {
        let mut h = libc::CMSG_FIRSTHDR(&msg);
        while !h.is_null() {
            if (*h).cmsg_level == libc::SOL_SOCKET && (*h).cmsg_type == libc::SCM_RIGHTS {
                let count =
                    ((*h).cmsg_len as usize - libc::CMSG_LEN(0) as usize) / mem::size_of::<RawFd>();
                for i in 0..count {
                    let fd = std::ptr::read_unaligned(libc::CMSG_DATA(h).cast::<RawFd>().add(i));
                    fds.push(OwnedFd::from_raw_fd(fd));
                }
            }
            h = libc::CMSG_NXTHDR(&msg, h);
        }
    }
    if msg.msg_flags & (libc::MSG_TRUNC | libc::MSG_CTRUNC) != 0 {
        return Err(io::Error::other("truncated message"));
    }
    Ok((n as usize, fds))
}

/// A connected, close-on-exec SOCK_SEQPACKET pair.
pub fn pair() -> io::Result<(OwnedFd, OwnedFd)> {
    let mut fds = [0; 2];
    if unsafe {
        libc::socketpair(
            libc::AF_UNIX,
            libc::SOCK_SEQPACKET | libc::SOCK_CLOEXEC,
            0,
            fds.as_mut_ptr(),
        )
    } < 0
    {
        return Err(io::Error::last_os_error());
    }
    Ok(unsafe { (OwnedFd::from_raw_fd(fds[0]), OwnedFd::from_raw_fd(fds[1])) })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::fd::AsRawFd;
    #[test]
    fn messages_carry_descriptors_and_names() {
        let (a, b) = pair().unwrap();
        let spawn = Command::Spawn {
            id: 7,
            argv: Some(vec!["sh".into(), "-c".into(), "true".into()]),
            cwd: None,
            env: vec![("TERM".into(), "dumb".into())],
            tty: false,
        };
        let text = serde_json::to_vec(&spawn).unwrap();
        assert!(String::from_utf8_lossy(&text).contains(r#""method":"spawn""#));
        let (r, w) = (pair().unwrap().0, pair().unwrap().1);
        send(a.as_raw_fd(), &text, &[r.as_raw_fd(), w.as_raw_fd()]).unwrap();
        let mut buffer = vec![0; MAX_MESSAGE];
        let (n, fds) = receive(b.as_raw_fd(), &mut buffer, 0).unwrap();
        assert_eq!(fds.len(), 2);
        assert_eq!(
            serde_json::from_slice::<Command>(&buffer[..n]).unwrap(),
            spawn
        );
        let flags = unsafe { libc::fcntl(fds[0].as_raw_fd(), libc::F_GETFD) };
        assert_ne!(flags & libc::FD_CLOEXEC, 0);
        drop(a);
        assert_eq!(receive(b.as_raw_fd(), &mut buffer, 0).unwrap().0, 0);
        assert!(env_name("_X1") && !env_name("1X") && !env_name("A=B") && !env_name(""));
        assert!(forwardable(libc::SIGINT) && !forwardable(libc::SIGKILL));
    }
}
