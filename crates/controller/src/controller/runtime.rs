//! The daemon's handle to one goblin's runtime: commands go in, events come
//! out, and neither side waits for the other.
use crate::{
    session::{Cancel, Identity, Inheritance},
    unix,
    worker::{Auto, Endpoints, Recreated, Source, Worker},
};
use goblins_protocol::{Reply, Request, init};
use serde::Deserialize;
use std::{
    collections::{BTreeMap, VecDeque},
    fs::{self, File},
    io,
    os::{
        fd::{AsRawFd, OwnedFd, RawFd},
        unix::{net::UnixListener, process::CommandExt},
    },
    path::{Path, PathBuf},
    process::{Child, Stdio as ChildStdio},
    sync::mpsc,
};

/// Output kept per host job; the rest is discarded with a note.
const LOG_LIMIT: u64 = 16 * 1024 * 1024;
/// Finished host jobs whose output stays readable per sandbox.
const KEPT: usize = 8;

/// The daemon's view of one goblin's runtime. Neither method blocks.
pub(crate) trait Runtime: Send {
    /// Err gives the command back when the runtime's queue is full.
    /// Cancellations, process signals and `Stop` are never refused.
    fn send(&mut self, command: Command) -> Result<(), Command>;
    /// The events of one round, then None; the daemon drains a round per
    /// tick, which bounds what one goblin costs it.
    fn poll(&mut self) -> Option<Event>;
    /// True once every event has been polled and the runtime is gone.
    fn ended(&self) -> bool;
}

/// A descriptor inside a command or an event. In-process it moves.
#[derive(Debug)]
pub(crate) struct Fd(OwnedFd);

impl From<OwnedFd> for Fd {
    fn from(fd: OwnedFd) -> Self {
        Self(fd)
    }
}

impl From<Fd> for OwnedFd {
    fn from(fd: Fd) -> Self {
        fd.0
    }
}

impl AsRawFd for Fd {
    fn as_raw_fd(&self) -> RawFd {
        self.0.as_raw_fd()
    }
}

/// A process's terminal or standard streams.
#[derive(Debug)]
pub(crate) enum Stdio {
    /// Frontend-provided stdin, stdout and stderr (`goblins exec`).
    Fds([Fd; 3]),
    /// A frontend-provided PTY slave (`goblins exec` with tty).
    Tty(Fd),
    /// A new PTY the runtime allocates; its master comes back in `Spawned`.
    NewPty { rows: u16, cols: u16 },
}

/// What the daemon asks of a runtime.
#[derive(Debug)]
pub(crate) enum Command {
    /// Resolve a package for its approval preview; cancelled by `approval`.
    Preview {
        approval: u64,
        package: String,
    },
    Decide {
        request: Request,
        approved: bool,
        output: Option<PathBuf>,
    },
    /// Evaluate a dev shell refresh candidate for approval; cancelled by
    /// `approval`.
    Refresh {
        approval: u64,
        changes: crate::devshell::Changes,
    },
    /// Build and mount a flake app from the trusted generation; cancelled by
    /// `serial`.
    Run {
        serial: u64,
        app: String,
    },
    /// Evaluate a refresh candidate for its preview only; nothing is stored.
    /// Cancelled by `serial`.
    Diff {
        serial: u64,
        changes: crate::devshell::Changes,
    },
    /// Cancel the work with this approval or serial, queued or running.
    Cancel(u64),
    /// Start process `id` in the running sandbox through goblins-init. The
    /// daemon allocates ids; `argv` None runs the sandbox's shell.
    Spawn {
        id: u64,
        argv: Option<Vec<String>>,
        cwd: Option<String>,
        env: Vec<(String, String)>,
        stdio: Stdio,
    },
    /// Forward a signal to process `id`'s group.
    Signal {
        id: u64,
        signal: i32,
    },
    /// End process `id`'s group, or with `everything` all it started.
    CancelProcess {
        id: u64,
        everything: bool,
    },
    /// End every process started with `Spawn`.
    KillAll,
    /// Resolve and pin the host directory a host command would start in,
    /// sandbox path `cwd`, for its approval.
    PrepareHostJob {
        request: String,
        cwd: PathBuf,
    },
    /// Run an approved host command in the directory its request pinned.
    RunHostJob {
        request: String,
        argv: Vec<String>,
        env: Vec<(String, String)>,
        timeout: Option<u64>,
    },
    /// Release a prepared directory, or stop a running job.
    CancelHostJob {
        request: String,
    },
    Stop,
}

