//! `goblins exec`: run a command inside a running sandbox. The client owns
//! the stdio relay (or the PTY master with -t); the daemon only hands the
//! sandbox ends to goblins-init and reports the exit status. This
//! connection stays open for the exec's life: closing it cancels the exec.
use goblins_controller::{
    Result,
    host::{Client, ExecStdio},
};
use goblins_jsonrpc::{self as rpc, Decoder};
use goblins_protocol::contract::{self, Method, Response};
use goblins_unix as unix;
use serde_json::Value;
#[cfg(test)]
use serde_json::json;
use std::{
    io::{self, Read, Write},
    os::fd::{AsRawFd, OwnedFd, RawFd},
    path::PathBuf,
    sync::atomic::{AtomicBool, Ordering},
    time::{Duration, Instant},
};

pub struct Options {
    pub goblin: String,
    pub command: Vec<String>,
    pub tty: String,
    pub env: Vec<String>,
    pub workdir: Option<String>,
    pub instance: Option<String>,
    pub session: Option<String>,
}

/// Exit code for failures of goblins itself rather than the command.
const FAILURE: i32 = 255;
/// After the command exits, output still arriving from processes it left
/// behind is relayed until the streams are idle this long.
const DRAIN_IDLE: Duration = Duration::from_millis(200);
const DRAIN_LIMIT: Duration = Duration::from_secs(2);

static HUP: AtomicBool = AtomicBool::new(false);
static INT: AtomicBool = AtomicBool::new(false);
static TERM: AtomicBool = AtomicBool::new(false);
static WINCH: AtomicBool = AtomicBool::new(false);
extern "C" fn caught(signal: i32) {
    match signal {
        libc::SIGHUP => &HUP,
        libc::SIGINT => &INT,
        libc::SIGTERM => &TERM,
        _ => &WINCH,
    }
    .store(true, Ordering::Relaxed);
}

struct RawMode(Option<libc::termios>);
impl RawMode {
    fn new(enable: bool) -> Result<Self> {
        if !enable {
            return Ok(Self(None));
        }
        let mut previous = unsafe { std::mem::zeroed() };
        unix::cvt(unsafe { libc::tcgetattr(0, &mut previous) })?;
        let mut raw = previous;
        unsafe { libc::cfmakeraw(&mut raw) };
        unix::cvt(unsafe { libc::tcsetattr(0, libc::TCSANOW, &raw) })?;
        Ok(Self(Some(previous)))
    }
}
impl Drop for RawMode {
    fn drop(&mut self) {
        if let Some(previous) = &self.0 {
            unsafe { libc::tcsetattr(0, libc::TCSANOW, previous) };
        }
    }
}

fn window(fd: RawFd) -> Option<libc::winsize> {
    let mut size: libc::winsize = unsafe { std::mem::zeroed() };
    (unsafe { libc::ioctl(fd, libc::TIOCGWINSZ, &mut size) } == 0 && size.ws_row > 0)
        .then_some(size)
}

/// One direction of the relay: bytes read from `from` wait in `buffer` for
/// `to`. Descriptors we created are non-blocking; our own stdio is polled.
struct Pipe {
    from: Option<RawFd>,
    to: Option<RawFd>,
    buffer: Vec<u8>,
    /// Close `to` once `from` ends and the buffer drains (stdin's EOF).
    close: Option<OwnedFd>,
}
impl Pipe {
    fn new(from: RawFd, to: RawFd) -> Self {
        Self {
            from: Some(from),
            to: Some(to),
            buffer: Vec::new(),
            close: None,
        }
    }
    fn done(&self) -> bool {
        self.from.is_none() && self.buffer.is_empty()
    }
}

pub fn run(state: PathBuf, options: Options) -> Result<i32> {
    match exec(state, options) {
        Ok(code) => Ok(code),
        Err(e) => {
            eprintln!("goblins: {e}");
            Ok(FAILURE)
        }
    }
}

/// The exec request, given whether stdin is a terminal.
fn params(options: Options, terminal: bool) -> Result<contract::ExecParams> {
    let tty = match options.tty.as_str() {
        "always" => true,
        "auto" => terminal,
        _ => false,
    };
    let mut env = Vec::new();
    for entry in &options.env {
        let (name, value) = entry
            .split_once('=')
            .ok_or_else(|| format!("-e expects NAME=VALUE, not {entry}"))?;
        env.push((name.to_string(), value.to_string()));
    }
    Ok(contract::ExecParams {
        session: options.goblin,
        tty,
        interactive: options.tty == "always",
        env,
        argv: (!options.command.is_empty()).then_some(options.command),
        cwd: options.workdir,
        pin: options.session,
    })
}

