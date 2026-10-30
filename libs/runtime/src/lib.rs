//! Internal runtime contract; no daemon or sandbox implementation dependency.
pub use crate::lease::{Identity, ResourceLease};
use crate::policy::{DevShellSummary, Operation, Policy, Support};
use goblins_protocol::{Reply, Request};
use serde::Deserialize;
use std::{
    os::fd::{AsRawFd, OwnedFd, RawFd},
    path::PathBuf,
};

pub type Result<T, E = Box<dyn std::error::Error + Send + Sync>> = std::result::Result<T, E>;
pub mod binds;
pub mod catalog;
pub mod channel;
pub mod config;
pub mod devshell;
pub mod history;
pub mod lease;
mod logging;
pub mod policy;
pub mod scope;
pub use logging::log_event;
/// Why an allowNix goblin gets no package grants.
pub const NIX_PACKAGES: &str =
    "this goblin has Nix; use nix shell nixpkgs#NAME instead of a package request";
/// Why an allowNix goblin gets no dev shell refreshes or flake runs.
pub const NIX_DEV_SHELL: &str =
    "this goblin has Nix; use nix develop and nix run instead of the Goblins dev shell commands";

/// What a recreated goblin takes from its record.
pub struct Recreate {
    /// Approved package names, in grant order.
    pub packages: Vec<String>,
    /// Auto-approved names, resolved again but not mounted.
    pub auto_approve: Vec<String>,
    pub docker: bool,
    pub variables: std::collections::BTreeMap<String, String>,
}
/// What recreating staged, for the controller's hand-over.
pub struct Recreated {
    pub packages: Vec<(String, PathBuf)>,
    /// Recorded auto-approved names, resolved and rooted.
    pub auto_approve: Vec<(String, PathBuf)>,
    /// What could not be restored, and why.
    pub skipped: Vec<crate::history::Skipped>,
    /// Why the agent starts a fresh conversation despite a resume command.
    pub fresh: Option<String>,
}
/// The auto-approve set a session rooted for itself before it started.
#[derive(Default)]
pub struct Auto {
    pub rooted: Vec<(String, PathBuf)>,
    pub skipped: Vec<crate::history::Skipped>,
}
pub enum Source {
    Host {
        configuration: PathBuf,
        name: String,
        scope: Option<String>,
        dev_shell: Option<String>,
        workspace: Option<PathBuf>,
        cwd: Option<PathBuf>,
        state: PathBuf,
        /// The history store, protected like the daemon's state.
        history: Option<PathBuf>,
        recreate: Option<Recreate>,
        /// Order of this acceptance, for the daemon's current manifest.
        sequence: u64,
    },
    /// A goblin a raid owner runs, from the daemon's current manifest. It
    /// starts once the owner's handoff arrives.
    Member {
        /// Held from acceptance, so the owner stopping cannot take it away.
        scope: Option<Fd>,
        /// The scope the owner is in, which the configuration must be able to
        /// join.
        owner_scope: Option<String>,
        /// The current manifest, which the daemon keeps leased until the
        /// launch started.
        manifest: PathBuf,
        configuration: String,
        /// Approvals copied from the owner, by exact output.
        auto: std::collections::BTreeMap<String, PathBuf>,
        state: PathBuf,
        /// The history store, protected like the daemon's state.
        history: Option<PathBuf>,
    },
}
/// The daemon's view of one goblin's runtime. Neither method blocks.
pub trait Runtime: Send {
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
pub struct Fd(OwnedFd);

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

/// What a goblin a raid owner runs takes from it, produced by the owner's
/// runtime on `Command::Export` and carried by the daemon, which never looks
/// inside its descriptors, to the member's runtime.
#[derive(Debug)]
pub struct Handoff {
    /// A fresh lease on every resources directory the owner holds, its own
    /// and each it inherited, each its own open file description. The
    /// member keeps them for its whole life, so they stay after any owner in
    /// the chain stops.
    pub leases: Vec<ResourceLease>,
    pub workspace: Fd,
    /// Absent when the owner launched without a working directory; the
    /// member then gets none either.
    pub working_directory: Option<WorkingDirectory>,
    /// The owner's dev shell generation at the time of the export.
    pub dev_shell: Option<crate::devshell::DevShell>,
    /// Whether the owner had Docker attached, read once.
    pub docker: bool,
}

/// The owner's pinned working directory.
#[derive(Debug)]
pub struct WorkingDirectory {
    pub fd: Fd,
    /// The host path it was opened at.
    pub source: PathBuf,
    /// Where the sandbox sees it.
    pub destination: PathBuf,
}

/// A process's terminal or standard streams.
#[derive(Debug)]
pub enum Stdio {
    /// Frontend-provided stdin, stdout and stderr (`goblins exec`).
    Fds([Fd; 3]),
    /// A frontend-provided PTY slave (`goblins exec` with tty).
    Tty(Fd),
    /// A new PTY the runtime allocates; its master comes back in `Spawned`.
    NewPty { rows: u16, cols: u16 },
}

/// What a spawned process is for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SpawnKind {
    /// `goblins exec`.
    Exec,
    /// An attach client or its `onDetach`.
    Attach,
}

