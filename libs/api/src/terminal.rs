//! Bounded raw terminal relay shared by host and sandbox clients.
//! Control calls and the authenticated stream are supplied by the caller.
use crate::contract;
use serde_json::Value;
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
        let mut previous = unsafe { std::mem::zeroed() };
        cvt(unsafe { libc::tcgetattr(0, &mut previous) })?;
        let mut raw = previous;
        unsafe {
            libc::cfmakeraw(&mut raw);
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
struct NonblockingOutput(RawFd, i32);
impl NonblockingOutput {
    fn new(fd: RawFd) -> Result<Self> {
        let flags = cvt(unsafe { libc::fcntl(fd, libc::F_GETFL) })?;
        cvt(unsafe { libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) })?;
        Ok(Self(fd, flags))
    }
}
impl Drop for NonblockingOutput {
    fn drop(&mut self) {
        unsafe {
            libc::fcntl(self.0, libc::F_SETFL, self.1);
        }
    }
}
pub fn size(fd: RawFd) -> Result<libc::winsize> {
    let mut s = unsafe { std::mem::zeroed() };
    cvt(unsafe { libc::ioctl(fd, libc::TIOCGWINSZ, &mut s) })?;
    Ok(s)
}
pub fn relay(
    terminal: UnixStream,
    session: &str,
    mut control: impl FnMut(&str, Value) -> Result<Value>,
    stop: &AtomicBool,
    resize: &AtomicBool,
    bridge: bool,
) -> Result<i32> {
    relay_with(
        terminal,
        &mut control,
        |control, rows, cols| {
            contract::invoke::<contract::host::SessionsResize, _>(
                contract::SessionResizeParams {
                    session: session.into(),
                    rows,
                    cols,
                },
                control,
            )
            .map(drop)
        },
        |control| wait_outcome(session, control, stop),
        stop,
        resize,
        bridge,
    )
}
/// Relay the frontend's terminal to `terminal` until the stream ends. The
/// caller says how to resize what is attached and how to find the outcome,
/// each with `context`, whatever the caller calls the daemon with.
///
/// A `bridge` relays for a program over pipes instead: standard input and
/// output are not a terminal and the program resizes through the daemon
/// itself. The end of standard input closes the stream at once and returns
/// 0 without the outcome, whatever input the stream has not taken; what
/// closing the stream means is the attachment's (see `docs/API.org`).
pub fn relay_with<C>(
    terminal: UnixStream,
    context: C,
    resize_to: impl FnMut(&mut C, u16, u16) -> Result<()>,
    outcome: impl FnOnce(&mut C) -> Result<i32>,
    stop: &AtomicBool,
    resize: &AtomicBool,
    bridge: bool,
) -> Result<i32> {
    relay_io(
        (0, 1),
        terminal,
        context,
        resize_to,
        outcome,
        stop,
        resize,
        bridge,
    )
}
#[allow(clippy::too_many_arguments)]
fn relay_io<C>(
    (input_fd, output_fd): (RawFd, RawFd),
    mut terminal: UnixStream,
    mut context: C,
    mut resize_to: impl FnMut(&mut C, u16, u16) -> Result<()>,
    outcome: impl FnOnce(&mut C) -> Result<i32>,
    stop: &AtomicBool,
    resize: &AtomicBool,
    bridge: bool,
) -> Result<i32> {
    use std::io::{Read, Write};
    terminal.set_read_timeout(None)?;
    terminal.set_nonblocking(true)?;
    let _mode = if bridge {
        None
    } else {
        Some(TerminalMode::raw()?)
    };
    let _output_flags = NonblockingOutput::new(output_fd)?;
    let mut to_terminal = Vec::<u8>::new();
    let mut to_shell = Vec::<u8>::new();
    let mut eof = false;
    let mut input = true;
    loop {
        if stop.load(Ordering::Relaxed) {
            return Err("terminal delivery interrupted; session remains daemon-owned".into());
        }
        if !bridge && resize.swap(false, Ordering::Relaxed) && !eof {
            let d = size(0)?;
            // Startup may not have a PTY yet; retry the first dimensions later.
            if resize_to(&mut context, d.ws_row.max(1), d.ws_col.max(1)).is_err() {
                resize.store(true, Ordering::Relaxed);
            }
        }
        if eof && to_terminal.is_empty() {
            break;
        }
        let mut fds = [
            // A bridge keeps polling a full input: its hangup still counts.
            libc::pollfd {
                fd: if input { input_fd } else { -1 },
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
                fd: output_fd,
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
        let mut closed = false;
        if fds[0].events != 0 && fds[0].revents & libc::POLLIN != 0 {
            let mut bytes = [0; 8192];
            let room = bytes.len().min(65536 - to_shell.len());
            let n = unsafe { libc::read(input_fd, bytes.as_mut_ptr().cast(), room) };
            if n == 0 && bridge {
                closed = true;
            } else if n <= 0 {
                return Err("terminal input disconnected; session remains daemon-owned".into());
            } else {
                to_shell.extend_from_slice(&bytes[..n as usize]);
            }
        } else if bridge && fds[0].revents & (libc::POLLHUP | libc::POLLERR) != 0 {
            // A closed pipe with nothing left in it, or one there is no room
            // to read: either way the program has gone.
            closed = true;
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
        // Decided after the stream's read: a stream that has ended, also in
        // this same poll, keeps its outcome.
        if closed {
            if eof {
                input = false;
            } else {
                // Offer what is queued once, without waiting for it.
                let _ = terminal.write(&to_shell);
                return Ok(0);
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
            write_output(output_fd, &mut to_terminal)?;
        } else if fds[2].revents & (libc::POLLERR | libc::POLLHUP) != 0 {
            // Its reader has gone: poll reports that even with nothing to
            // write, and would not wait again.
            return Err("terminal output disconnected; session remains daemon-owned".into());
        }
    }
    outcome(&mut context)
}

// A nonblocking writer may make partial progress; an output failure must
// never become a successful terminal completion.
fn write_output(fd: RawFd, pending: &mut Vec<u8>) -> Result<()> {
    let n = unsafe { libc::write(fd, pending.as_ptr().cast(), pending.len().min(1024)) };
    if n > 0 {
        pending.drain(..n as usize);
    } else if n == 0
        || !matches!(
            io::Error::last_os_error().kind(),
            io::ErrorKind::WouldBlock | io::ErrorKind::Interrupted
        )
    {
        return Err("terminal output incomplete".into());
    }
    Ok(())
}

// Shared completion phase, kept separate from process-global terminal mode.
fn wait_outcome(
    session: &str,
    mut control: impl FnMut(&str, Value) -> Result<Value>,
    stop: &AtomicBool,
) -> Result<i32> {
    // Stream EOF precedes local drain above. A control disconnect is never a
    // substitute for EOF, and a daemon crash must not look like normal exit.
    loop {
        let record = contract::invoke::<contract::host::SessionsGet, _>(
            contract::SessionSelectorParams {
                session: session.into(),
            },
            &mut control,
        )?;
        // The daemon keeps this half-closed attachment until we drop the
        // stream, so a subsequent attachment cannot overwrite its outcome.
        if record.terminal_detached {
            return Ok(0);
        }
        if matches!(record.state.as_str(), "stopped" | "failed") {
            if record.state == "failed" {
                return Err(record.detail.as_deref().unwrap_or("launch failed").into());
            }
            if record.terminal_interrupted {
                return Err("terminal output incomplete".into());
            }
            // The raw stream and lifecycle notifications travel separately.
            // Stopped can be visible before the drained terminal notification.
            if record.terminal_complete {
                return record.exit_code.ok_or_else(|| {
                    record
                        .stop_reason
                        .as_deref()
                        .or(record.detail.as_deref())
                        .unwrap_or("payload exit status unknown")
                        .into()
                });
            }
        }
        if stop.load(Ordering::Relaxed) {
            return Err("terminal completion interrupted".into());
        }
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    #[test]
    fn a_failed_output_writer_is_not_a_successful_terminal_completion() {
        let full = std::fs::OpenOptions::new()
            .write(true)
            .open("/dev/full")
            .unwrap();
        let mut pending = b"final output".to_vec();
        assert_eq!(
            write_output(full.as_raw_fd(), &mut pending)
                .unwrap_err()
                .to_string(),
            "terminal output incomplete"
        );
        assert_eq!(pending, b"final output");
        let sink = std::fs::OpenOptions::new()
            .write(true)
            .open("/dev/null")
            .unwrap();
        let mut pending = vec![b'x'; 2048];
        write_output(sink.as_raw_fd(), &mut pending).unwrap();
        assert_eq!(pending.len(), 1024);
        write_output(sink.as_raw_fd(), &mut pending).unwrap();
        assert!(pending.is_empty());
    }

    #[test]
    fn a_bridge_leaves_at_once_when_its_input_closes_with_input_the_stream_does_not_take() {
        let pipe = || {
            let mut fds = [0; 2];
            cvt(unsafe { libc::pipe2(fds.as_mut_ptr(), libc::O_CLOEXEC) }).unwrap();
            fds
        };
        let [input, writer] = pipe();
        let [reader, output] = pipe();
        // The attached side never reads what it is sent.
        let (terminal, _deaf) = UnixStream::pair().unwrap();
        let (done, finished) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let result = relay_io(
                (input, output),
                terminal,
                (),
                |_, _, _| panic!("a bridge does not resize"),
                |_| panic!("no outcome when the input ends"),
                &AtomicBool::new(false),
                &AtomicBool::new(false),
                true,
            );
            done.send(result.map_err(|e| e.to_string())).unwrap();
        });
        // Fill the stream, the relay's queue and the pipe until nothing moves.
        cvt(unsafe { libc::fcntl(writer, libc::F_SETFL, libc::O_NONBLOCK) }).unwrap();
        let chunk = [b'x'; 65536];
        let mut stalled = 0;
        while stalled < 10 {
            if unsafe { libc::write(writer, chunk.as_ptr().cast(), chunk.len()) } > 0 {
                stalled = 0;
            } else {
                stalled += 1;
                std::thread::sleep(std::time::Duration::from_millis(20));
            }
        }
        unsafe { libc::close(writer) };
        let result = finished
            .recv_timeout(std::time::Duration::from_secs(2))
            .expect("the bridge left at once");
        assert_eq!(result, Ok(0));
        unsafe {
            libc::close(input);
            libc::close(reader);
            libc::close(output);
        }
    }

    #[test]
    fn a_bridge_whose_input_and_stream_end_together_keeps_the_outcome() {
        let pipe = || {
            let mut fds = [0; 2];
            cvt(unsafe { libc::pipe2(fds.as_mut_ptr(), libc::O_CLOEXEC) }).unwrap();
            fds
        };
        let [input, writer] = pipe();
        let [reader, output] = pipe();
        let (terminal, ended) = UnixStream::pair().unwrap();
        // Both are ready at the first poll.
        drop(ended);
        unsafe { libc::close(writer) };
        let result = relay_io(
            (input, output),
            terminal,
            (),
            |_, _, _| panic!("a bridge does not resize"),
            |_| Ok(7),
            &AtomicBool::new(false),
            &AtomicBool::new(false),
            true,
        );
        assert_eq!(result.unwrap(), 7);
        unsafe {
            libc::close(input);
            libc::close(reader);
            libc::close(output);
        }
    }

    #[test]
    fn a_relay_whose_output_reader_goes_away_ends_while_idle() {
        let pipe = || {
            let mut fds = [0; 2];
            cvt(unsafe { libc::pipe2(fds.as_mut_ptr(), libc::O_CLOEXEC) }).unwrap();
            fds
        };
        let [input, writer] = pipe();
        let [reader, output] = pipe();
        // The goblin stays quiet and the program keeps its input open.
        let (terminal, _quiet) = UnixStream::pair().unwrap();
        unsafe { libc::close(reader) };
        let (done, finished) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let result = relay_io(
                (input, output),
                terminal,
                (),
                |_, _, _| panic!("a bridge does not resize"),
                |_| panic!("no outcome when the output is gone"),
                &AtomicBool::new(false),
                &AtomicBool::new(false),
                true,
            );
            done.send(result.map_err(|e| e.to_string())).unwrap();
        });
        let result = finished
            .recv_timeout(std::time::Duration::from_secs(2))
            .expect("the relay left at once");
        assert_eq!(
            result,
            Err("terminal output disconnected; session remains daemon-owned".into())
        );
        unsafe {
            libc::close(input);
            libc::close(writer);
            libc::close(output);
        }
    }

    fn completion_record(outcome: Value) -> Value {
        let mut record = serde_json::from_str::<Value>(include_str!("../tests/fixtures.json"))
            .unwrap()["host_session"]
            .clone();
        record
            .as_object_mut()
            .unwrap()
            .extend(outcome.as_object().unwrap().clone());
        record
    }
    #[test]
    fn completion_wait_preserves_failure_unknown_exit_and_control_loss() {
        for (record, expected) in [
            (
                json!({"state":"failed","detail":"launch failed"}),
                "launch failed",
            ),
            (
                json!({"state":"stopped","terminal_complete":true,"terminal_interrupted":false}),
                "payload exit status unknown",
            ),
        ] {
            let error = wait_outcome(
                "s",
                |_, _| Ok(completion_record(record.clone())),
                &AtomicBool::new(false),
            )
            .unwrap_err();
            assert_eq!(error.to_string(), expected);
        }
        let error = wait_outcome(
            "s",
            |_, _| Err("control disconnected".into()),
            &AtomicBool::new(false),
        )
        .unwrap_err();
        assert_eq!(error.to_string(), "control disconnected");
        let error = wait_outcome(
            "s",
            |_, _| {
                Ok(completion_record(
                    json!({"state":"stopped","terminal_complete":false}),
                ))
            },
            &AtomicBool::new(true),
        )
        .unwrap_err();
        assert_eq!(error.to_string(), "terminal completion interrupted");
    }
    #[test]
    fn c4_interrupted_terminal_completion_remains_an_error() {
        let result = wait_outcome(
            "s",
            |_, _| {
                Ok(completion_record(
                    json!({"state":"stopped","terminal_complete":false,"terminal_interrupted":true,"terminal_detached":false,"exit_code":0}),
                ))
            },
            &AtomicBool::new(false),
        );
        assert_eq!(
            result.unwrap_err().to_string(),
            "terminal output incomplete"
        );
    }
    #[test]
    fn c4_terminal_eof_waits_for_delayed_completion_state() {
        let mut states = std::collections::VecDeque::from([
            json!({"state":"stopped","terminal_complete":false,"terminal_interrupted":false,"terminal_detached":false,"exit_code":0}),
            json!({"state":"stopped","terminal_complete":true,"terminal_interrupted":false,"terminal_detached":false,"exit_code":0}),
        ]);
        let result = wait_outcome(
            "s",
            |method, params| {
                assert_eq!(method, "sessions.get");
                assert_eq!(params, json!({"session":"s"}));
                Ok(completion_record(
                    states
                        .pop_front()
                        .expect("completion must be consumed exactly once"),
                ))
            },
            &AtomicBool::new(false),
        );
        assert_eq!(result.unwrap(), 0);
        assert!(states.is_empty());
    }
}
