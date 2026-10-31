//! The private transport between the supervisor and a sandbox's init.
//! One JSON message per SOCK_SEQPACKET datagram; wire data lives in the
//! shared protocol contract and is re-exported here for existing callers.
pub use goblins_protocol::init::{Command, Event, MAX_FDS, MAX_MESSAGE, forwardable};
use std::{
    io,
    os::fd::{FromRawFd, OwnedFd, RawFd},
};

/// Send one init datagram without blocking or raising SIGPIPE.
pub fn send(socket: RawFd, bytes: &[u8], fds: &[RawFd]) -> io::Result<()> {
    goblins_unix::ancillary::send(socket, bytes, fds, MAX_FDS)
}
/// Receive an init datagram with close-on-exec descriptors.
/// Truncation closes every received descriptor and returns an error.
pub fn receive(socket: RawFd, buffer: &mut [u8], flags: i32) -> io::Result<(usize, Vec<OwnedFd>)> {
    goblins_unix::ancillary::receive(socket, buffer, flags)
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
    use goblins_protocol::env_name;
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