impl Command {
    /// The id a `Cancel` names this command by, when it can be cancelled.
    fn cancel_id(&self) -> Option<u64> {
        match self {
            Command::Preview { approval, .. } | Command::Refresh { approval, .. } => {
                Some(*approval)
            }
            Command::Run { serial, .. } | Command::Diff { serial, .. } => Some(*serial),
            _ => None,
        }
    }
}

/// What a runtime reports.
pub(crate) enum Event {
    Preview {
        approval: u64,
        result: std::result::Result<crate::catalog::Preview, String>,
    },
    Started {
        docker_enabled: bool,
        scope: Option<String>,
        dev_shell: Option<String>,
        dev_identity: Option<String>,
        initial_packages: Vec<String>,
        /// The agent's PTY, or with attach handlers its output pipe.
        master: OwnedFd,
        handlers: Option<Box<crate::config::Handlers>>,
        identity: Identity,
        inheritance: Inheritance,
        recreated: Option<Box<Recreated>>,
        auto: Box<Auto>,
    },
    Progress {
        request: String,
        state: &'static str,
    },
    /// Whether the agent will run without a terminal, known before the
    /// launch's slow work.
    Mode {
        server: bool,
    },
    /// Structured approval preview and candidate identity, or why it cannot
    /// refresh.
    Refresh {
        approval: u64,
        result: std::result::Result<(serde_json::Value, String), String>,
    },
    /// The reply to a sandbox call waiting on the runtime (`Command::Run`,
    /// `Command::Diff`), or why it failed.
    Call {
        serial: u64,
        result: std::result::Result<serde_json::Value, String>,
    },
    /// A refresh became the current generation; members it runs later start in it.
    DevShell {
        identity: String,
        inheritance: Inheritance,
    },
    Granted {
        reply: Reply,
        detail: Option<String>,
        output: Option<PathBuf>,
    },
    /// A host launch's manifest validated; `sequence` orders acceptances.
    Manifest {
        sequence: u64,
        lease: std::sync::Arc<crate::config::ManifestLease>,
    },
    /// Process `id` started; `master` is the PTY `Stdio::NewPty` asked for.
    Spawned {
        id: u64,
        pid: i32,
        master: Option<Fd>,
    },
    /// Process `id` did not start. `code` is what a shell would report, as
    /// init gives it; None when init could not be asked.
    SpawnFailed {
        id: u64,
        code: Option<i32>,
        message: String,
    },
    Exited {
        id: u64,
        code: Option<i32>,
        signal: Option<i32>,
    },
    /// The channel to the sandbox's goblins-init closed: no process can
    /// start, and none reports any more.
    InitLost,
    /// The payload exited. The daemon decides what follows.
    PayloadExited,
    /// A connection to the sandbox's `request.sock`, for the daemon to serve.
    SandboxConnected(Fd),
    /// The host directory a host command would start in, as approval shows
    /// it, or why it cannot be used.
    HostJobPrepared {
        request: String,
        result: Result<PathBuf, String>,
    },
    /// An approved host command could not be started.
    HostJobFailed {
        request: String,
        error: String,
    },
    /// A host job's supervisor exited: the outcome it recorded, or without
    /// one, its diagnostic.
    HostJobFinished {
        request: String,
        outcome: Result<HostOutcome, String>,
    },
    Failed(String),
    Stopped {
        exit_code: Option<i32>,
    },
}

/// What a host job's supervisor records in its `status.json`.
#[derive(Debug, Deserialize, Default)]
pub(crate) struct HostOutcome {
    pub exit_code: Option<i32>,
    pub signal: Option<i32>,
    pub spawn_error: Option<String>,
    pub cancelled: bool,
    pub timed_out: bool,
    pub truncated: bool,
}

/// A running host job.
struct HostJob {
    request: String,
    child: Child,
    /// Dropping it stops the job.
    lifetime: Option<OwnedFd>,
    directory: PathBuf,
}

