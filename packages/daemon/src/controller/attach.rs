//! `attach.sock`: a frontend writes a framed `attach.hello` with its version
//! and size before anything else. A goblin without attach handlers then
//! attaches its one terminal. A goblin with `onAttach` runs its agent without
//! a terminal and starts the client on a new PTY for each attachment, once
//! the variables the client needs were set during this run.
use super::{
    Active, Controller,
    policy::Operation,
    runtime::{Command, Event, SpawnKind, Stdio},
};
use crate::{config, terminal::Terminal, unix};
use goblins_protocol::rpc;
use serde::Deserialize;
use serde_json::{Value, json};
use std::{
    fs::{self, File},
    io::{self, Read, Write},
    os::{
        fd::{AsRawFd, OwnedFd},
        unix::{
            fs::{OpenOptionsExt, PermissionsExt},
            net::{UnixListener, UnixStream},
        },
    },
    path::{Path, PathBuf},
    time::{Duration, Instant},
};

/// The `attach.hello` version this daemon speaks.
const VERSION: u64 = 1;
/// Frontends that have connected but not yet attached, per goblin.
const HELLOS: usize = 8;
/// Between `attach.waiting` notifications.
const NOTICE: Duration = Duration::from_secs(1);
/// What `onDetach` may take before the client is cancelled.
const ON_DETACH: Duration = Duration::from_secs(5);
/// The server log rotates to `server.log.1` beyond this size.
const LOG: u64 = 1 << 20;
/// What a goblin with attach handlers answers on `terminal.sock`.
pub(super) const UPGRADE: &str =
    "this goblin starts a client at each attach; upgrade Goblins to attach";

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct HelloParams {
    version: u64,
    rows: u16,
    cols: u16,
}

struct Hello {
    stream: UnixStream,
    decoder: rpc::Decoder,
    /// The frontend's size, once its hello arrived.
    size: Option<(u16, u16)>,
    notice: Instant,
}

/// A goblin's attach socket and, with attach handlers, its attachment.
pub(super) struct Attach {
    path: PathBuf,
    listener: UnixListener,
    hellos: Vec<Hello>,
    client: Option<Client>,
}

enum Client {
    /// Until the variables the client needs are ready.
    Waiting {
        stream: UnixStream,
        size: (u16, u16),
        deadline: Instant,
        notice: Instant,
    },
    /// The client was sent to init, which has not answered yet.
    Starting {
        stream: UnixStream,
        size: (u16, u16),
        exec: u64,
    },
    Running(Box<Running>),
}

struct Running {
    exec: u64,
    pid: i32,
    /// When the client started, in Unix milliseconds.
    since: u64,
    terminal: Terminal,
    /// The client's command exited: the goblin is detached.
    exited: bool,
    ending: Option<Ending>,
}

/// A detach in progress: `onDetach`, then the client's cancellation.
struct Ending {
    on_detach: Option<u64>,
    deadline: Instant,
    cancelled: bool,
}

/// The output of an agent without a terminal, kept in a bounded log.
pub(super) struct ServerLog {
    pipe: Option<File>,
    path: PathBuf,
    file: File,
    size: u64,
}

fn frame(stream: &mut UnixStream, value: &Value) -> bool {
    rpc::encode(value).is_ok_and(|bytes| stream.write_all(&bytes).is_ok())
}

fn refuse(stream: &mut UnixStream, message: &str) {
    frame(stream, &rpc::error(json!(0), -32009, message));
}

fn waiting(stream: &mut UnixStream, missing: &[String]) -> bool {
    frame(
        stream,
        &json!({"jsonrpc":"2.0","method":"attach.waiting","params":{"missing":missing}}),
    )
}

/// The frontend closed its end.
fn gone(stream: &UnixStream) -> bool {
    let mut state = libc::pollfd {
        fd: stream.as_raw_fd(),
        events: libc::POLLRDHUP,
        revents: 0,
    };
    unsafe { libc::poll(&mut state, 1, 0) };
    state.revents & (libc::POLLRDHUP | libc::POLLHUP | libc::POLLERR) != 0
}