/// The exit code for a finished exec: the command's, 128 plus the signal that
/// ended it, or FAILURE when the result says neither.
fn exit_code(result: &<contract::host::SessionsExec as Method>::Result) -> i32 {
    match (result.code, result.signal) {
        (Some(code), _) => code as i32,
        (None, Some(signal)) => 128 + signal as i32,
        _ => FAILURE,
    }
}

fn exec(state: PathBuf, options: Options) -> Result<i32> {
    let terminal = unsafe { libc::isatty(0) } == 1;
    let instance = options.instance.clone();
    let goblin = options.goblin.clone();
    let params = params(options, terminal)?;
    let tty = params.tty;
    let mut client = Client::connect(&state)?;
    if !client.features.iter().any(|f| f == "exec") {
        return Err("this daemon cannot run commands in sandboxes; restart it".into());
    }
    if instance
        .as_ref()
        .is_some_and(|expected| client.instance != *expected)
    {
        return Err(format!("daemon instance changed; goblin {goblin} is gone").into());
    }
    // The sandbox ends travel to goblins-init; we keep our ends.
    let mut pipes = Vec::new();
    let mut keep = Vec::new();
    let id;
    let master;
    if tty {
        let size = window(0).unwrap_or(libc::winsize {
            ws_row: 24,
            ws_col: 80,
            ws_xpixel: 0,
            ws_ypixel: 0,
        });
        let (m, slave) = unix::pty(size.ws_row, size.ws_col)?;
        id = client.send_exec(params, ExecStdio::Tty(&slave))?;
        unix::nonblocking(m.as_raw_fd())?;
        pipes.push(Pipe::new(0, m.as_raw_fd()));
        pipes.push(Pipe::new(m.as_raw_fd(), 1));
        master = Some(m);
    } else {
        let (stdin_r, stdin_w) = unix::pipe()?;
        let (stdout_r, stdout_w) = unix::pipe()?;
        let (stderr_r, stderr_w) = unix::pipe()?;
        id = client.send_exec(params, ExecStdio::Fds([&stdin_r, &stdout_w, &stderr_w]))?;
        for fd in [&stdin_w, &stdout_r, &stderr_r] {
            unix::nonblocking(fd.as_raw_fd())?;
        }
        let mut input = Pipe::new(0, stdin_w.as_raw_fd());
        input.close = Some(stdin_w);
        pipes.push(input);
        pipes.push(Pipe::new(stdout_r.as_raw_fd(), 1));
        pipes.push(Pipe::new(stderr_r.as_raw_fd(), 2));
        keep.extend([stdout_r, stderr_r]);
        master = None;
    }
    let mut stream = client.into_stream();
    stream.set_nonblocking(true)?;
    unsafe {
        for signal in [libc::SIGHUP, libc::SIGINT, libc::SIGTERM, libc::SIGWINCH] {
            libc::signal(signal, caught as *const () as libc::sighandler_t);
        }
    }
    let _raw = RawMode::new(tty && terminal)?;
    let reply = relay(&mut stream, &id, &mut pipes, master.as_ref())?;
    drop(keep);
    if let Some(error) = reply.get("error") {
        return Err(error["message"]
            .as_str()
            .unwrap_or("exec failed")
            .to_string()
            .into());
    }
    let result = <contract::host::SessionsExec as Method>::Result::decode(reply["result"].clone())?;
    if let Some(message) = &result.message {
        eprintln!("goblins: {}", goblins_protocol::plain_text(message));
    }
    Ok(exit_code(&result))
}

fn relay(
    stream: &mut std::os::unix::net::UnixStream,
    id: &Value,
    pipes: &mut [Pipe],
    master: Option<&OwnedFd>,
) -> Result<Value> {
    relay_with_clock(stream, id, pipes, master, Instant::now)
}

