//! Common inherited-socket startup, before any runtime worker or subprocess.
use crate::Result;
use goblins_runtime::{channel::Channel, scope, supervisor};
use goblins_unix as unix;
use std::os::{fd::FromRawFd, unix::net::UnixStream};

/// Called directly by the minimal executable; fd 3 is its only capability.
pub fn main() -> Result<()> {
    // Socketpair credentials name the creating daemon even if it died before
    // exec. Install the independent kernel backstop before checking that race.
    unix::cvt(unsafe { libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL) })?;
    let mut peer: libc::ucred = unsafe { std::mem::zeroed() };
    let mut size = std::mem::size_of_val(&peer) as libc::socklen_t;
    unix::cvt(unsafe {
        libc::getsockopt(
            supervisor::CONTROL_FD,
            libc::SOL_SOCKET,
            libc::SO_PEERCRED,
            (&mut peer as *mut libc::ucred).cast(),
            &mut size,
        )
    })?;
    if size as usize != std::mem::size_of_val(&peer) || unsafe { libc::getppid() } != peer.pid {
        return Err("control socket daemon exited before startup".into());
    }
    unix::cvt(unsafe { libc::syscall(libc::SYS_close_range, 4u32, u32::MAX, 0u32) as i32 })?;
    unix::cvt(unsafe { libc::syscall(libc::SYS_close_range, 0u32, 2u32, 0u32) as i32 })?;
    // Reserve stdio so later namespace/lease capabilities cannot occupy 0..2.
    for expected in 0..3 {
        let fd = unix::cvt(unsafe {
            libc::open(c"/dev/null".as_ptr(), libc::O_RDWR | libc::O_CLOEXEC)
        })?;
        if fd != expected {
            return Err("control descriptor allowlist setup failed".into());
        }
    }
    unix::cvt(unsafe { libc::fcntl(supervisor::CONTROL_FD, libc::F_SETFD, libc::FD_CLOEXEC) })?;
    let mut channel = Channel::new(unsafe { UnixStream::from_raw_fd(supervisor::CONTROL_FD) })?;
    let first = loop {
        if let Some(frame) = channel.receive()? {
            break frame;
        }
        if channel.closed() {
            return Ok(());
        }
        wait(&channel);
    };
    match first.value.get("method").and_then(|v| v.as_str()) {
        Some("Setup") => {
            if !first.fds.is_empty() {
                drop(first);
                channel.close();
                return Err("scope setup has descriptors".into());
            }
            let setup = scope::decode(first.value)?;
            crate::scope_process::start(channel, setup)
        }
        Some("SupervisorStart") => crate::supervisor_process::start(channel, first),
        _ => {
            drop(first);
            channel.close();
            Err("scope Setup or SupervisorStart must be first".into())
        }
    }
}

pub(crate) fn wait(channel: &Channel) {
    if let Some(fd) = channel.fd() {
        let mut poll = libc::pollfd {
            fd,
            events: libc::POLLIN
                | if channel.wants_write() {
                    libc::POLLOUT
                } else {
                    0
                },
            revents: 0,
        };
        unsafe {
            libc::poll(&mut poll, 1, 20);
        }
    }
}