impl Attach {
    pub(super) fn new(path: &Path) -> crate::Result<Self> {
        let listener = UnixListener::bind(path)?;
        listener.set_nonblocking(true)?;
        fs::set_permissions(path, fs::Permissions::from_mode(0o600))?;
        Ok(Self {
            path: path.into(),
            listener,
            hellos: vec![],
            client: None,
        })
    }
    fn running(&self) -> Option<&Running> {
        match &self.client {
            Some(Client::Running(running)) => Some(running),
            _ => None,
        }
    }
    /// A client runs for an attached frontend and no detach has begun.
    pub(super) fn attached(&self) -> bool {
        self.running()
            .is_some_and(|r| r.ending.is_none() && !r.exited)
    }
    /// No client runs: none started, or init reported it exited.
    pub(super) fn detached(&self) -> bool {
        match &self.client {
            Some(Client::Running(running)) => running.exited,
            _ => true,
        }
    }
    /// Nothing is left to deliver to a frontend: no client, or one that
    /// exited and whose terminal may go.
    fn can_evict(&self) -> bool {
        match &self.client {
            None => true,
            Some(Client::Running(running)) => running.exited && running.terminal.can_evict(),
            Some(_) => false,
        }
    }
    /// When the attached client started.
    pub(super) fn since(&self) -> Option<u64> {
        self.running().filter(|_| self.attached()).map(|r| r.since)
    }
    pub(super) fn resize(&mut self, rows: u16, cols: u16) -> crate::Result<()> {
        match &mut self.client {
            Some(Client::Running(running)) => running.terminal.resize(rows, cols),
            _ => Err("no client is attached".into()),
        }
    }
    /// Accept frontends and read their hellos.
    fn accept(&mut self) {
        for _ in 0..4 {
            match self.listener.accept() {
                Ok((stream, _)) if self.hellos.len() < HELLOS => {
                    if stream.set_nonblocking(true).is_ok() {
                        let now = Instant::now();
                        self.hellos.push(Hello {
                            stream,
                            decoder: rpc::Decoder::new(4096, Some(now)),
                            size: None,
                            notice: now,
                        });
                    }
                }
                Ok(_) => (),
                Err(_) => break,
            }
        }
        let now = Instant::now();
        self.hellos.retain_mut(|hello| {
            if hello.size.is_some() {
                return !gone(&hello.stream);
            }
            // Never read past the hello: nothing after it is consumed here.
            let mut bytes = [0; 4096];
            let parsed = loop {
                let room = hello.decoder.remaining().min(bytes.len());
                let read = match hello.stream.read(&mut bytes[..room]) {
                    Ok(0) => return false,
                    Ok(n) => n,
                    Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                    Err(e) if e.kind() == io::ErrorKind::WouldBlock => 0,
                    Err(_) => return false,
                };
                match hello.decoder.push(&bytes[..read], now) {
                    Ok((_, Some(body))) => {
                        break rpc::call(&body)
                            .ok()
                            .filter(|call| call.method == "attach.hello" && call.id.is_none())
                            .and_then(|call| {
                                serde_json::from_value::<HelloParams>(call.params).ok()
                            })
                            .ok_or("invalid attach.hello");
                    }
                    Ok(_) if read > 0 => (),
                    Ok(_) if !hello.decoder.expired(now) => return true,
                    Ok(_) => break Err("no attach.hello in time"),
                    Err(_) => break Err("invalid attach.hello"),
                }
            };
            match parsed {
                Ok(p) if p.version == VERSION && p.rows > 0 && p.cols > 0 => {
                    hello.size = Some((p.rows, p.cols));
                    true
                }
                Ok(_) => {
                    refuse(&mut hello.stream, "unsupported attach.hello version");
                    false
                }
                Err(message) => {
                    refuse(&mut hello.stream, message);
                    false
                }
            }
        });
    }
}

