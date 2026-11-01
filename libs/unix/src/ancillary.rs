//! Generic Unix socket descriptor I/O; protocol limits belong to callers.
use std::{
    io, mem,
    os::fd::{FromRawFd, OwnedFd, RawFd},
};

/// Send `bytes` with `fds` attached as SCM_RIGHTS. Never blocks or raises
/// SIGPIPE. Rejects more than the caller's limit or fixed ancillary capacity.
pub fn send(socket: RawFd, bytes: &[u8], fds: &[RawFd], max_fds: usize) -> io::Result<()> {
    if fds.len() > max_fds {
        return Err(io::Error::other("too many descriptors"));
    }
    let mut iov = libc::iovec {
        iov_base: bytes.as_ptr() as *mut _,
        iov_len: bytes.len(),
    };
    // usize storage gives the ancillary header its required alignment.
    let mut control = [0usize; 8];
    let ancillary_bytes = mem::size_of_val(fds);
    if ancillary_bytes > mem::size_of_val(&control) {
        return Err(io::Error::other("too many descriptors"));
    }
    // Bound before converting to CMSG_SPACE's narrower argument type.
    let size = unsafe { libc::CMSG_SPACE(ancillary_bytes as _) } as usize;
    if size > mem::size_of_val(&control) {
        return Err(io::Error::other("too many descriptors"));
    }
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        fs::File,
        io::Read,
        os::fd::AsRawFd,
        os::unix::net::{UnixDatagram, UnixStream},
    };

    #[test]
    fn caller_and_physical_limits_refuse_before_sending_without_taking_fds() {
        let _no_forks = crate::no_forks();
        let (sender, receiver) = UnixStream::pair().unwrap();
        let (reader, writer) = crate::pipe().unwrap();
        crate::nonblocking(reader.as_raw_fd()).unwrap();
        let fd = writer.as_raw_fd();
        let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
        let padded_overflow = mem::size_of::<[usize; 8]>() / mem::size_of::<RawFd>();
        for (count, limit) in [(4, 3), (padded_overflow, usize::MAX), (64, usize::MAX)] {
            assert_eq!(
                send(sender.as_raw_fd(), b"x", &vec![fd; count], limit)
                    .unwrap_err()
                    .to_string(),
                "too many descriptors"
            );
            assert!(unsafe { libc::fcntl(fd, libc::F_GETFD) } >= 0);
            assert_eq!(
                receive(receiver.as_raw_fd(), &mut [0; 8], libc::MSG_DONTWAIT)
                    .unwrap_err()
                    .kind(),
                io::ErrorKind::WouldBlock
            );
        }
        send(sender.as_raw_fd(), b"three", &[fd; 3], 3).unwrap();
        drop(writer);
        let mut bytes = [0; 8];
        let (n, fds) = receive(receiver.as_raw_fd(), &mut bytes, 0).unwrap();
        assert_eq!(&bytes[..n], b"three");
        assert_eq!(fds.len(), 3);
        for fd in &fds {
            assert_eq!(unsafe { libc::fcntl(fd.as_raw_fd(), libc::F_GETFL) }, flags);
            assert_ne!(
                unsafe { libc::fcntl(fd.as_raw_fd(), libc::F_GETFD) } & libc::FD_CLOEXEC,
                0
            );
        }
        drop(fds);
        assert_eq!(File::from(reader).read(&mut bytes).unwrap(), 0);
    }

    #[test]
    fn truncation_closes_received_capabilities() {
        let _no_forks = crate::no_forks();
        let (sender, receiver) = UnixDatagram::pair().unwrap();
        let (reader, writer) = crate::pipe().unwrap();
        crate::nonblocking(reader.as_raw_fd()).unwrap();
        send(sender.as_raw_fd(), b"too long", &[writer.as_raw_fd()], 3).unwrap();
        drop(writer);
        assert_eq!(
            receive(receiver.as_raw_fd(), &mut [0; 1], 0)
                .unwrap_err()
                .to_string(),
            "truncated message"
        );
        assert_eq!(File::from(reader).read(&mut [0]).unwrap(), 0);
    }

    #[test]
    fn ancillary_truncation_closes_every_installed_descriptor() {
        let _no_forks = crate::no_forks();
        let (sender, receiver) = UnixDatagram::pair().unwrap();
        let (reader, writer) = crate::pipe().unwrap();
        crate::nonblocking(reader.as_raw_fd()).unwrap();
        // A raw peer can exceed both our sender and receiver capacities.
        let fds = [writer.as_raw_fd(); 64];
        let size = unsafe { libc::CMSG_SPACE(mem::size_of_val(&fds) as _) } as usize;
        let mut control = vec![0usize; size.div_ceil(mem::size_of::<usize>())];
        let mut byte = [b'x'];
        let mut iov = libc::iovec {
            iov_base: byte.as_mut_ptr().cast(),
            iov_len: 1,
        };
        let mut msg: libc::msghdr = unsafe { mem::zeroed() };
        msg.msg_iov = &mut iov;
        msg.msg_iovlen = 1;
        msg.msg_control = control.as_mut_ptr().cast();
        msg.msg_controllen = size;
        unsafe {
            let h = libc::CMSG_FIRSTHDR(&msg);
            (*h).cmsg_level = libc::SOL_SOCKET;
            (*h).cmsg_type = libc::SCM_RIGHTS;
            (*h).cmsg_len = libc::CMSG_LEN(mem::size_of_val(&fds) as _) as _;
            std::ptr::copy_nonoverlapping(fds.as_ptr(), libc::CMSG_DATA(h).cast(), fds.len());
            assert_eq!(
                libc::sendmsg(sender.as_raw_fd(), &msg, libc::MSG_NOSIGNAL),
                1
            );
        }
        drop(writer);
        assert_eq!(
            receive(receiver.as_raw_fd(), &mut [0; 8], 0)
                .unwrap_err()
                .to_string(),
            "truncated message"
        );
        assert_eq!(
            File::from(reader).read(&mut [0]).unwrap(),
            0,
            "installed ancillary descriptors leaked"
        );
    }

    #[test]
    fn nonblocking_short_send_and_peer_close_preserve_socket_flags() {
        let _no_forks = crate::no_forks();
        let (sender, receiver) = UnixStream::pair().unwrap();
        let before = unsafe { libc::fcntl(sender.as_raw_fd(), libc::F_GETFL) };
        assert_eq!(before & libc::O_NONBLOCK, 0);
        let size = 4096i32;
        assert_eq!(
            unsafe {
                libc::setsockopt(
                    sender.as_raw_fd(),
                    libc::SOL_SOCKET,
                    libc::SO_SNDBUF,
                    (&size as *const i32).cast(),
                    mem::size_of_val(&size) as _,
                )
            },
            0
        );
        assert_eq!(
            send(sender.as_raw_fd(), &vec![b'x'; 1024 * 1024], &[], 3)
                .unwrap_err()
                .to_string(),
            "short message"
        );
        assert_eq!(
            send(sender.as_raw_fd(), b"x", &[], 3).unwrap_err().kind(),
            io::ErrorKind::WouldBlock
        );
        assert_eq!(
            unsafe { libc::fcntl(sender.as_raw_fd(), libc::F_GETFL) },
            before
        );
        drop(receiver);
        assert_eq!(
            send(sender.as_raw_fd(), b"x", &[], 3).unwrap_err().kind(),
            io::ErrorKind::BrokenPipe
        );
    }
}
