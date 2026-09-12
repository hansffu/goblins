//! Host terminal relay; receives only a PTY master from the controller.
use crate::{RESIZE, STOP};
use goblins_controller::{Result, unix};
use std::{
    io,
    os::fd::{AsRawFd, RawFd},
    path::PathBuf,
    sync::atomic::Ordering,
};

struct TerminalMode(libc::termios);
impl TerminalMode {
    fn raw() -> Result<Self> {
        let mut previous = unsafe { std::mem::zeroed() };
        unix::cvt(unsafe { libc::tcgetattr(0, &mut previous) })?;
        let mut raw = previous;
        unsafe {
            libc::cfmakeraw(&mut raw);
        }
        unix::cvt(unsafe { libc::tcsetattr(0, libc::TCSANOW, &raw) })?;
        Ok(Self(previous))
    }
}
impl Drop for TerminalMode {
    fn drop(&mut self) {
        unsafe {
            libc::tcsetattr(0, libc::TCSANOW, &self.0);
        }
    }
}
// File status flags are shared with the caller's terminal, so restore them on
// every normal/signal exit just as we restore termios.
struct NonblockingOutput(i32);
impl NonblockingOutput {
    fn new() -> Result<Self> {
        let flags = unix::cvt(unsafe { libc::fcntl(1, libc::F_GETFL) })?;
        unix::nonblocking(1)?;
        Ok(Self(flags))
    }
}
impl Drop for NonblockingOutput {
    fn drop(&mut self) {
        unsafe {
            libc::fcntl(1, libc::F_SETFL, self.0);
        }
    }
}
fn size(fd: RawFd) -> Result<libc::winsize> {
    let mut s = unsafe { std::mem::zeroed() };
    unix::cvt(unsafe { libc::ioctl(fd, libc::TIOCGWINSZ, &mut s) })?;
    Ok(s)
}
pub fn run(state: PathBuf, name: String, configuration: String) -> Result<()> {
    if unsafe { libc::isatty(0) != 1 || libc::isatty(1) != 1 } {
        return Err("goblins run requires an interactive terminal".into());
    }
    unix::private_directory(&state)?;
    let control = unix::seqpacket(&state.join("serve.sock"), false)
        .map_err(|e| format!("start goblins serve in another terminal first: {e}"))?;
    let dimensions = size(0)?;
    unix::send(
        control.as_raw_fd(),
        &serde_json::to_vec(
            &serde_json::json!({"v":1,"op":"run","name":name,"configuration":configuration,"rows":dimensions.ws_row.max(1),"cols":dimensions.ws_col.max(1)}),
        )?,
    )?;
    while !unix::readable(control.as_raw_fd(), 20)? {
        if STOP.load(Ordering::Relaxed) {
            return Ok(());
        }
    }
    let master = unix::receive_terminal(control.as_raw_fd())?;
    let _mode = TerminalMode::raw()?;
    // Bounded buffers and nonblocking writes keep disconnect/shutdown responsive
    // even when a terminal stops consuming output.
    unix::nonblocking(master.as_raw_fd())?;
    let _output_flags = NonblockingOutput::new()?;
    let mut to_terminal = Vec::<u8>::new();
    let mut to_shell = Vec::<u8>::new();
    while !STOP.load(Ordering::Relaxed) {
        if RESIZE.swap(false, Ordering::Relaxed) {
            let dimensions = size(0)?;
            unix::cvt(unsafe { libc::ioctl(master.as_raw_fd(), libc::TIOCSWINSZ, &dimensions) })?;
        }
        let mut fds = [
            libc::pollfd {
                fd: control.as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            },
            libc::pollfd {
                fd: 0,
                events: if to_shell.len() < 65536 {
                    libc::POLLIN
                } else {
                    0
                },
                revents: 0,
            },
            libc::pollfd {
                fd: master.as_raw_fd(),
                events: (if to_terminal.len() < 65536 {
                    libc::POLLIN
                } else {
                    0
                }) | (if !to_shell.is_empty() {
                    libc::POLLOUT
                } else {
                    0
                }),
                revents: 0,
            },
            libc::pollfd {
                fd: 1,
                events: if !to_terminal.is_empty() {
                    libc::POLLOUT
                } else {
                    0
                },
                revents: 0,
            },
        ];
        let n = unsafe { libc::poll(fds.as_mut_ptr(), fds.len() as _, 20) };
        if n < 0 {
            if io::Error::last_os_error().kind() == io::ErrorKind::Interrupted {
                continue;
            }
            return Err(io::Error::last_os_error().into());
        }
        if fds[0].revents != 0 {
            break;
        }
        for (index, buffer) in [(1, &mut to_shell), (2, &mut to_terminal)] {
            if fds[index].revents & (libc::POLLIN | libc::POLLHUP | libc::POLLERR) == 0 {
                continue;
            }
            let mut bytes = [0; 8192];
            let count =
                unsafe { libc::read(fds[index].fd, bytes.as_mut_ptr().cast(), bytes.len()) };
            if count == 0 {
                return Ok(());
            }
            if count < 0 {
                let e = io::Error::last_os_error();
                if e.kind() == io::ErrorKind::WouldBlock {
                    continue;
                }
                if index == 2 && e.raw_os_error() == Some(libc::EIO) {
                    return Ok(());
                }
                return Err(e.into());
            }
            buffer.extend_from_slice(&bytes[..count as usize]);
        }
        for (index, buffer) in [(2, &mut to_shell), (3, &mut to_terminal)] {
            if fds[index].revents & libc::POLLOUT == 0 || buffer.is_empty() {
                continue;
            }
            // Small tty writes after POLLOUT avoid monopolizing the relay.
            let count = unsafe {
                libc::write(
                    fds[index].fd,
                    buffer.as_ptr().cast(),
                    buffer.len().min(1024),
                )
            };
            if count > 0 {
                buffer.drain(..count as usize);
            } else if count < 0 && io::Error::last_os_error().kind() != io::ErrorKind::WouldBlock {
                return Err(io::Error::last_os_error().into());
            }
        }
    }
    Ok(())
}
