//! `goblins exec`: run a command inside a running sandbox. The client owns
//! the stdio relay (or the PTY master with -t); the daemon only hands the
//! sandbox ends to goblins-init and reports the exit status. This
//! connection stays open for the exec's life: closing it cancels the exec.
use goblins_controller::{Result, host::Client, unix};
use goblins_protocol::rpc::{self, Decoder};
use serde_json::{Value, json};
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

fn exec(state: PathBuf, options: Options) -> Result<i32> {
    let terminal = unsafe { libc::isatty(0) } == 1;
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
        env.push(json!([name, value]));
    }
    let mut client = Client::connect(&state)?;
    if !client.features.iter().any(|f| f == "exec") {
        return Err("this daemon cannot run commands in sandboxes; restart it".into());
    }
    if options
        .instance
        .as_ref()
        .is_some_and(|expected| client.instance != *expected)
    {
        return Err(format!("daemon instance changed; goblin {} is gone", options.goblin).into());
    }
    let mut params = json!({
        "session": options.goblin,
        "tty": tty,
        "env": env,
    });
    if !options.command.is_empty() {
        params["argv"] = json!(options.command);
    }
    if let Some(cwd) = &options.workdir {
        params["cwd"] = json!(cwd);
    }
    if let Some(pin) = &options.session {
        params["pin"] = json!(pin);
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
        id = client.send_with_fds("sessions.exec", params, &[slave.as_raw_fd()])?;
        unix::nonblocking(m.as_raw_fd())?;
        pipes.push(Pipe::new(0, m.as_raw_fd()));
        pipes.push(Pipe::new(m.as_raw_fd(), 1));
        master = Some(m);
    } else {
        let (stdin_r, stdin_w) = unix::pipe()?;
        let (stdout_r, stdout_w) = unix::pipe()?;
        let (stderr_r, stderr_w) = unix::pipe()?;
        id = client.send_with_fds(
            "sessions.exec",
            params,
            &[
                stdin_r.as_raw_fd(),
                stdout_w.as_raw_fd(),
                stderr_w.as_raw_fd(),
            ],
        )?;
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
    let result = &reply["result"];
    if let Some(message) = result["message"].as_str() {
        eprintln!("goblins: {}", goblins_protocol::plain_text(message));
    }
    Ok(match (result["code"].as_i64(), result["signal"].as_i64()) {
        (Some(code), _) => code as i32,
        (None, Some(signal)) => 128 + signal as i32,
        _ => FAILURE,
    })
}

fn relay(
    stream: &mut std::os::unix::net::UnixStream,
    id: &Value,
    pipes: &mut [Pipe],
    master: Option<&OwnedFd>,
) -> Result<Value> {
    let mut decoder = Decoder::new(rpc::MAX_BODY, None);
    let mut reply: Option<Value> = None;
    let mut finished: Option<Instant> = None;
    let mut last_output = Instant::now();
    loop {
        for (flag, signal) in [
            (&INT, libc::SIGINT),
            (&TERM, libc::SIGTERM),
            (&HUP, libc::SIGHUP),
        ] {
            if flag.swap(false, Ordering::Relaxed) && reply.is_none() {
                let frame = rpc::encode(
                    &json!({"jsonrpc":"2.0","method":"exec.signal","params":{"signal":signal}}),
                )?;
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
            let now = Instant::now();
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
                    let (_, body) = decoder.push(&bytes[..n], Instant::now())?;
                    if let Some(body) = body {
                        let value = rpc::parse(&body)?;
                        rpc::validate_response(&value, id)?;
                        reply = Some(value);
                        finished = Some(Instant::now());
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
                        last_output = Instant::now();
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
