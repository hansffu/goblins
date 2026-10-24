//! Bounded raw terminal relay shared by host and sandbox clients.
//! Control calls and the authenticated stream are supplied by the caller.
pub mod palette;
use serde_json::{Value, json};
use std::{
    io,
    os::{
        fd::{AsRawFd, RawFd},
        unix::net::UnixStream,
    },
    sync::atomic::{AtomicBool, Ordering},
};
pub type Result<T> = std::result::Result<T, Box<dyn std::error::Error + Send + Sync>>;
fn cvt(n: i32) -> io::Result<i32> {
    if n < 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(n)
    }
}
struct TerminalMode(libc::termios);
impl TerminalMode {
    fn raw() -> Result<Self> {
        Self::configure(false)
    }
    fn probe() -> Result<Self> {
        Self::configure(true)
    }
    fn configure(probe: bool) -> Result<Self> {
        let mut previous = unsafe { std::mem::zeroed() };
        cvt(unsafe { libc::tcgetattr(0, &mut previous) })?;
        let mut raw = previous;
        unsafe {
            libc::cfmakeraw(&mut raw);
        }
        if probe {
            // Keyboard interrupts and normal diagnostic line endings remain
            // usable while probing. Only the relay needs fully raw mode.
            raw.c_lflag |= previous.c_lflag & libc::ISIG;
            raw.c_oflag = previous.c_oflag;
        }
        cvt(unsafe { libc::tcsetattr(0, libc::TCSANOW, &raw) })?;
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
        let flags = cvt(unsafe { libc::fcntl(1, libc::F_GETFL) })?;
        cvt(unsafe { libc::fcntl(1, libc::F_SETFL, flags | libc::O_NONBLOCK) })?;
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
pub fn size(fd: RawFd) -> Result<libc::winsize> {
    let mut s = unsafe { std::mem::zeroed() };
    cvt(unsafe { libc::ioctl(fd, libc::TIOCGWINSZ, &mut s) })?;
    Ok(s)
}
/// Probe only for an immediately attached launch, preserving typeahead.
/// Restore termios before launch; detached launches must not read shell input.
pub struct PaletteProbe {
    pub palette: palette::Palette,
    pub input: Vec<u8>,
}
impl PaletteProbe {
    pub fn new() -> Result<Self> {
        use std::{
            io::Write,
            time::{Duration, Instant},
        };
        let _mode = TerminalMode::probe()?;
        std::io::stdout().write_all(b"\x1b]10;?\x1b\\\x1b]11;?\x1b\\\x1b[c")?;
        std::io::stdout().flush()?;
        let until = Instant::now() + Duration::from_secs(1);
        let mut bytes = Vec::new();
        while Instant::now() < until && bytes.len() < 65536 {
            let mut fd = libc::pollfd {
                fd: 0,
                events: libc::POLLIN,
                revents: 0,
            };
            let timeout = until
                .saturating_duration_since(Instant::now())
                .as_millis()
                .min(1000) as i32;
            if unsafe { libc::poll(&mut fd, 1, timeout) } <= 0 {
                break;
            }
            let mut buf = [0u8; 1024];
            let room = buf.len().min(65536 - bytes.len());
            let n = unsafe { libc::read(0, buf.as_mut_ptr().cast(), room) };
            if n <= 0 {
                break;
            }
            bytes.extend_from_slice(&buf[..n as usize]);
            let (_, _, boundary) = palette::probe_replies(&bytes);
            if boundary {
                break;
            }
        }
        let (palette, input, _) = palette::probe_replies(&bytes);
        Ok(Self { palette, input })
    }
}
pub fn relay(
    terminal: UnixStream,
    session: &str,
    control: impl FnMut(&str, Value) -> Result<Value>,
    stop: &AtomicBool,
    resize: &AtomicBool,
) -> Result<i32> {
    relay_with_input(terminal, session, control, stop, resize, Vec::new())
}
pub fn relay_with_input(
    mut terminal: UnixStream,
    session: &str,
    mut control: impl FnMut(&str, Value) -> Result<Value>,
    stop: &AtomicBool,
    resize: &AtomicBool,
    input: Vec<u8>,
) -> Result<i32> {
    use std::io::{Read, Write};
    terminal.set_read_timeout(None)?;
    terminal.set_nonblocking(true)?;
    let _mode = TerminalMode::raw()?;
    let _output_flags = NonblockingOutput::new()?;
    let mut to_terminal = Vec::<u8>::new();
    let mut to_shell = input;
    let mut eof = false;
    loop {
        if stop.load(Ordering::Relaxed) {
            return Err("terminal delivery interrupted; session remains daemon-owned".into());
        }
        if resize.swap(false, Ordering::Relaxed) && !eof {
            let d = size(0)?;
            // Startup may not have a PTY yet; retry the first dimensions later.
            if control(
                "sessions.resize",
                json!({"session":session,"rows":d.ws_row.max(1),"cols":d.ws_col.max(1)}),
            )
            .is_err()
            {
                resize.store(true, Ordering::Relaxed);
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
        let record = control("sessions.get", json!({"session":session}))?;
        // The daemon keeps this half-closed attachment until we drop the
        // stream, so a subsequent attachment cannot overwrite its outcome.
        if record["terminal_detached"] == true {
            return Ok(0);
        }
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
                    record["stop_reason"]
                        .as_str()
                        .or_else(|| record["detail"].as_str())
                        .unwrap_or("payload exit status unknown")
                        .into()
                });
        }
        if stop.load(Ordering::Relaxed) {
            return Err("terminal completion interrupted".into());
        }
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
}