impl Drop for Attach {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.path);
    }
}

impl ServerLog {
    pub(super) fn new(pipe: OwnedFd, path: PathBuf) -> crate::Result<Self> {
        unix::nonblocking(pipe.as_raw_fd())?;
        Ok(Self {
            pipe: Some(File::from(pipe)),
            file: Self::create(&path)?,
            path,
            size: 0,
        })
    }
    fn create(path: &Path) -> io::Result<File> {
        fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(path)
    }
    pub(super) fn path(&self) -> &Path {
        &self.path
    }
    /// Move what the agent wrote into the log, without blocking.
    pub(super) fn tick(&mut self) {
        let mut bytes = [0; 8192];
        for _ in 0..8 {
            let Some(pipe) = &mut self.pipe else {
                return;
            };
            let n = match pipe.read(&mut bytes) {
                Ok(0) => {
                    self.pipe = None;
                    return;
                }
                Ok(n) => n,
                Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => return,
                Err(_) => {
                    self.pipe = None;
                    return;
                }
            };
            if self.size + n as u64 > LOG {
                let mut old = self.path.clone().into_os_string();
                old.push(".1");
                let _ = fs::rename(&self.path, old);
                match Self::create(&self.path) {
                    Ok(file) => self.file = file,
                    Err(_) => continue,
                }
                self.size = 0;
            }
            if self.file.write_all(&bytes[..n]).is_ok() {
                self.size += n as u64;
            }
        }
    }
}

/// Remove an evicted goblin's sockets and server log. Its directory goes
/// once nothing else is left in it.
pub(super) fn remove_files(directory: &Path) {
    for name in ["terminal.sock", "attach.sock", "server.log", "server.log.1"] {
        let _ = fs::remove_file(directory.join(name));
    }
}