/// The runtime inside the daemon: a worker thread for Nix-bound work, with
/// `send` as the dispatcher that never waits for it, and the running
/// sandbox's init channel, request socket, exit watch and host jobs.
pub(crate) struct Local {
    worker: Worker,
    /// Tokens of cancellable work sent to the worker, by cancel id.
    cancels: BTreeMap<u64, Cancel>,
    /// The private channel to the sandbox's goblins-init, from `Started`.
    init: Option<OwnedFd>,
    /// The sandbox's `request.sock`, from `Started` until it stops.
    listener: Option<UnixListener>,
    /// The helper's pidfd, until the payload's exit is reported.
    exit_watch: Option<OwnedFd>,
    /// PTYs allocated for processes init has not reported started.
    masters: BTreeMap<u64, OwnedFd>,
    buffer: Vec<u8>,
    /// The goblin's capability, from `Started`: it resolves host
    /// directories through the sandbox's binds.
    inheritance: Option<Inheritance>,
    /// Where host jobs keep their output and outcome.
    host_jobs: PathBuf,
    /// Pinned directories of host commands awaiting approval, by request.
    prepared: BTreeMap<String, File>,
    jobs: Vec<HostJob>,
    stopping: bool,
    /// Events reported and not yet polled.
    events: VecDeque<Event>,
    /// A round is being polled: its sources were collected.
    polling: bool,
    ended: bool,
}