/// What the daemon asks of a runtime.
#[derive(Debug)]
pub enum Command {
    /// Preserve the scope-wide exit outcome after an unexpected engine death.
    ScopeFailed,
    /// Membership requested by a worker after resolving its scope.
    ScopeMembership {
        result: std::result::Result<Fd, String>,
    },
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
    /// daemon allocates ids; `argv` None runs the sandbox's shell. `kind`
    /// says what the process is for, so the runtime can check it.
    Spawn {
        id: u64,
        kind: SpawnKind,
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
    /// Hand over what a goblin the owner runs takes from it. Answered with
    /// `Event::Exported` without waiting for Nix work.
    Export {
        member: String,
    },
    /// The handoff a member waits for before it starts.
    Start(Box<Handoff>),
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
    /// What this command asks of the sandbox, for the runtime's own check of
    /// its policy; None for commands any goblin may be sent.
    pub fn operation(&self) -> Option<Operation> {
        match self {
            Command::Preview { .. } => Some(Operation::Package),
            Command::Decide { request, .. } => match request.kind.as_str() {
                "package" => Some(Operation::Package),
                "docker" => Some(Operation::Docker),
                "devshell" => Some(Operation::DevShellRefresh),
                _ => None,
            },
            Command::Refresh { .. } => Some(Operation::DevShellRefresh),
            Command::Run { .. } => Some(Operation::FlakeRun),
            Command::Diff { .. } => Some(Operation::DevShellDiff),
            Command::Spawn {
                kind: SpawnKind::Exec,
                ..
            } => Some(Operation::Exec),
            Command::Spawn {
                kind: SpawnKind::Attach,
                ..
            } => Some(Operation::Attach),
            Command::PrepareHostJob { .. } | Command::RunHostJob { .. } => {
                Some(Operation::HostCommand)
            }
            _ => None,
        }
    }
    /// The id a `Cancel` names this command by, when it can be cancelled.
    pub fn cancel_id(&self) -> Option<u64> {
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
pub enum Event {
    /// Transitional daemon-side Docker bridge failure, until phase 4B.
    ScopeFailed {
        name: String,
    },
    /// Resolve a private scope membership without blocking the event pump.
    ScopeNeeded {
        name: String,
        executable: PathBuf,
    },
    Preview {
        approval: u64,
        result: std::result::Result<crate::catalog::Preview, String>,
    },
    Started {
        docker_enabled: bool,
        /// What the configuration allows and what this implementation can
        /// do, fixed for the goblin's life.
        policy: Box<Policy>,
        support: Support,
        /// The launch's dev shell generation, when it has one.
        dev_shell: Option<DevShellSummary>,
        /// The agent's PTY, or with attach handlers its output pipe.
        master: OwnedFd,
        identity: Identity,
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
        summary: DevShellSummary,
    },
    /// What `member` takes from this goblin, or why it cannot be had.
    Exported {
        member: String,
        result: Result<Box<Handoff>, String>,
    },
    Granted {
        reply: Reply,
        detail: Option<String>,
        output: Option<PathBuf>,
    },
    /// A host launch's manifest validated; `sequence` orders acceptances.
    Manifest {
        sequence: u64,
        /// The first lease on the manifest, which owns the GC root of the
        /// store output that holds it: an event nobody consumes removes the
        /// root when it is dropped, and the daemon shares it with `Arc`.
        lease: crate::config::ManifestLease,
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
pub struct HostOutcome {
    pub exit_code: Option<i32>,
    pub signal: Option<i32>,
    pub spawn_error: Option<String>,
    pub cancelled: bool,
    pub timed_out: bool,
    pub truncated: bool,
}

/// Commands a `Fake` runtime received, shared with the test.
#[cfg(any(test, feature = "test-support"))]
pub type Received = std::sync::Arc<std::sync::Mutex<VecDeque<Command>>>;

/// A runtime for tests: it records the commands it is sent and replays the
/// events the test supplies, at most 16 a round.
#[cfg(any(test, feature = "test-support"))]
pub struct Fake {
    received: Received,
    events: mpsc::Receiver<Event>,
    round: usize,
    ended: bool,
    /// What the runtime holds for the goblin's life, such as a lease.
    held: Vec<Box<dyn std::any::Any + Send>>,
}

#[cfg(any(test, feature = "test-support"))]
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
            held: vec![],
        };
        (fake, sender, received)
    }
    /// The runtime holds `what` until it is dropped.
    pub fn holding(mut self, what: impl std::any::Any + Send) -> Self {
        self.held.push(Box::new(what));
        self
    }
}

#[cfg(any(test, feature = "test-support"))]
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

#[cfg(any(test, feature = "test-support"))]
use std::{collections::VecDeque, sync::mpsc};