fn relay_with_clock(
    stream: &mut std::os::unix::net::UnixStream,
    id: &Value,
    pipes: &mut [Pipe],
    master: Option<&OwnedFd>,
    mut clock: impl FnMut() -> Instant,
) -> Result<Value> {
    let mut decoder = Decoder::new(rpc::MAX_BODY, None);
    let mut reply: Option<Value> = None;
    let mut finished: Option<Instant> = None;
    let mut last_output = clock();
    loop {
        for (flag, signal) in [
            (&INT, libc::SIGINT),
            (&TERM, libc::SIGTERM),
            (&HUP, libc::SIGHUP),
        ] {
            if flag.swap(false, Ordering::Relaxed) && reply.is_none() {
                let frame = rpc::encode(&contract::notification::<
                    contract::host_notifications::ExecSignal,
                >(contract::ExecSignalParams {
                    signal,
                })?)?;
                // Small and rare: a blocking write is fine.
                stream.set_nonblocking(false)?;
                stream.write_all(&frame)?;
                stream.set_nonblocking(true)?;
            }
        }
        if WINCH.swap(false, Ordering::Relaxed)
            && let (Some(master), Some(size)) = (master, window(0))
        {
            unsafe { libc::ioctl(master.as_raw_fd(), libc::TIOCSWINSZ, &size) };
        }
        if let Some(at) = finished {
            let outputs_done = pipes.iter().skip(1).all(Pipe::done);
            let now = clock();
            if outputs_done
                || now.duration_since(last_output) >= DRAIN_IDLE
                || now.duration_since(at) >= DRAIN_LIMIT
            {
                // Flush what is buffered, then stop.
                for pipe in pipes.iter_mut().skip(1) {
                    if let Some(to) = pipe.to {
                        let _ = write_all(to, &pipe.buffer);
                    }
                }
                return Ok(reply.unwrap());
            }
        }
        let mut polls = vec![libc::pollfd {
            fd: if reply.is_some() {
                -1
            } else {
                stream.as_raw_fd()
            },
            events: libc::POLLIN,
            revents: 0,
        }];
        for pipe in pipes.iter() {
            polls.push(libc::pollfd {
                fd: match pipe.from {
                    Some(fd) if pipe.buffer.len() < 65536 => fd,
                    _ => -1,
                },
                events: libc::POLLIN,
                revents: 0,
            });
            polls.push(libc::pollfd {
                fd: match pipe.to {
                    Some(fd) if !pipe.buffer.is_empty() => fd,
                    _ => -1,
                },
                events: libc::POLLOUT,
                revents: 0,
            });
        }
        let timeout = if finished.is_some() { 20 } else { 200 };
        if unsafe { libc::poll(polls.as_mut_ptr(), polls.len() as _, timeout) } < 0 {
            if io::Error::last_os_error().kind() == io::ErrorKind::Interrupted {
                continue;
            }
            return Err(io::Error::last_os_error().into());
        }
        if polls[0].revents != 0 {
            let mut bytes = [0; 8192];
            match stream.read(&mut bytes) {
                Ok(0) => return Err("lost the connection to the daemon".into()),
                Ok(n) => {
                    let (_, body) = decoder.push(&bytes[..n], clock())?;
                    if let Some(body) = body {
                        let value = rpc::parse(&body)?;
                        rpc::validate_response(&value, id)?;
                        reply = Some(value);
                        let at = clock();
                        finished = Some(at);
                        // Pre-exit silence is not part of the post-exit drain.
                        // Output readiness can follow the exit-status poll.
                        last_output = at;
                    }
                }
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => (),
                Err(e) if e.kind() == io::ErrorKind::Interrupted => (),
                Err(e) => return Err(e.into()),
            }
        }
        for (i, pipe) in pipes.iter_mut().enumerate() {
            let (read, write) = (polls[1 + 2 * i].revents, polls[2 + 2 * i].revents);
            if read != 0
                && let Some(from) = pipe.from
            {
                let mut bytes = [0; 16384];
                let n = unsafe { libc::read(from, bytes.as_mut_ptr().cast(), bytes.len()) };
                if n > 0 {
                    pipe.buffer.extend_from_slice(&bytes[..n as usize]);
                    if i > 0 {
                        last_output = clock();
                    }
                } else {
                    let error = io::Error::last_os_error();
                    // A PTY master reports EIO once every slave is closed.
                    if n == 0
                        || !matches!(
                            error.kind(),
                            io::ErrorKind::WouldBlock | io::ErrorKind::Interrupted
                        )
                    {
                        pipe.from = None;
                    }
                }
            }
            if write != 0
                && let Some(to) = pipe.to
            {
                let n = unsafe {
                    libc::write(
                        to,
                        pipe.buffer.as_ptr().cast(),
                        pipe.buffer.len().min(65536),
                    )
                };
                if n > 0 {
                    pipe.buffer.drain(..n as usize);
                } else if !matches!(
                    io::Error::last_os_error().kind(),
                    io::ErrorKind::WouldBlock | io::ErrorKind::Interrupted
                ) {
                    // The reader went away: drop this direction.
                    pipe.to = None;
                    pipe.from = None;
                    pipe.buffer.clear();
                    pipe.close.take();
                }
            }
            // Stdin's end of file reaches the command as end of file.
            if pipe.from.is_none() && pipe.buffer.is_empty() {
                pipe.close.take();
                if i == 0 && pipe.to.is_some() && master.is_none() {
                    pipe.to = None;
                }
            }
        }
    }
}