impl Local {
    pub fn start(source: Source, directory: PathBuf, dimensions: (u16, u16)) -> Self {
        let host_jobs = directory.join("host-jobs");
        Self::new(Worker::start(source, directory, dimensions), host_jobs)
    }
    fn new(worker: Worker, host_jobs: PathBuf) -> Self {
        Self {
            worker,
            inheritance: None,
            host_jobs,
            prepared: BTreeMap::new(),
            jobs: vec![],
            cancels: BTreeMap::new(),
            init: None,
            listener: None,
            exit_watch: None,
            masters: BTreeMap::new(),
            buffer: vec![],
            stopping: false,
            events: VecDeque::new(),
            polling: false,
            ended: false,
        }
    }
    /// Report an event in the current round, or the next.
    fn report(&mut self, event: Event) {
        self.events.push_back(event);
    }
    /// Collect a round: worker results, init's events, new request
    /// connections and the payload's exit.
    fn collect(&mut self) {
        for _ in 0..16 {
            match self.worker.results.try_recv() {
                Ok(event) => {
                    match &event {
                        // Finished work no longer needs its token.
                        Event::Preview { approval: id, .. }
                        | Event::Refresh { approval: id, .. }
                        | Event::Call { serial: id, .. } => {
                            self.cancels.remove(id);
                        }
                        Event::DevShell { inheritance, .. } => {
                            self.inheritance = Some(inheritance.clone());
                        }
                        // The worker hands the endpoints over just before.
                        Event::Started { inheritance, .. } => {
                            self.inheritance = Some(inheritance.clone());
                            if let Ok(endpoints) = self.worker.endpoints.try_recv() {
                                let Endpoints {
                                    control,
                                    listener,
                                    exit_watch,
                                } = *endpoints;
                                self.init = Some(control);
                                self.exit_watch = Some(exit_watch);
                                self.listener = (!self.stopping).then_some(listener);
                            }
                        }
                        Event::Stopped { .. } => self.listener = None,
                        _ => (),
                    }
                    self.report(event);
                }
                Err(mpsc::TryRecvError::Empty) => break,
                // Completion is usable only after observing the channel
                // disconnected and empty. Checking thread completion after
                // an empty read could otherwise discard a final result sent
                // between those two observations.
                Err(mpsc::TryRecvError::Disconnected) => {
                    self.ended = self.worker.finished();
                    break;
                }
            }
        }
        self.receive_init();
        for _ in 0..8 {
            let Some(listener) = &self.listener else {
                break;
            };
            match listener.accept() {
                Ok((stream, _)) => {
                    self.report(Event::SandboxConnected(OwnedFd::from(stream).into()))
                }
                Err(e) if e.kind() == io::ErrorKind::Interrupted => (),
                Err(_) => break,
            }
        }
        self.finish_host_jobs();
        // Observe process exit independently of a worker blocked on Nix.
        // A pidfd pins the process identity, avoiding PID-reuse races.
        if self
            .exit_watch
            .as_ref()
            .is_some_and(|fd| unix::readable(fd.as_raw_fd(), 0).unwrap_or(true))
        {
            self.exit_watch = None;
            self.report(Event::PayloadExited);
        }
    }
    /// Read init's events without blocking.
    fn receive_init(&mut self) {
        if self.init.is_some() && self.buffer.is_empty() {
            self.buffer = vec![0; init::MAX_MESSAGE];
        }
        for _ in 0..256 {
            let Some(socket) = &self.init else {
                return;
            };
            match init::receive(socket.as_raw_fd(), &mut self.buffer, libc::MSG_DONTWAIT) {
                // Init never sends descriptors; any that arrive are closed.
                Ok((n, _)) if n > 0 => {
                    let Ok(event) = serde_json::from_slice::<init::Event>(&self.buffer[..n]) else {
                        continue;
                    };
                    let event = match event {
                        init::Event::Spawned { id, pid } => Event::Spawned {
                            id,
                            pid,
                            master: self.masters.remove(&id).map(Fd::from),
                        },
                        init::Event::Failed { id, code, message } => {
                            self.masters.remove(&id);
                            Event::SpawnFailed {
                                id,
                                code: Some(code),
                                message,
                            }
                        }
                        init::Event::Exited { id, code, signal } => {
                            Event::Exited { id, code, signal }
                        }
                    };
                    self.report(event);
                }
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => return,
                Ok(_) | Err(_) => {
                    self.init = None;
                    self.masters.clear();
                    self.report(Event::InitLost);
                    return;
                }
            }
        }
    }
    /// Send `command` to init with `fds` attached.
    fn to_init(&self, command: &init::Command, fds: &[RawFd]) -> Result<(), String> {
        let socket = self.init.as_ref().ok_or("the goblin has no init channel")?;
        let bytes = serde_json::to_vec(command).map_err(|e| e.to_string())?;
        init::send(socket.as_raw_fd(), &bytes, fds)
            .map_err(|e| format!("cannot reach the sandbox's init: {e}"))
    }
    fn spawn(
        &mut self,
        id: u64,
        argv: Option<Vec<String>>,
        cwd: Option<String>,
        env: Vec<(String, String)>,
        stdio: Stdio,
    ) {
        let (tty, fds, master): (_, Vec<OwnedFd>, _) = match stdio {
            Stdio::Fds(fds) => (false, fds.into_iter().map(OwnedFd::from).collect(), None),
            Stdio::Tty(slave) => (true, vec![slave.into()], None),
            Stdio::NewPty { rows, cols } => match unix::pty(rows, cols) {
                Ok((master, slave)) => (true, vec![slave], Some(master)),
                Err(error) => {
                    return self.report(Event::SpawnFailed {
                        id,
                        code: None,
                        message: error.to_string(),
                    });
                }
            },
        };
        let command = init::Command::Spawn {
            id,
            argv,
            cwd,
            env,
            tty,
        };
        let raw: Vec<_> = fds.iter().map(AsRawFd::as_raw_fd).collect();
        match self.to_init(&command, &raw) {
            Ok(()) => {
                if let Some(master) = master {
                    self.masters.insert(id, master);
                }
            }
            Err(message) => self.report(Event::SpawnFailed {
                id,
                code: None,
                message,
            }),
        }
    }
}

