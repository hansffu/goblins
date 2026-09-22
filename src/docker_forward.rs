//! TCP relay across two sandbox-owned networks. Only trusted FD arguments cross
//! the host boundary; container traffic is copied as bytes, never interpreted.
use super::*;
use std::{
    net::{Ipv4Addr, Shutdown, TcpListener, TcpStream},
    os::unix::net::UnixStream,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
};

fn enter(user: &OwnedFd, net: &OwnedFd) -> io::Result<()> {
    let owner = fd(unsafe { libc::ioctl(net.as_raw_fd(), NS_GET_USERNS) })?;
    if inode(&owner)? != inode(user)? {
        return Err(io::Error::other("forward namespace owner mismatch"));
    }
    cvt(unsafe { libc::setns(user.as_raw_fd(), libc::CLONE_NEWUSER) })?;
    cvt(unsafe { libc::setns(net.as_raw_fd(), libc::CLONE_NEWNET) })?;
    Ok(())
}
fn transfer(socket: RawFd, passing: Option<RawFd>) -> io::Result<Option<OwnedFd>> {
    let mut byte = [0u8];
    let mut iov = libc::iovec {
        iov_base: byte.as_mut_ptr().cast(),
        iov_len: 1,
    };
    let mut control = [0usize; 8];
    let mut msg: libc::msghdr = unsafe { std::mem::zeroed() };
    msg.msg_iov = &mut iov;
    msg.msg_iovlen = 1;
    msg.msg_control = control.as_mut_ptr().cast();
    msg.msg_controllen = unsafe { libc::CMSG_SPACE(size_of::<RawFd>() as _) } as _;
    if let Some(passing) = passing {
        unsafe {
            let header = libc::CMSG_FIRSTHDR(&msg);
            (*header).cmsg_level = libc::SOL_SOCKET;
            (*header).cmsg_type = libc::SCM_RIGHTS;
            (*header).cmsg_len = libc::CMSG_LEN(size_of::<RawFd>() as _) as _;
            std::ptr::write_unaligned(libc::CMSG_DATA(header).cast::<RawFd>(), passing);
            cvt(libc::sendmsg(socket, &msg, libc::MSG_NOSIGNAL) as i32)?;
        }
        Ok(None)
    } else {
        let n = cvt(unsafe { libc::recvmsg(socket, &mut msg, libc::MSG_CMSG_CLOEXEC) as i32 })?;
        if n == 0 {
            return Err(io::Error::other("relay listener exited"));
        }
        unsafe {
            let header = libc::CMSG_FIRSTHDR(&msg);
            if header.is_null()
                || (*header).cmsg_level != libc::SOL_SOCKET
                || (*header).cmsg_type != libc::SCM_RIGHTS
                || (*header).cmsg_len != libc::CMSG_LEN(size_of::<RawFd>() as _) as usize
            {
                return Err(io::Error::other("invalid relay descriptor"));
            }
            Ok(Some(OwnedFd::from_raw_fd(std::ptr::read_unaligned(
                libc::CMSG_DATA(header).cast::<RawFd>(),
            ))))
        }
    }
}
pub fn run() -> io::Result<i32> {
    let args: Vec<_> = env::args().collect();
    if args.len() != 8 {
        return Err(io::Error::other("invalid forwarding arguments"));
    }
    let mut handles = Vec::new();
    for arg in &args[2..6] {
        let n: i32 = arg.parse().map_err(io::Error::other)?;
        if n < 3 || handles.iter().any(|f: &OwnedFd| f.as_raw_fd() == n) {
            return Err(io::Error::other("invalid relay descriptor"));
        }
        handles.push(unsafe { OwnedFd::from_raw_fd(n) });
    }
    let port: u16 = args[6].parse().map_err(io::Error::other)?;
    let address: Ipv4Addr = args[7].parse().map_err(io::Error::other)?;
    if port == 0 {
        return Err(io::Error::other("invalid relay port"));
    }
    let (listener_side, connector_side) = UnixStream::pair()?;
    let parent = unsafe { libc::getpid() };
    let child = cvt(unsafe { libc::fork() })?;
    if child == 0 {
        cvt(unsafe { libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL) })?;
        if unsafe { libc::getppid() } != parent {
            return Err(io::Error::other("relay exited"));
        }
        drop(listener_side);
        enter(&handles[2], &handles[3])?;
        drop(handles);
        let count = Arc::new(AtomicUsize::new(0));
        loop {
            let client = TcpStream::from(transfer(connector_side.as_raw_fd(), None)?.unwrap());
            if count.load(Ordering::Relaxed) >= 64 {
                continue;
            }
            count.fetch_add(1, Ordering::Relaxed);
            let count = count.clone();
            thread::spawn(move || {
                let relay = || -> io::Result<()> {
                    let mut peer = TcpStream::connect_timeout(
                        &(address, port).into(),
                        Duration::from_secs(5),
                    )?;
                    let mut upstream = peer.try_clone()?;
                    let mut downstream = client.try_clone()?;
                    let mut client = client;
                    let copy = thread::spawn(move || {
                        let _ = io::copy(&mut client, &mut upstream);
                        let _ = upstream.shutdown(Shutdown::Write);
                    });
                    let _ = io::copy(&mut peer, &mut downstream);
                    let _ = downstream.shutdown(Shutdown::Write);
                    let _ = copy.join();
                    Ok(())
                };
                let _ = relay();
                count.fetch_sub(1, Ordering::Relaxed);
            });
        }
    }
    drop(connector_side);
    let result = (|| -> io::Result<()> {
        enter(&handles[0], &handles[1])?;
        drop(handles);
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, port))?;
        println!("READY");
        io::stdout().flush()?;
        for client in listener.incoming() {
            let client = client?;
            transfer(listener_side.as_raw_fd(), Some(client.as_raw_fd()))?;
        }
        Ok(())
    })();
    unsafe {
        libc::kill(child, libc::SIGKILL);
        libc::waitpid(child, std::ptr::null_mut(), 0);
    }
    result?;
    Ok(0)
}