fn write_all(fd: RawFd, mut bytes: &[u8]) -> io::Result<()> {
    while !bytes.is_empty() {
        let n = unsafe { libc::write(fd, bytes.as_ptr().cast(), bytes.len()) };
        if n > 0 {
            bytes = &bytes[n as usize..];
        } else if io::Error::last_os_error().kind() == io::ErrorKind::WouldBlock {
            let mut p = libc::pollfd {
                fd,
                events: libc::POLLOUT,
                revents: 0,
            };
            unsafe { libc::poll(&mut p, 1, 100) };
        } else if io::Error::last_os_error().kind() != io::ErrorKind::Interrupted {
            return Err(io::Error::last_os_error());
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        fs::File,
        os::unix::net::UnixStream,
        process::{Child, Command, Stdio},
    };

    /// The signal flags are the process's: one relay reads them at a time.
    static SIGNALS: std::sync::Mutex<()> = std::sync::Mutex::new(());

    /// `goblins exec ARGS` as parsed.
    fn options(args: &[&str]) -> Options {
        use clap::Parser;
        let Some(crate::cli::Command::Exec {
            tty,
            env,
            workdir,
            instance,
            session,
            goblin,
            command,
        }) = crate::cli::Cli::try_parse_from(["goblins", "exec"].iter().chain(args))
            .unwrap()
            .command
        else {
            unreachable!()
        };
        Options {
            goblin,
            command,
            tty,
            env,
            workdir,
            instance,
            session,
        }
    }

    #[test]
    fn exec_requests_carry_the_options_and_no_descriptors() {
        let named = [
            "--session",
            "pinned",
            "--workdir",
            "/work",
            "-e",
            "A=b",
            "agent",
            "--",
            "echo",
            "hello",
        ];
        let cases = [
            (
                &["agent"][..],
                false,
                json!({"session":"agent","env":[],"tty":false,"interactive":false}),
            ),
            (
                &["agent"],
                true,
                json!({"session":"agent","env":[],"tty":false,"interactive":false}),
            ),
            (
                &["-t", "agent"],
                false,
                json!({"session":"agent","env":[],"tty":true,"interactive":true}),
            ),
            (
                &["--tty=auto", "agent"],
                true,
                json!({"session":"agent","env":[],"tty":true,"interactive":false}),
            ),
            (
                &["--tty=auto", "agent"],
                false,
                json!({"session":"agent","env":[],"tty":false,"interactive":false}),
            ),
            (
                &named,
                false,
                json!({"session":"agent","env":[["A","b"]],"tty":false,"interactive":false,"pin":"pinned","cwd":"/work","argv":["echo","hello"]}),
            ),
        ];
        for (args, terminal, expected) in cases {
            let params = params(options(args), terminal).unwrap();
            assert_eq!(
                contract::params_value::<contract::host::SessionsExec>(params).unwrap(),
                expected,
                "{args:?} {terminal}"
            );
        }
        assert_eq!(
            params(options(&["-e", "A", "agent"]), false)
                .unwrap_err()
                .to_string(),
            "-e expects NAME=VALUE, not A"
        );
    }

    #[test]
    fn exec_exit_code_is_the_command_status_or_failure() {
        let cases = [
            (json!({"code":7,"signal":null,"future":true}), 7),
            (json!({"code":127,"message":"fixture missing"}), 127),
            (json!({"code":null,"signal":15}), 143),
            (json!({}), FAILURE),
            (json!({"code":0,"signal":"bad"}), 0),
            (json!({"code":"bad","signal":15}), 143),
            (json!({"code":3,"message":42}), 3),
            (json!({"code":true,"signal":15}), 143),
            (Value::Null, FAILURE),
        ];
        for (wire, expected) in cases {
            let result =
                <contract::host::SessionsExec as Method>::Result::decode(wire.clone()).unwrap();
            assert_eq!(exit_code(&result), expected, "{wire}");
        }
    }

    #[test]
    fn interrupt_terminate_and_hangup_reach_the_remote_process_group() {
        let _signals = SIGNALS.lock().unwrap_or_else(|e| e.into_inner());
        let _no_forks = unix::no_forks();
        let (mut stream, mut daemon) = UnixStream::pair().unwrap();
        stream.set_nonblocking(true).unwrap();
        daemon.set_read_timeout(Some(rpc::TIMEOUT)).unwrap();
        // Caught before the exit status: each is forwarded once, in order.
        for signal in [libc::SIGINT, libc::SIGTERM, libc::SIGHUP] {
            caught(signal);
        }
        let expected = rpc::result(json!(1), json!({"code":null,"signal":2}));
        daemon.write_all(&rpc::encode(&expected).unwrap()).unwrap();
        let mut pipes = [Pipe {
            from: None,
            to: None,
            buffer: vec![],
            close: None,
        }];
        let reply = relay(&mut stream, &json!(1), &mut pipes, None).unwrap();
        assert_eq!(reply, expected);
        for signal in [2, 15, 1] {
            assert_eq!(
                rpc::read(&mut daemon).unwrap(),
                json!({"jsonrpc":"2.0","method":"exec.signal","params":{"signal":signal}})
            );
        }
        // Nothing else was sent, and no flag is left for the next relay.
        drop(stream);
        let mut rest = vec![];
        daemon.read_to_end(&mut rest).unwrap();
        assert!(rest.is_empty());
        assert!(
            ![&INT, &TERM, &HUP]
                .iter()
                .any(|f| f.load(Ordering::Relaxed))
        );
    }

    #[test]
    fn an_instance_pin_refuses_another_daemon() {
        let _no_forks = unix::no_forks();
        let daemon = crate::fake_daemon::Daemon::start(&["exec"], Value::Null);
        let error = exec(
            daemon.state.clone(),
            options(&[
                "--instance",
                "wrong-instance",
                "--session",
                "session-id",
                "gribble",
            ]),
        )
        .unwrap_err();
        assert_eq!(
            error.to_string(),
            "daemon instance changed; goblin gribble is gone"
        );
        // Nothing was asked of the other daemon.
        assert_eq!(daemon.call(), Value::Null);
    }

    #[test]
    fn finished_relay_stops_at_the_idle_bound_even_with_a_retained_writer() {
        let _signals = SIGNALS.lock().unwrap_or_else(|e| e.into_inner());
        let _no_forks = unix::no_forks();
        let (mut stream, mut daemon) = UnixStream::pair().unwrap();
        stream.set_nonblocking(true).unwrap();
        let (input, _retained_writer) = unix::pipe().unwrap();
        let (_captured, output) = unix::pipe().unwrap();
        unix::nonblocking(input.as_raw_fd()).unwrap();
        let expected = rpc::result(json!(1), json!({"code":7}));
        daemon.write_all(&rpc::encode(&expected).unwrap()).unwrap();
        let mut pipes = [
            Pipe {
                from: None,
                to: None,
                buffer: vec![],
                close: None,
            },
            Pipe::new(input.as_raw_fd(), output.as_raw_fd()),
        ];
        let at = Instant::now();
        let mut reads = 0;
        let reply = relay_with_clock(&mut stream, &json!(1), &mut pipes, None, || {
            reads += 1;
            match reads {
                1..=3 => at,
                4 => at + DRAIN_IDLE,
                _ => at + DRAIN_LIMIT,
            }
        })
        .unwrap();
        assert_eq!(reply, expected);
        assert_eq!(reads, 4, "idle bound waited for EOF or the absolute bound");
        assert!(pipes[1].from.is_some());
    }

    #[test]
    fn finished_relay_stops_at_the_absolute_bound_despite_recent_output() {
        let _signals = SIGNALS.lock().unwrap_or_else(|e| e.into_inner());
        let _no_forks = unix::no_forks();
        let (mut stream, mut daemon) = UnixStream::pair().unwrap();
        stream.set_nonblocking(true).unwrap();
        let (input, writer) = unix::pipe().unwrap();
        let mut writer = File::from(writer);
        unix::nonblocking(input.as_raw_fd()).unwrap();
        let (captured, output) = unix::pipe().unwrap();
        writer.write_all(b"x").unwrap();
        let expected = rpc::result(json!(1), json!({"code":7}));
        daemon.write_all(&rpc::encode(&expected).unwrap()).unwrap();
        let mut pipes = [
            Pipe {
                from: None,
                to: None,
                buffer: vec![],
                close: None,
            },
            Pipe::new(input.as_raw_fd(), output.as_raw_fd()),
        ];
        let at = Instant::now();
        let mut reads = 0;
        let reply = relay_with_clock(&mut stream, &json!(1), &mut pipes, None, || {
            reads += 1;
            match reads {
                1..=3 => at,
                4 => at + Duration::from_millis(100),
                5 => {
                    writer.write_all(b"y").unwrap();
                    at + Duration::from_millis(150)
                }
                6 => at + DRAIN_LIMIT - Duration::from_millis(100),
                7 => at + DRAIN_LIMIT,
                _ => at + DRAIN_LIMIT + DRAIN_IDLE,
            }
        })
        .unwrap();
        assert_eq!(reply, expected);
        assert_eq!(reads, 7, "fresh output bypassed the absolute drain bound");
        assert!(pipes[1].from.is_some());
        drop(output);
        let mut text = vec![];
        File::from(captured).read_to_end(&mut text).unwrap();
        assert_eq!(text, b"xy");
    }

    #[test]
    fn silent_command_status_does_not_discard_pty_output_ready_after_the_poll() {
        let _signals = SIGNALS.lock().unwrap_or_else(|e| e.into_inner());
        let _fork = unix::forking();
        let (mut stream, mut daemon) = UnixStream::pair().unwrap();
        stream.set_nonblocking(true).unwrap();
        let (master, slave) = unix::pty(24, 80).unwrap();
        unix::nonblocking(master.as_raw_fd()).unwrap();
        let (captured, output) = unix::pipe().unwrap();
        struct Running {
            child: Child,
            reaped: bool,
        }
        impl Drop for Running {
            fn drop(&mut self) {
                if !self.reaped {
                    let _ = self.child.kill();
                    let _ = self.child.wait();
                }
            }
        }
        let mut child = Running { child: Command::new("sh")
            .args(["-c", "read gate; printf 'goblins: opening the files in Emacs was refused\\n' >&2; exit 7"])
            .stdin(Stdio::piped())
            .stdout(Stdio::from(File::from(slave.try_clone().unwrap())))
            .stderr(Stdio::from(File::from(slave)))
            .spawn().unwrap(), reaped: false };
        let expected = rpc::result(json!(1), json!({"code":7}));
        daemon.write_all(&rpc::encode(&expected).unwrap()).unwrap();
        let mut pipes = [
            Pipe {
                from: None,
                to: None,
                buffer: vec![],
                close: None,
            },
            Pipe::new(master.as_raw_fd(), output.as_raw_fd()),
        ];
        let started = Instant::now();
        let after_silence = started + DRAIN_IDLE * 2;
        let mut clock_reads = 0;
        let reply = relay_with_clock(&mut stream, &json!(1), &mut pipes, Some(&master), || {
            clock_reads += 1;
            if clock_reads == 3 {
                // This is exit observation, after poll took its readiness
                // snapshot with no PTY output. Make final output ready now.
                child
                    .child
                    .stdin
                    .take()
                    .unwrap()
                    .write_all(b"go\n")
                    .unwrap();
                assert_eq!(child.child.wait().unwrap().code(), Some(7));
                child.reaped = true;
            }
            if clock_reads == 1 {
                started
            } else {
                after_silence
            }
        })
        .unwrap();
        assert!(child.reaped);
        assert_eq!(reply, expected);
        drop(output);
        let mut text = vec![];
        File::from(captured).read_to_end(&mut text).unwrap();
        assert_eq!(text, b"goblins: opening the files in Emacs was refused\r\n");
    }
}