impl Local {
    /// Start an approved host job under the helper's `--host-job`
    /// supervisor, in the directory its request pinned.
    fn run_host_job(
        &mut self,
        request: &str,
        argv: &[String],
        env: &[(String, String)],
        timeout: Option<u64>,
    ) -> crate::Result<()> {
        let cwd = self
            .prepared
            .remove(request)
            .ok_or("the host command's directory was not prepared")?;
        let helper = self
            .inheritance
            .as_ref()
            .ok_or("the goblin has stopped")?
            .helper()
            .to_path_buf();
        prune(&self.host_jobs);
        let directory = self.host_jobs.join(request);
        fs::create_dir(&directory)?;
        let (lifetime_read, lifetime_write) = unix::pipe()?;
        let mut command = std::process::Command::new(helper);
        command
            .arg("--host-job")
            .arg(directory.join("output.log"))
            .arg(directory.join("status.json"))
            .arg(LOG_LIMIT.to_string())
            .arg(timeout.unwrap_or(0).to_string());
        // The request's variables reach the command only, not the supervisor.
        for (key, value) in env {
            command.args(["--env", key, value]);
        }
        command
            .arg("--")
            .args(argv)
            // Temporary files go straight to /tmp unless the request says
            // otherwise: nested daemons and nix develop put sockets beneath
            // TMPDIR, and Unix socket paths are limited to 107 bytes.
            .env_remove("TMPDIR")
            .env("GOBLINS_HOST_JOB", request)
            .stdin(ChildStdio::from(lifetime_read))
            .stdout(ChildStdio::null())
            .stderr(File::create(directory.join("supervisor.log"))?);
        let cwd = cwd.as_raw_fd();
        let parent = unsafe { libc::getpid() };
        unsafe {
            command.pre_exec(move || {
                // Lifetime: the pipe closes with the daemon, but also stop
                // promptly if the daemon dies before reaching it.
                unix::cvt(libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGTERM))?;
                if libc::getppid() != parent {
                    return Err(std::io::Error::from_raw_os_error(libc::ESRCH));
                }
                unix::cvt(libc::fchdir(cwd))?;
                // Only stdio crosses: no daemon socket or session descriptor.
                unix::cvt(libc::syscall(libc::SYS_close_range, 3u32, u32::MAX, 4u32) as i32)?;
                Ok(())
            });
        }
        let child = command.spawn();
        drop(command);
        self.jobs.push(HostJob {
            request: request.to_string(),
            child: child?,
            lifetime: Some(lifetime_write),
            directory,
        });
        Ok(())
    }
    /// Report jobs whose supervisor has exited.
    fn finish_host_jobs(&mut self) {
        let mut index = 0;
        while index < self.jobs.len() {
            if matches!(self.jobs[index].child.try_wait(), Ok(None)) {
                index += 1;
                continue;
            }
            let mut job = self.jobs.remove(index);
            drop(job.lifetime.take());
            let _ = job.child.wait();
            let outcome = fs::read(job.directory.join("status.json"))
                .ok()
                .and_then(|bytes| serde_json::from_slice(&bytes).ok())
                .ok_or_else(|| crate::process::diagnostic(&job.directory.join("supervisor.log")));
            self.report(Event::HostJobFinished {
                request: job.request,
                outcome,
            });
        }
    }
}

/// Keep the output of the most recent finished jobs only.
fn prune(jobs: &Path) {
    let Ok(entries) = fs::read_dir(jobs) else {
        return;
    };
    let mut finished: Vec<_> = entries
        .filter_map(|e| e.ok())
        .filter(|e| e.path().join("status.json").exists())
        .filter_map(|e| Some((e.metadata().ok()?.modified().ok()?, e.path())))
        .collect();
    finished.sort();
    let excess = finished.len().saturating_sub(KEPT - 1);
    for (_, path) in finished.into_iter().take(excess) {
        let _ = fs::remove_dir_all(path);
    }
}

impl Runtime for Local {
    fn send(&mut self, command: Command) -> Result<(), Command> {
        match command {
            Command::Cancel(id) => {
                if let Some(cancel) = self.cancels.remove(&id) {
                    cancel.cancel();
                }
            }
            Command::Stop => {
                // Children of the worker's token see it cancelled too.
                self.worker.cancel.cancel();
                self.cancels.clear();
                self.stopping = true;
                self.listener = None;
            }
            Command::Spawn {
                id,
                argv,
                cwd,
                env,
                stdio,
            } => self.spawn(id, argv, cwd, env, stdio),
            // Best effort, like init's own delivery.
            Command::Signal { id, signal } => {
                let _ = self.to_init(&init::Command::Signal { id, signal }, &[]);
            }
            Command::CancelProcess { id, everything } => {
                let _ = self.to_init(&init::Command::Cancel { id, everything }, &[]);
            }
            Command::KillAll => {
                let _ = self.to_init(&init::Command::KillAll {}, &[]);
            }
            Command::PrepareHostJob { request, cwd } => {
                let result = match &self.inheritance {
                    Some(inheritance) => inheritance
                        .host_directory(&cwd)
                        .map(|(file, path)| {
                            self.prepared.insert(request.clone(), file);
                            path
                        })
                        .map_err(|e| e.to_string()),
                    None => Err("the goblin has stopped".into()),
                };
                self.report(Event::HostJobPrepared { request, result });
            }
            Command::RunHostJob {
                request,
                argv,
                env,
                timeout,
            } => {
                if let Err(error) = self.run_host_job(&request, &argv, &env, timeout) {
                    self.report(Event::HostJobFailed {
                        request,
                        error: error.to_string(),
                    });
                }
            }
            Command::CancelHostJob { request } => {
                self.prepared.remove(&request);
                if let Some(job) = self.jobs.iter_mut().find(|j| j.request == request) {
                    job.lifetime.take();
                }
            }
            command => {
                let id = command.cancel_id();
                let cancel = self.worker.cancel.child();
                match self.worker.commands.try_send((command, cancel.clone())) {
                    Ok(()) => {
                        if let Some(id) = id {
                            self.cancels.insert(id, cancel);
                        }
                    }
                    Err(error) => return Err(super::unsent(error).0),
                }
            }
        }
        Ok(())
    }
    fn poll(&mut self) -> Option<Event> {
        // Every round collects every source, behind what `send` reported.
        if !self.polling {
            self.collect();
            self.polling = true;
        }
        let event = self.events.pop_front();
        self.polling = event.is_some();
        event
    }
    fn ended(&self) -> bool {
        self.ended && self.events.is_empty() && self.jobs.is_empty()
    }
}

