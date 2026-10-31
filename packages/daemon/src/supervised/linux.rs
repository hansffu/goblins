//! The daemon's Linux child backend. No caller handles pids or pidfds.
use goblins_unix as unix;
use std::{
    io,
    os::{
        fd::{AsRawFd, OwnedFd},
        unix::{net::UnixStream, process::CommandExt},
    },
    path::Path,
    process::{Command, Stdio},
};

pub(crate) struct Child {
    process: std::process::Child,
    exit: OwnedFd,
    reaped: bool,
    success: bool,
}
impl Child {
    pub fn start(executable: &Path) -> io::Result<(Self, UnixStream)> {
        // Test callers own the exclusive forking() guard through cleanup;
        // taking a nested guard here would deadlock those lifecycle tests.
        let (daemon, peer) = UnixStream::pair()?;
        let fd = peer.as_raw_fd();
        let mut command = Command::new(executable);
        command
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        unsafe {
            command.pre_exec(move || {
                let control = goblins_runtime::supervisor::CONTROL_FD;
                if fd != control {
                    unix::cvt(libc::dup2(fd, control))?;
                }
                unix::cvt(libc::fcntl(control, libc::F_SETFD, 0))?;
                unix::cvt(libc::syscall(libc::SYS_close_range, 4u32, u32::MAX, 4u32) as i32)?;
                Ok(())
            });
        }
        let process = command.spawn()?;
        drop(command);
        drop(peer);
        Self::adopt(process).map(|child| (child, daemon))
    }
    fn adopt(mut process: std::process::Child) -> io::Result<Self> {
        let fd = unsafe { libc::syscall(libc::SYS_pidfd_open, process.id(), 0) as i32 };
        match unix::owned(fd) {
            Ok(exit) => Ok(Self {
                process,
                exit,
                reaped: false,
                success: false,
            }),
            Err(error) => {
                let _ = process.kill();
                let _ = process.wait();
                Err(error)
            }
        }
    }
    /// Only exit notification is observed here; this never waits for a child.
    pub fn exited(&self) -> io::Result<bool> {
        if self.reaped {
            return Ok(true);
        }
        let mut fd = libc::pollfd {
            fd: self.exit.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        };
        let result = unsafe { libc::poll(&mut fd, 1, 0) };
        if result < 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(result != 0 && fd.revents & (libc::POLLIN | libc::POLLHUP) != 0)
    }
    pub fn kill(&self) -> io::Result<()> {
        let result = unsafe {
            libc::syscall(
                libc::SYS_pidfd_send_signal,
                self.exit.as_raw_fd(),
                libc::SIGKILL,
                std::ptr::null::<libc::siginfo_t>(),
                0,
            )
        };
        if result == 0 || io::Error::last_os_error().raw_os_error() == Some(libc::ESRCH) {
            Ok(())
        } else {
            Err(io::Error::last_os_error())
        }
    }
    /// Reap only after exit notification; there is no blocking wait in a tick.
    pub fn reap(&mut self) -> io::Result<bool> {
        if self.reaped {
            return Ok(true);
        }
        if !self.exited()? {
            return Ok(false);
        }
        if let Some(status) = self.process.try_wait()? {
            self.reaped = true;
            self.success = status.success();
        }
        Ok(self.reaped)
    }
    pub fn successful(&self) -> bool {
        self.reaped && self.success
    }
    #[cfg(test)]
    pub(crate) fn signal_for_test(&self, signal: i32) {
        assert_eq!(
            unsafe {
                libc::syscall(
                    libc::SYS_pidfd_send_signal,
                    self.exit.as_raw_fd(),
                    signal,
                    std::ptr::null::<libc::siginfo_t>(),
                    0,
                )
            },
            0
        );
    }
    #[cfg(test)]
    pub(crate) fn exit_status_for_test(&self) -> io::Result<Option<(i32, i32)>> {
        // Observe the actual kernel result without consuming it or changing
        // production state. The normal poll/reap path still owns reaping.
        let mut status: libc::siginfo_t = unsafe { std::mem::zeroed() };
        unix::cvt(unsafe {
            libc::waitid(
                libc::P_PIDFD,
                self.exit.as_raw_fd() as libc::id_t,
                &mut status,
                libc::WEXITED | libc::WNOHANG | libc::WNOWAIT,
            )
        })?;
        Ok((unsafe { status.si_pid() } != 0)
            .then(|| (status.si_code, unsafe { status.si_status() })))
    }
    #[cfg(test)]
    pub(crate) fn fixture(process: std::process::Child) -> Self {
        Self::adopt(process).unwrap()
    }
}
impl Drop for Child {
    fn drop(&mut self) {
        if !self.reaped {
            let _ = self.kill();
            // Drop is failure/shutdown only. Callers kill all children first;
            // full typed sender guards outlive this final reap.
            let _ = self.process.wait();
            self.reaped = true;
        }
    }
}
