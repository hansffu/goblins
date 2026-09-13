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
pub fn run(
    state: PathBuf,
    name: String,
    configuration: String,
    agent_name: Option<String>,
) -> Result<i32> {
    use goblins_controller::host::Client;
    use serde_json::json;
    use std::{
        io::{Read, Write},
        os::unix::net::UnixStream,
    };
    if unsafe { libc::isatty(0) != 1 } {
        return Err("goblins run requires terminal input".into());
    }
    let mut control = Client::connect(&state)?;
    let dimensions = size(0)?;
    let mut random = [0; 16];
    std::fs::File::open("/dev/urandom")?.read_exact(&mut random)?;
    let key: String = random.iter().map(|b| format!("{b:02x}")).collect();
    let launch=control.call("sessions.start",json!({"key":key,"configuration":configuration,"name":name,"agent_name":agent_name,"cwd":std::env::current_dir()?,"rows":dimensions.ws_row.max(1),"cols":dimensions.ws_col.max(1)}))?;
    let session = launch["session"].as_str().ok_or("missing session ID")?;
    let mut terminal =
        UnixStream::connect(launch["terminal"].as_str().ok_or("missing terminal path")?)?;
    terminal.set_nonblocking(true)?;
    let _mode = TerminalMode::raw()?;
    let _output_flags = NonblockingOutput::new()?;
    let mut to_terminal = Vec::<u8>::new();
    let mut to_shell = Vec::<u8>::new();
    let mut eof = false;
    loop {
        if STOP.load(Ordering::Relaxed) {
            return Err("terminal delivery interrupted; session remains daemon-owned".into());
        }
        if RESIZE.swap(false, Ordering::Relaxed) && !eof {
            let d = size(0)?;
            // Startup may not have a PTY yet; retry the first dimensions later.
            if control
                .call(
                    "sessions.resize",
                    json!({"session":session,"rows":d.ws_row.max(1),"cols":d.ws_col.max(1)}),
                )
                .is_err()
            {
                RESIZE.store(true, Ordering::Relaxed);
            }
        }
        if eof && to_terminal.is_empty() {
            break;
        }
        let mut fds = [
            libc::pollfd {
                fd: 0,
                events: if !eof && to_shell.len() < 65536 {
                    libc::POLLIN
                } else {
                    0
                },
                revents: 0,
            },
            libc::pollfd {
                fd: if eof { -1 } else { terminal.as_raw_fd() },
                events: if to_terminal.len() < 65536 {
                    libc::POLLIN
                } else {
                    0
                } | if !to_shell.is_empty() {
                    libc::POLLOUT
                } else {
                    0
                },
                revents: 0,
            },
            libc::pollfd {
                fd: 1,
                events: if to_terminal.is_empty() {
                    0
                } else {
                    libc::POLLOUT
                },
                revents: 0,
            },
        ];
        if unsafe { libc::poll(fds.as_mut_ptr(), 3, 20) } < 0 {
            if io::Error::last_os_error().kind() == io::ErrorKind::Interrupted {
                continue;
            }
            return Err(io::Error::last_os_error().into());
        }
        if fds[0].revents & libc::POLLIN != 0 {
            let mut bytes = [0; 8192];
            let room = bytes.len().min(65536 - to_shell.len());
            let n = unsafe { libc::read(0, bytes.as_mut_ptr().cast(), room) };
            if n <= 0 {
                return Err("terminal input disconnected; session remains daemon-owned".into());
            }
            to_shell.extend_from_slice(&bytes[..n as usize]);
        }
        if !eof
            && to_terminal.len() < 65536
            && fds[1].revents & (libc::POLLIN | libc::POLLHUP | libc::POLLERR) != 0
        {
            let mut bytes = [0; 8192];
            let room = bytes.len().min(65536 - to_terminal.len());
            match terminal.read(&mut bytes[..room]) {
                Ok(0) => {
                    eof = true;
                    to_shell.clear();
                }
                Ok(n) => to_terminal.extend_from_slice(&bytes[..n]),
                Err(e)
                    if matches!(
                        e.kind(),
                        io::ErrorKind::WouldBlock | io::ErrorKind::Interrupted
                    ) => {}
                Err(e) => return Err(e.into()),
            }
        }
        if !eof && !to_shell.is_empty() && fds[1].revents & libc::POLLOUT != 0 {
            match terminal.write(&to_shell) {
                Ok(n) => {
                    to_shell.drain(..n);
                }
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => (),
                Err(e) => return Err(e.into()),
            }
        }
        if !to_terminal.is_empty()
            && fds[2].revents & (libc::POLLOUT | libc::POLLERR | libc::POLLHUP) != 0
        {
            let n =
                unsafe { libc::write(1, to_terminal.as_ptr().cast(), to_terminal.len().min(1024)) };
            if n > 0 {
                to_terminal.drain(..n as usize);
            } else if n == 0
                || !matches!(
                    io::Error::last_os_error().kind(),
                    io::ErrorKind::WouldBlock | io::ErrorKind::Interrupted
                )
            {
                return Err("terminal output incomplete".into());
            }
        }
    }
    // Stream EOF precedes local drain above. A control disconnect is never a
    // substitute for EOF, and a daemon crash must not look like normal exit.
    loop {
        let record = control.call("sessions.get", json!({"session":session}))?;
        if ["stopped", "failed"].contains(&record["state"].as_str().unwrap_or("")) {
            if record["state"] == "failed" {
                return Err(record["detail"].as_str().unwrap_or("launch failed").into());
            }
            if record["terminal_complete"] != true || record["terminal_interrupted"] == true {
                return Err("terminal output incomplete".into());
            }
            return record["exit_code"]
                .as_i64()
                .map(|c| c as i32)
                .ok_or_else(|| {
                    record["detail"]
                        .as_str()
                        .unwrap_or("payload exit status unknown")
                        .into()
                });
        }
        if STOP.load(Ordering::Relaxed) {
            return Err("terminal completion interrupted".into());
        }
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
}