/// Commands a `Fake` runtime received, shared with the test.
#[cfg(test)]
pub(crate) type Received = std::sync::Arc<std::sync::Mutex<VecDeque<Command>>>;

/// A runtime for tests: it records the commands it is sent and replays the
/// events the test supplies, at most 16 a round.
#[cfg(test)]
pub(crate) struct Fake {
    received: Received,
    events: mpsc::Receiver<Event>,
    round: usize,
    ended: bool,
}

#[cfg(test)]
impl Fake {
    /// A runtime that reports `events` and then ends.
    pub fn ended(events: Vec<Event>) -> Self {
        let (fake, sender, _) = Self::new();
        for event in events {
            sender
                .try_send(event)
                .unwrap_or_else(|_| panic!("test event queue full"));
        }
        fake
    }
    /// A live runtime whose events the test supplies through the sender.
    pub fn new() -> (Self, mpsc::SyncSender<Event>, Received) {
        let (sender, events) = mpsc::sync_channel(16);
        let received = Received::default();
        let fake = Self {
            received: received.clone(),
            events,
            round: 0,
            ended: false,
        };
        (fake, sender, received)
    }
}

#[cfg(test)]
impl Runtime for Fake {
    fn send(&mut self, command: Command) -> Result<(), Command> {
        let mut received = self.received.lock().unwrap();
        // Requests are bounded like a worker's queue; nothing else is.
        let request = |c: &Command| {
            !matches!(
                c,
                Command::Cancel(_)
                    | Command::Signal { .. }
                    | Command::CancelProcess { .. }
                    | Command::KillAll
                    | Command::Stop
            )
        };
        if request(&command) && received.iter().filter(|c| request(c)).count() >= 4 {
            return Err(command);
        }
        received.push_back(command);
        Ok(())
    }
    fn poll(&mut self) -> Option<Event> {
        if self.round == 16 {
            self.round = 0;
            return None;
        }
        match self.events.try_recv() {
            Ok(event) => {
                self.round += 1;
                return Some(event);
            }
            Err(mpsc::TryRecvError::Empty) => (),
            Err(mpsc::TryRecvError::Disconnected) => self.ended = true,
        }
        self.round = 0;
        None
    }
    fn ended(&self) -> bool {
        self.ended
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A worker without a thread: the test receives its commands and sends
    /// its results.
    fn worker() -> (
        Worker,
        mpsc::Receiver<crate::worker::Work>,
        mpsc::SyncSender<Event>,
    ) {
        let (commands, received) = mpsc::sync_channel(4);
        let (sender, results) = mpsc::sync_channel(16);
        let (_, endpoints) = mpsc::sync_channel(1);
        let worker = Worker {
            cancel: Cancel::default(),
            commands,
            results,
            endpoints,
            thread: None,
        };
        (worker, received, sender)
    }

    #[test]
    fn final_result_after_empty_poll_is_not_discarded() {
        let (worker, _, sender) = worker();
        let mut local = Local::new(worker, PathBuf::from("/nonexistent"));
        // Even a finished producer observation cannot make an empty (still
        // connected) channel final. A last message may arrive after this poll.
        assert!(local.poll().is_none());
        assert!(!local.ended());
        sender
            .try_send(Event::Stopped { exit_code: Some(7) })
            .unwrap_or_else(|_| panic!("send"));
        drop(sender);
        assert!(matches!(
            local.poll(),
            Some(Event::Stopped { exit_code: Some(7) })
        ));
        assert!(local.poll().is_none());
        assert!(local.ended());
    }

    #[test]
    fn cancellation_reaches_queued_and_running_work_even_when_the_queue_is_full() {
        let (worker, received, _sender) = worker();
        let mut local = Local::new(worker, PathBuf::from("/nonexistent"));
        let preview = |approval| Command::Preview {
            approval,
            package: "hello".into(),
        };
        let drain = || -> Vec<(u64, Cancel)> {
            received
                .try_iter()
                .map(|(command, cancel)| (command.cancel_id().unwrap(), cancel))
                .collect()
        };
        // A cancel reaches work still in the queue, and only that work.
        local.send(preview(1)).unwrap();
        local.send(preview(2)).unwrap();
        let (_, running) = received.try_recv().unwrap();
        local.send(Command::Cancel(2)).unwrap();
        let queued = drain();
        assert_eq!(queued.len(), 1);
        assert_eq!(queued[0].0, 2);
        assert!(queued[0].1.check().is_err());
        assert!(running.check().is_ok());
        // And work the worker is running.
        local.send(Command::Cancel(1)).unwrap();
        assert!(running.check().is_err());
        // Unknown and repeated cancellations are harmless.
        local.send(Command::Cancel(1)).unwrap();
        local.send(Command::Cancel(9)).unwrap();
        // A full queue gives a request back, keeping no token for it, but
        // never refuses a cancellation.
        for approval in 3..7 {
            local.send(preview(approval)).unwrap();
        }
        assert!(matches!(
            local.send(preview(7)),
            Err(Command::Preview { approval: 7, .. })
        ));
        assert!(!local.cancels.contains_key(&7));
        local.send(Command::Cancel(4)).unwrap();
        let cancelled: Vec<_> = drain()
            .into_iter()
            .map(|(id, cancel)| (id, cancel.check().is_err()))
            .collect();
        assert_eq!(cancelled, [(3, false), (4, true), (5, false), (6, false)]);
        // Nor Stop, which reaches every queued request and the worker.
        for approval in 8..12 {
            local.send(preview(approval)).unwrap();
        }
        assert!(local.send(preview(12)).is_err());
        local.send(Command::Stop).unwrap();
        assert!(drain().iter().all(|(_, cancel)| cancel.check().is_err()));
        assert!(local.worker.cancel.check().is_err());
    }

    #[test]
    fn local_relays_init_the_payloads_exit_and_request_connections() {
        let directory = unix::temp_directory().unwrap();
        let (worker, _, _results) = worker();
        let mut local = Local::new(worker, PathBuf::from("/nonexistent"));
        let round = |local: &mut Local| std::iter::from_fn(|| local.poll()).collect::<Vec<_>>();
        // Before Started there is no init to ask.
        local
            .send(Command::Spawn {
                id: 1,
                argv: None,
                cwd: None,
                env: vec![],
                stdio: Stdio::NewPty { rows: 24, cols: 80 },
            })
            .unwrap();
        assert!(matches!(
            round(&mut local).as_slice(),
            [Event::SpawnFailed { id: 1, code: None, message }]
                if message == "the goblin has no init channel"
        ));
        let (control, sandbox) = init::pair().unwrap();
        let (exit, exited) = unix::pipe().unwrap();
        let listener = UnixListener::bind(directory.join("request.sock")).unwrap();
        listener.set_nonblocking(true).unwrap();
        local.init = Some(control);
        local.exit_watch = Some(exit);
        local.listener = Some(listener);
        let mut buffer = vec![0; init::MAX_MESSAGE];
        let mut sent = || {
            let (n, fds) = init::receive(sandbox.as_raw_fd(), &mut buffer, 0).unwrap();
            (
                serde_json::from_slice::<init::Command>(&buffer[..n]).unwrap(),
                fds.len(),
            )
        };
        let reply = |event: init::Event| {
            let bytes = serde_json::to_vec(&event).unwrap();
            init::send(sandbox.as_raw_fd(), &bytes, &[]).unwrap();
        };
        // A new PTY: its slave goes to init, its master comes back with
        // Spawned.
        local
            .send(Command::Spawn {
                id: 2,
                argv: Some(vec!["client".into()]),
                cwd: None,
                env: vec![],
                stdio: Stdio::NewPty { rows: 24, cols: 80 },
            })
            .unwrap();
        let (command, fds) = sent();
        assert!(matches!(
            command,
            init::Command::Spawn {
                id: 2,
                tty: true,
                ..
            }
        ));
        assert_eq!(fds, 1);
        reply(init::Event::Spawned { id: 2, pid: 42 });
        reply(init::Event::Failed {
            id: 3,
            code: 127,
            message: "not found".into(),
        });
        reply(init::Event::Exited {
            id: 2,
            code: Some(0),
            signal: None,
        });
        let events = round(&mut local);
        assert!(matches!(
            events.as_slice(),
            [
                Event::Spawned {
                    id: 2,
                    pid: 42,
                    master: Some(_)
                },
                Event::SpawnFailed {
                    id: 3,
                    code: Some(127),
                    ..
                },
                Event::Exited {
                    id: 2,
                    code: Some(0),
                    signal: None
                },
            ]
        ));
        // Signals and cancellations reach init in the order sent.
        local.send(Command::Signal { id: 2, signal: 2 }).unwrap();
        local
            .send(Command::CancelProcess {
                id: 2,
                everything: true,
            })
            .unwrap();
        local.send(Command::KillAll).unwrap();
        assert_eq!(sent().0, init::Command::Signal { id: 2, signal: 2 });
        assert_eq!(
            sent().0,
            init::Command::Cancel {
                id: 2,
                everything: true
            }
        );
        assert_eq!(sent().0, init::Command::KillAll {});
        // A request connection, then the payload's exit, reported once.
        let _client =
            std::os::unix::net::UnixStream::connect(directory.join("request.sock")).unwrap();
        // Data, not a closed end: a process another test forks may hold the
        // write end open for a moment, which would delay the end of file.
        assert_eq!(
            unsafe { libc::write(exited.as_raw_fd(), b"x".as_ptr().cast(), 1) },
            1
        );
        let events = round(&mut local);
        assert!(matches!(
            events.as_slice(),
            [Event::SandboxConnected(_), Event::PayloadExited]
        ));
        assert!(round(&mut local).is_empty());
        // Stop closes the request socket; init's end closes its channel.
        local.send(Command::Stop).unwrap();
        assert!(local.listener.is_none());
        // Shut the peer down, so init's end reads as closed at once, even
        // while a process another test forks holds a copy of the descriptor.
        assert_eq!(
            unsafe { libc::shutdown(sandbox.as_raw_fd(), libc::SHUT_RDWR) },
            0
        );
        drop(sandbox);
        assert!(matches!(round(&mut local).as_slice(), [Event::InitLost]));
        local.send(Command::KillAll).unwrap();
        assert!(round(&mut local).is_empty());
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn what_send_reports_never_delays_collecting_a_round() {
        let (worker, _, results) = worker();
        let mut local = Local::new(worker, PathBuf::from("/nonexistent"));
        let (exit, exited) = unix::pipe().unwrap();
        local.exit_watch = Some(exit);
        // A spawn without init fails between rounds...
        local
            .send(Command::Spawn {
                id: 1,
                argv: None,
                cwd: None,
                env: vec![],
                stdio: Stdio::NewPty { rows: 24, cols: 80 },
            })
            .unwrap();
        results
            .try_send(Event::Mode { server: false })
            .unwrap_or_else(|_| panic!());
        // Data, not a closed end: a process another test forks may hold the
        // write end open for a moment, which would delay the end of file.
        assert_eq!(
            unsafe { libc::write(exited.as_raw_fd(), b"x".as_ptr().cast(), 1) },
            1
        );
        // ...and the next round still collects every source.
        let round: Vec<_> = std::iter::from_fn(|| local.poll()).collect();
        assert!(matches!(
            round.as_slice(),
            [
                Event::SpawnFailed {
                    id: 1,
                    code: None,
                    ..
                },
                Event::Mode { server: false },
                Event::PayloadExited,
            ]
        ));
    }
}