impl Active {
    /// A stopped goblin's record may go: its terminal, or its attached
    /// client's, has nothing left to deliver.
    pub(super) fn can_evict(&self) -> bool {
        self.terminal.can_evict() && self.attach.can_evict()
    }
    /// The agent started without a terminal: its output goes to the server
    /// log, and its own terminal refuses older frontends.
    pub(super) fn serve(
        &mut self,
        output: OwnedFd,
        handlers: config::Handlers,
    ) -> crate::Result<()> {
        let path = self.directory.join("server.log");
        self.server_log = Some(ServerLog::new(output, path.clone())?);
        self.record.server_log = Some(path.display().to_string());
        self.terminal.refuse(UPGRADE);
        self.terminal.detached = true;
        self.handlers = Some(handlers);
        Ok(())
    }
    /// The variables `needs` names that are not ready: set to a value during
    /// this run.
    fn missing(&self, needs: &[String]) -> Vec<String> {
        needs
            .iter()
            .filter(|name| {
                !(self.fresh.contains(*name)
                    && self.variables.get(*name).is_some_and(|v| !v.is_empty()))
            })
            .cloned()
            .collect()
    }
    /// Ask init to run a handler as exec `id`. Whether it started comes
    /// back as a process event.
    fn spawn(&mut self, id: u64, argv: Vec<String>, stdio: Stdio) -> Result<(), String> {
        if !self.init {
            return Err("the goblin has no init channel".into());
        }
        let env = self
            .variables
            .iter()
            .map(|(k, v)| (format!("GOBLIN_VAR_{}", k.to_uppercase()), v.clone()))
            .collect();
        let runtime = self.runtime.as_mut().ok_or("the goblin has stopped")?;
        runtime
            .send(Command::Spawn {
                id,
                kind: SpawnKind::Attach,
                argv: Some(argv),
                cwd: None,
                env,
                stdio,
            })
            .map_err(|_| "the goblin is busy".to_string())
    }
    /// SIGHUP, SIGTERM and SIGCONT to everything exec `id` started, then
    /// SIGKILL after init's grace period.
    fn cancel(&mut self, id: u64) {
        if self.init
            && let Some(runtime) = &mut self.runtime
        {
            let _ = runtime.send(Command::CancelProcess {
                id,
                everything: true,
            });
        }
    }
    /// Start the client for a frontend whose variables are ready, on a PTY
    /// of its own.
    fn start_client(&mut self, id: u64, size: (u16, u16)) -> Result<(), String> {
        self.allow(&Operation::Attach)
            .map_err(|(_, message)| message)?;
        let handlers = self.handlers.as_ref().ok_or("no attach handlers")?;
        let argv = config::handler_arguments(&handlers.on_attach.command, &self.variables, None)?;
        self.spawn(
            id,
            argv,
            Stdio::NewPty {
                rows: size.0,
                cols: size.1,
            },
        )
    }
    /// Begin a detach: run `onDetach` when asked and set, then cancel the
    /// client. `id` is the exec ID `onDetach` would use.
    fn end_client(&mut self, id: u64, on_detach: bool) {
        let Some(Client::Running(running)) = &self.attach.client else {
            return;
        };
        if running.ending.is_some() {
            return;
        }
        let (exec, pid) = (running.exec, running.pid);
        let command = self
            .handlers
            .as_ref()
            .and_then(|h| h.on_detach.as_ref())
            .filter(|_| on_detach && !running.exited);
        let argv = command.and_then(|command| {
            config::handler_arguments(command, &self.variables, Some(pid)).ok()
        });
        let on_detach = argv.and_then(|argv| {
            let null = || {
                File::options()
                    .read(true)
                    .write(true)
                    .open("/dev/null")
                    .map(|file| OwnedFd::from(file).into())
            };
            let stdio = Stdio::Fds([null().ok()?, null().ok()?, null().ok()?]);
            self.spawn(id, argv, stdio).ok().map(|()| id)
        });
        if on_detach.is_none() {
            self.cancel(exec);
        }
        if let Some(Client::Running(running)) = &mut self.attach.client {
            running.ending = Some(Ending {
                on_detach,
                deadline: Instant::now() + ON_DETACH,
                cancelled: on_detach.is_none(),
            });
        }
    }
}

impl Controller {
    /// Advance goblin `id`'s attach socket and attachment. `a` is out of the
    /// session map for the tick.
    pub(super) fn attach_tick(&mut self, a: &mut Active) {
        a.attach.accept();
        if let Some(log) = &mut a.server_log {
            log.tick();
        }
        let now = Instant::now();
        match a.record.state.as_str() {
            "running" => (),
            "starting" => {
                for hello in a.attach.hellos.iter_mut().filter(|h| h.size.is_some()) {
                    if now >= hello.notice {
                        hello.notice = now + NOTICE;
                        waiting(&mut hello.stream, &[]);
                    }
                }
            }
            _ => {
                let mut message = a
                    .record
                    .detail
                    .clone()
                    .unwrap_or_else(|| "the goblin has stopped".into());
                // Why it stopped is in its server log.
                if let Some(log) = &a.server_log {
                    message = format!("{message}; its server log is {}", log.path().display());
                }
                for mut hello in a.attach.hellos.drain(..) {
                    refuse(&mut hello.stream, &message);
                }
                match a.attach.client.take() {
                    Some(Client::Waiting { mut stream, .. })
                    | Some(Client::Starting { mut stream, .. }) => refuse(&mut stream, &message),
                    Some(Client::Running(mut running)) => {
                        running.exited = true;
                        if running.terminal.connected() {
                            a.attach.client = Some(Client::Running(running));
                        }
                    }
                    None => (),
                }
            }
        }
        if a.record.state == "running" {
            let (ready, pending) = a
                .attach
                .hellos
                .drain(..)
                .partition::<Vec<_>, _>(|h| h.size.is_some());
            a.attach.hellos = pending;
            for mut hello in ready {
                let size = hello.size.unwrap();
                match &a.handlers {
                    None => {
                        let attached = hello
                            .stream
                            .try_clone()
                            .map_err(|e| e.to_string())
                            .and_then(|writer| {
                                a.terminal
                                    .attach(writer, json!(0))
                                    .map_err(|e| e.to_string())
                            });
                        match attached {
                            Ok(()) => {
                                let _ = a.terminal.resize(size.0, size.1);
                            }
                            Err(message) => refuse(&mut hello.stream, &message),
                        }
                    }
                    Some(_) if a.attach.client.is_some() => {
                        refuse(&mut hello.stream, "terminal is already attached")
                    }
                    Some(handlers) => {
                        let timeout = Duration::from_secs(handlers.on_attach.timeout);
                        a.attach.client = Some(Client::Waiting {
                            stream: hello.stream,
                            size,
                            deadline: now + timeout,
                            notice: now,
                        });
                    }
                }
            }
        }
        self.client_tick(a, now);
        let (attached, detached) = match a.handlers {
            Some(_) => (a.attach.attached(), a.attach.detached()),
            None => (a.terminal.attached(), a.terminal.detached),
        };
        if (attached, detached) != (a.record.terminal_attached, a.record.terminal_detached) {
            a.record.terminal_attached = attached;
            a.record.terminal_detached = detached;
            self.dirty = true;
        }
    }
    fn client_tick(&mut self, a: &mut Active, now: Instant) {
        match a.attach.client.take() {
            None => (),
            Some(Client::Waiting {
                mut stream,
                size,
                deadline,
                notice,
            }) => {
                if gone(&stream) {
                    return;
                }
                let needs = a
                    .handlers
                    .as_ref()
                    .map(|h| h.on_attach.needs())
                    .unwrap_or_default();
                let missing = a.missing(&needs);
                if missing.is_empty() {
                    let exec = self.number();
                    match a.start_client(exec, size) {
                        Ok(()) => a.attach.client = Some(Client::Starting { stream, size, exec }),
                        Err(message) => refuse(
                            &mut stream,
                            &format!("cannot start the attach client: {message}"),
                        ),
                    }
                } else if now >= deadline {
                    let log = a
                        .server_log
                        .as_ref()
                        .map(|l| l.path().display().to_string())
                        .unwrap_or_default();
                    refuse(
                        &mut stream,
                        &format!(
                            "the goblin did not set {} in time; its server log is {log}; restart the goblin",
                            missing.join(", ")
                        ),
                    );
                } else if now < notice || waiting(&mut stream, &missing) {
                    a.attach.client = Some(Client::Waiting {
                        stream,
                        size,
                        deadline,
                        notice: if now < notice { notice } else { now + NOTICE },
                    });
                }
            }
            Some(client @ Client::Starting { .. }) => a.attach.client = Some(client),
            Some(Client::Running(mut running)) => {
                if let Err(e) = running.terminal.tick() {
                    a.record.detail = Some(super::bounded(&e.to_string()));
                }
                let left = !running.terminal.connected();
                if running.exited && left {
                    self.dirty = true;
                    return;
                }
                let begin = left && running.ending.is_none() && !running.exited;
                let overdue = running
                    .ending
                    .as_ref()
                    .is_some_and(|e| !e.cancelled && (e.on_detach.is_none() || now >= e.deadline));
                a.attach.client = Some(Client::Running(running));
                if begin {
                    let id = self.number();
                    a.end_client(id, true);
                } else if overdue && let Some(Client::Running(running)) = &mut a.attach.client {
                    let ending = running.ending.as_mut().unwrap();
                    ending.cancelled = true;
                    let (exec, on_detach) = (running.exec, ending.on_detach);
                    if let Some(on_detach) = on_detach {
                        a.cancel(on_detach);
                    }
                    a.cancel(exec);
                }
            }
        }
    }
    /// A process event for an attach client or its `onDetach`; given back
    /// when it is for another exec.
    pub(super) fn attach_event(&mut self, session: &str, event: Event) -> Option<Event> {
        let Some(a) = self.sessions.get_mut(session) else {
            return Some(event);
        };
        let unconsumed = match (a.attach.client.take(), event) {
            (
                Some(Client::Starting { stream, size, exec }),
                Event::Spawned {
                    id,
                    pid,
                    master: Some(master),
                },
            ) if id == exec => {
                let started =
                    Terminal::client(master.into(), size.0, size.1).and_then(|mut terminal| {
                        terminal.attach(stream, json!(0))?;
                        Ok(terminal)
                    });
                match started {
                    Ok(terminal) => {
                        a.attach.client = Some(Client::Running(Box::new(Running {
                            exec,
                            pid,
                            since: crate::messaging::now(),
                            terminal,
                            exited: false,
                            ending: None,
                        })))
                    }
                    Err(_) => a.cancel(exec),
                }
                None
            }
            (
                Some(Client::Starting {
                    mut stream, exec, ..
                }),
                Event::SpawnFailed { id, message, .. },
            ) if id == exec => {
                refuse(
                    &mut stream,
                    &format!("cannot start the attach client: {message}"),
                );
                None
            }
            (Some(Client::Running(mut running)), event) => {
                let on_detach = running.ending.as_ref().and_then(|e| e.on_detach);
                let unconsumed = match event {
                    // Detached; what the client left behind goes too, and
                    // so does an onDetach still running.
                    Event::Exited { id, .. } if id == running.exec => {
                        running.exited = true;
                        if !running.ending.as_ref().is_some_and(|e| e.cancelled) {
                            if let Some(on_detach) = on_detach {
                                a.cancel(on_detach);
                            }
                            a.cancel(id);
                            running.ending = Some(Ending {
                                on_detach: None,
                                deadline: Instant::now(),
                                cancelled: true,
                            });
                        }
                        None
                    }
                    Event::Exited { id, .. } | Event::SpawnFailed { id, .. }
                        if on_detach == Some(id) =>
                    {
                        // onDetach finished: the next tick cancels the client.
                        running.ending.as_mut().unwrap().on_detach = None;
                        None
                    }
                    Event::Spawned { id, .. } if on_detach == Some(id) => None,
                    event => Some(event),
                };
                a.attach.client = Some(Client::Running(running));
                unconsumed
            }
            (client, event) => {
                a.attach.client = client;
                Some(event)
            }
        };
        if unconsumed.is_none() {
            self.dirty = true;
        }
        unconsumed
    }
    /// The goblin's init went away: no client is left, and none can start.
    pub(super) fn attach_lost(&mut self, session: &str) {
        let Some(a) = self.sessions.get_mut(session) else {
            return;
        };
        match a.attach.client.take() {
            Some(Client::Running(mut running)) => {
                running.exited = true;
                a.attach.client = Some(Client::Running(running));
            }
            Some(Client::Waiting { mut stream, .. })
            | Some(Client::Starting { mut stream, .. }) => {
                refuse(&mut stream, "the goblin's init has gone away");
            }
            None => return,
        }
        self.dirty = true;
    }
    /// `sessions.detach` for a goblin with attach handlers.
    pub(super) fn detach_client(&mut self, session: &str) -> Result<(), String> {
        let id = self.number();
        let a = self.sessions.get_mut(session).ok_or("no such goblin")?;
        if !a.attach.attached() {
            return Err("terminal is not attached".into());
        }
        a.end_client(id, true);
        if let Some(Client::Running(running)) = &mut a.attach.client {
            let _ = running.terminal.detach();
        }
        a.record.terminal_attached = false;
        self.dirty = true;
        Ok(())
    }
}
