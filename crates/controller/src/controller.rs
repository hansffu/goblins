//! Single authority/event loop, with independent blocking workers per session.
use crate::{
    Result,
    session::{Cancel, Identity, Inheritance},
    terminal::Terminal,
    unix,
    worker::{Completed, Source, Work, Worker},
};
use goblins_protocol::{
    Request,
    rpc::{self, Decoder, DevShellDiffParams, FlakeRunParams, PermissionParams},
};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, VecDeque},
    fs::{self, File},
    io::{self, Read, Write},
    os::fd::{AsRawFd, OwnedFd},
    os::unix::{
        fs::PermissionsExt,
        net::{UnixListener, UnixStream},
    },
    path::{Path, PathBuf},
    time::Instant,
};

mod editor;
mod exec;
mod raids;
pub use raids::{RaidMemberRecord, RaidMembership, RaidRecord};

const HOSTS: usize = 32;
const SESSIONS: usize = 16;
const GOBLIN_NAMES: &str = include_str!("goblin-names.txt");
const QUEUE: usize = 2 * 1024 * 1024;
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SessionRecord {
    #[serde(default)]
    pub docker_enabled: bool,
    #[serde(default, alias = "docker_scope")]
    pub scope: Option<String>,
    /// Flake reference of the host-launched dev shell, if any.
    #[serde(default)]
    pub dev_shell: Option<String>,
    /// Working directory the sandbox was launched in (children inherit it).
    #[serde(default)]
    pub cwd: Option<PathBuf>,
    pub id: String,
    pub agent_name: String,
    /// None denotes the host root. Ownership always uses immutable IDs.
    #[serde(default)]
    pub parent: Option<String>,
    #[serde(default)]
    pub path: String,
    /// Reusable Nix configuration key, distinct from the per-launch agent name.
    pub name: String,
    #[serde(default)]
    pub description: String,
    pub configuration: String,
    #[serde(default)]
    pub agent_state: goblins_protocol::AgentState,
    /// When `agent_state` last changed, in Unix milliseconds.
    #[serde(default)]
    pub agent_state_since: Option<u64>,
    /// Messages queued in the goblin's inbox, as the messaging worker last
    /// counted them.
    #[serde(default)]
    pub inbox: u64,
    pub state: String,
    pub identity: Option<Identity>,
    pub initial_packages: Vec<String>,
    pub packages: Vec<String>,
    pub terminal: String,
    pub pty_eof: bool,
    pub terminal_complete: bool,
    pub terminal_interrupted: bool,
    #[serde(default)]
    pub terminal_attached: bool,
    #[serde(default)]
    pub terminal_detached: bool,
    pub exit_code: Option<i32>,
    /// Daemon-authored lifecycle explanation, safe to expose inside sandboxes.
    #[serde(default)]
    pub stop_reason: Option<String>,
    pub detail: Option<String>,
    #[serde(default)]
    pub raid: Option<RaidMembership>,
    /// Live interactive shells (`goblins exec -t`) in the sandbox.
    #[serde(default)]
    pub shells: u32,
    /// Live non-interactive execs: TRAMP, eshell and scripts.
    #[serde(default)]
    pub execs: u32,
    /// The history record this goblin was recreated from.
    #[serde(default)]
    pub recreated_from: Option<String>,
    /// What could not be restored when it was recreated, and why.
    #[serde(default)]
    pub skipped: Vec<crate::history::Skipped>,
}
impl SessionRecord {
    /// True when the visible activity changed; the change is timestamped.
    fn set_agent_state(&mut self, state: goblins_protocol::AgentState) -> bool {
        let changed = self.agent_state != state;
        if changed {
            self.agent_state = state;
            self.agent_state_since = Some(crate::messaging::now());
        }
        changed
    }
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PermissionRecord {
    #[serde(default = "goblins_protocol::package_kind")]
    pub kind: String,
    pub id: String,
    pub session: String,
    /// Retained even after the session record is evicted or its name reused.
    pub agent_name: String,
    pub approval: String,
    pub package: String,
    pub reason: String,
    pub state: String,
    pub approved: Option<bool>,
    pub preview: Option<Value>,
    pub message: Option<String>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Snapshot {
    pub instance: String,
    pub sessions: Vec<SessionRecord>,
    pub permissions: Vec<PermissionRecord>,
    #[serde(default)]
    pub raids: Vec<RaidRecord>,
}
struct Pending {
    initial_output: Option<PathBuf>,
    /// Identity of a prepared dev shell refresh candidate.
    dev_identity: Option<String>,
    id: String,
    serial: u64,
    connection: u64,
    cancel: Cancel,
    /// The requester has an agent integration to wake with a completion notice.
    notify: bool,
}
/// A sandbox call waiting on the worker (`flake.run`, `devshell.diff`). One
/// at a time per sandbox; the worker runs them in turn with other work.
struct Call {
    serial: u64,
    connection: u64,
    cancel: Cancel,
}
fn call_key(serial: u64) -> String {
    format!("call-{serial}")
}
fn prune_key(serial: u64) -> String {
    format!("prune-{serial}")
}
struct Active {
    activity_at: Option<Instant>,
    created: u64,
    record: SessionRecord,
    worker: Option<Worker>,
    listener: Option<UnixListener>,
    pending: Option<Pending>,
    terminal: Terminal,
    directory: PathBuf,
    inheritance: Option<Inheritance>,
    exit_watch: Option<OwnedFd>,
    /// The private channel to the sandbox's goblins-init.
    init: Option<OwnedFd>,
    granted_outputs: BTreeMap<String, PathBuf>,
    /// Identity of the current trusted dev shell generation.
    dev_identity: Option<String>,
    call: Option<Call>,
    /// The record this goblin was recreated from, reserved while it is live
    /// so no other goblin resumes the same agent conversation.
    recreated_from: Option<Box<crate::history::Record>>,
    /// A root goblin's recorded variables, kept also while this server has
    /// no history, for a record created when it takes the store over.
    variables: BTreeMap<String, String>,
    /// The history record this goblin writes: the record it was recreated
    /// from, otherwise its own id.
    line: String,
}
impl Active {
    /// Refresh activity freshness; true when the visible activity changed.
    fn report_activity(&mut self, state: goblins_protocol::AgentState) -> bool {
        self.activity_at = Some(Instant::now());
        self.record.set_agent_state(state)
    }
}
#[derive(Clone)]
enum Role {
    Host,
    Sandbox(String),
}
struct Connection {
    id: u64,
    role: Role,
    peer: UnixStream,
    decoder: Decoder,
    input: Vec<u8>,
    output: VecDeque<Vec<u8>>,
    offset: usize,
    queued: usize,
    initialized: bool,
    waiting: Option<(String, Value)>,
    close: bool,
    dead: bool,
    calls: usize,
    body_bytes: usize,
    subscription: Option<String>,
    /// Descriptors received with the pending frame, for `sessions.exec`.
    fds: Vec<OwnedFd>,
    /// The exec this connection holds open; only signals may follow.
    exec: Option<u64>,
}
impl Connection {
    fn new(id: u64, role: Role, peer: UnixStream) -> Result<Self> {
        peer.set_nonblocking(true)?;
        let limit = goblins_protocol::messages::MAX_REQUEST;
        Ok(Self {
            id,
            role,
            peer,
            decoder: Decoder::new(limit, Some(Instant::now())),
            input: vec![],
            output: VecDeque::new(),
            offset: 0,
            queued: 0,
            initialized: false,
            waiting: None,
            close: false,
            dead: false,
            calls: 0,
            body_bytes: 0,
            subscription: None,
            fds: Vec::new(),
            exec: None,
        })
    }
    fn queue(&mut self, value: Value) {
        match rpc::encode(&value) {
            Ok(data) if self.queued + data.len() <= QUEUE => {
                self.queued += data.len();
                self.output.push_back(data);
            }
            _ => self.dead = true,
        }
    }
    fn flush(&mut self) {
        // One bounded write per tick; a slow subscriber cannot hold authority.
        if let Some(front) = self.output.front() {
            match self.peer.write(&front[self.offset..]) {
                Ok(0) => self.dead = true,
                Ok(n) => {
                    self.offset += n;
                    self.queued -= n;
                    if self.offset == front.len() {
                        self.output.pop_front();
                        self.offset = 0;
                    }
                }
                Err(e)
                    if matches!(
                        e.kind(),
                        io::ErrorKind::WouldBlock | io::ErrorKind::Interrupted
                    ) => {}
                Err(_) => self.dead = true,
            }
        }
        if self.close && self.output.is_empty() {
            self.dead = true;
        }
    }
    fn read(&mut self) -> Option<rpc::Call> {
        if self.close || self.dead {
            return None;
        }
        let mut bytes = [0; 16384];
        let waiting = self.waiting.is_some() && self.exec.is_none();
        match goblins_protocol::init::receive(self.peer.as_raw_fd(), &mut bytes, libc::MSG_DONTWAIT)
        {
            Ok((0, _)) => {
                self.dead = true;
                return None;
            }
            Ok((n, fds)) => {
                // Only host clients pass descriptors, and only for one exec.
                if matches!(self.role, Role::Host) {
                    self.fds.extend(fds);
                }
                if waiting
                    || self.input.len() + n > 32768
                    || self.fds.len() > goblins_protocol::init::MAX_FDS
                {
                    self.dead = true;
                    return None;
                }
                self.input.extend_from_slice(&bytes[..n]);
            }
            Err(e)
                if matches!(
                    e.kind(),
                    io::ErrorKind::WouldBlock | io::ErrorKind::Interrupted
                ) => {}
            Err(_) => {
                self.dead = true;
                return None;
            }
        }
        if waiting {
            return None;
        }
        match self.decoder.push(&self.input, Instant::now()) {
            Ok((n, body)) => {
                self.input.drain(..n);
                if let Some(body) = body {
                    self.body_bytes = body.len();
                    self.calls += 1;
                    let limit = if matches!(self.role, Role::Host) {
                        4096
                    } else {
                        2
                    };
                    if self.calls > limit {
                        self.dead = true;
                        return None;
                    }
                    let limit = goblins_protocol::messages::MAX_REQUEST;
                    self.decoder = Decoder::new(
                        limit,
                        if matches!(self.role, Role::Sandbox(_)) || !self.initialized {
                            Some(Instant::now())
                        } else {
                            None
                        },
                    );
                    match rpc::call(&body) {
                        Ok(c) => Some(c),
                        Err(e) => {
                            self.queue(e);
                            self.close = true;
                            None
                        }
                    }
                } else {
                    None
                }
            }
            Err(e) => {
                self.queue(rpc::error(Value::Null, -32600, &e.to_string()));
                self.close = true;
                None
            }
        }
    }
}
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Start {
    key: String,
    configuration: String,
    /// Empty for a recreate, which takes it from the record.
    #[serde(default)]
    name: String,
    agent_name: Option<String>,
    #[serde(default)]
    parent: Option<String>,
    #[serde(default)]
    cwd: Option<PathBuf>,
    /// Scope for a root sandbox; children always inherit their parent's.
    #[serde(default)]
    scope: Option<String>,
    /// Absolute `PATH[#ATTR]` of a host flake dev shell; children inherit.
    #[serde(default)]
    dev_shell: Option<String>,
    rows: u16,
    cols: u16,
    #[serde(default)]
    detached: bool,
    /// Join this raid, creating it with the new goblin as owner if absent.
    #[serde(default)]
    raid: Option<String>,
    /// Sandbox launches only: join the selected parent's raid.
    #[serde(skip)]
    inherit_raid: bool,
    /// Recreate this recorded goblin (record id, unique id prefix or unique
    /// agent name) from the current configuration of the same name.
    #[serde(default)]
    recreate: Option<String>,
    /// The record a recreate launch uses, resolved by the daemon.
    #[serde(skip)]
    recorded: Option<Box<crate::history::Record>>,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ChildStart {
    key: String,
    name: String,
    agent_name: Option<String>,
    parent: Option<String>,
    #[serde(default)]
    detached: bool,
    #[serde(default)]
    inherit_raid: bool,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Stop {
    session: String,
    #[serde(default)]
    kill_children: bool,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Initialize {
    api: u32,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SessionId {
    session: String,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RequestId {
    request: String,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Decision {
    session: String,
    request: String,
    approval: String,
    approved: bool,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Resize {
    session: String,
    rows: u16,
    cols: u16,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Filter {
    session: Option<String>,
    state: Option<String>,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Subscription {
    subscription: String,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Empty {}
type Fault = (i32, String);
fn params<T: DeserializeOwned>(v: Value) -> std::result::Result<T, Fault> {
    serde_json::from_value(v).map_err(|e| (-32602, e.to_string()))
}
fn missing() -> Fault {
    (-32004, "record absent or no longer retained".into())
}
fn conflict() -> Fault {
    (-32009, "stale or conflicting operation".into())
}
fn capacity() -> Fault {
    (-32010, "resource limit reached".into())
}
fn dimensions(rows: u16, cols: u16) -> bool {
    (1..=1000).contains(&rows) && (1..=1000).contains(&cols)
}
pub(crate) fn valid_agent_name(name: &str) -> bool {
    (1..=32).contains(&name.len())
        && name.as_bytes()[0].is_ascii_lowercase()
        && name
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
}
// Rejection sampling avoids modulo bias. Bound retries even if the random
// source fails or repeatedly supplies rejected values.
fn random_name<'a>(
    available: &[&'a str],
    random: &mut impl Read,
) -> std::result::Result<&'a str, Fault> {
    if available.is_empty() {
        return Err((-32010, "automatic agent name pool exhausted".into()));
    }
    let count = available.len() as u64;
    let limit = u64::MAX - u64::MAX % count;
    for _ in 0..8 {
        let mut bytes = [0; 8];
        random
            .read_exact(&mut bytes)
            .map_err(|_| (-32010, "cannot read randomness for agent name".into()))?;
        let sample = u64::from_le_bytes(bytes);
        if sample < limit {
            return Ok(available[(sample % count) as usize]);
        }
    }
    Err((
        -32010,
        "cannot select random agent name after 8 attempts".into(),
    ))
}
pub struct Controller {
    messaging: crate::messaging::Service,
    messaging_directory: crate::mailbox::Directory,
    state: PathBuf,
    workspace: Option<PathBuf>,
    _lock: File,
    listener: UnixListener,
    instance: String,
    next: u64,
    sequence: u64,
    connections: Vec<Connection>,
    sessions: BTreeMap<String, Active>,
    execs: BTreeMap<u64, exec::Exec>,
    edits: Vec<editor::Edit>,
    permissions: VecDeque<PermissionRecord>,
    launches: BTreeMap<String, (Start, Value)>,
    shutdown: Option<(u64, Instant)>,
    dirty: bool,
    /// Never persisted: raids end with the daemon.
    raids: crate::raid::Raids,
    /// Raid events not yet accepted by the messaging worker, in order.
    raid_outbox: VecDeque<crate::raid::Event>,
    raid_events: usize,
    /// Operation lifecycle events not yet accepted by the messaging worker,
    /// in order. Completions are never dropped; new requests are refused
    /// while this is full.
    operation_outbox: VecDeque<crate::operation::Event>,
    /// Recreate records, or why this daemon keeps none.
    history: std::result::Result<crate::history::Store, String>,
    /// The history store's directory, protected from binds even when
    /// another daemon owns it.
    history_directory: Option<PathBuf>,
    keep_sessions: usize,
    /// The store, once the process that holds it lets go. A short CLI
    /// call must not leave this server without history for good.
    history_waiting: Option<std::sync::mpsc::Receiver<crate::history::Store>>,
    /// Ends the waiting thread with this server.
    history_token: std::sync::Arc<()>,
}
impl Controller {
    pub fn new(state: &Path, workspace: Option<PathBuf>) -> Result<Self> {
        unix::private_directory(state)?;
        let state = fs::canonicalize(state)?;
        let lock = unix::lock(&state.join("daemon.lock"))?;
        let socket = state.join("host.sock");
        if socket.is_symlink() {
            return Err("refusing symlink host socket".into());
        }
        if socket.exists() {
            fs::remove_file(&socket)?;
        }
        let listener = UnixListener::bind(socket.clone())?;
        listener.set_nonblocking(true)?;
        fs::set_permissions(socket, fs::Permissions::from_mode(0o600))?;
        let mut bytes = [0; 16];
        File::open("/dev/urandom")?.read_exact(&mut bytes)?;
        let instance: String = bytes.iter().map(|b| format!("{b:02x}")).collect();
        let messaging = crate::messaging::Service::new(&state, &instance)?;
        Ok(Self {
            messaging,
            messaging_directory: BTreeMap::new(),
            state,
            workspace,
            _lock: lock,
            listener,
            instance,
            next: 0,
            sequence: 0,
            connections: vec![],
            sessions: BTreeMap::new(),
            execs: BTreeMap::new(),
            edits: Vec::new(),
            permissions: VecDeque::new(),
            launches: BTreeMap::new(),
            shutdown: None,
            dirty: false,
            raids: Default::default(),
            raid_outbox: VecDeque::new(),
            raid_events: 0,
            operation_outbox: VecDeque::new(),
            history: Err("this server keeps no session history".into()),
            history_directory: None,
            keep_sessions: crate::history::DEFAULT_KEEP,
            history_waiting: None,
            history_token: std::sync::Arc::new(()),
        })
    }
    /// Own the history store in `directory`. While another process holds
    /// it, run without history and take it over once it is released.
    /// Call before the first launch.
    pub fn open_history(&mut self, directory: Result<PathBuf>, keep: usize) {
        self.keep_sessions = keep;
        let directory = match directory {
            Ok(directory) => directory,
            Err(e) => {
                self.history = Err(format!("no session history: {e}"));
                return;
            }
        };
        let owner = format!(
            "goblins server (pid {}, --state-dir {})",
            std::process::id(),
            self.state.display()
        );
        match crate::history::Store::open(&directory, &owner) {
            Ok(store) => {
                self.history_directory = Some(store.directory.clone());
                self.history = Ok(store);
            }
            Err(error) => {
                self.history_directory = Some(directory.clone());
                self.history = Err(error.to_string());
                if error.is::<crate::history::Held>() {
                    self.history = Err(format!(
                        "{error}; this server takes it over once it is released"
                    ));
                    let (found, waiting) = std::sync::mpsc::sync_channel(1);
                    let alive = std::sync::Arc::downgrade(&self.history_token);
                    std::thread::spawn(move || {
                        while alive.upgrade().is_some() {
                            std::thread::sleep(std::time::Duration::from_secs(2));
                            match crate::history::Store::open(&directory, &owner) {
                                Ok(store) => {
                                    let _ = found.send(store);
                                    return;
                                }
                                Err(e) if e.is::<crate::history::Held>() => (),
                                Err(_) => return,
                            }
                        }
                    });
                    self.history_waiting = Some(waiting);
                }
            }
        }
    }
    /// Take over the history store once its holder released it.
    fn poll_history(&mut self) {
        let Some(store) = self
            .history_waiting
            .as_ref()
            .and_then(|w| w.try_recv().ok())
        else {
            return;
        };
        println!("Session history in {}", store.directory.display());
        self.history_waiting = None;
        self.history_directory = Some(store.directory.clone());
        self.history = Ok(store);
        self.record_running();
    }
    /// The history store's directory and whether this daemon owns it.
    pub fn history_status(&self) -> Value {
        match &self.history {
            Ok(store) => json!({"directory":store.directory,"owner":true,"keep":self.keep_sessions,
                "recording":self.workspace.is_none()}),
            Err(reason) => {
                json!({"directory":self.history_directory,"owner":false,"reason":reason})
            }
        }
    }
    /// Records with a live goblin, which retention and prune never delete.
    fn live_lines(&self) -> std::collections::BTreeSet<String> {
        self.sessions
            .values()
            .filter(|a| a.worker.is_some())
            .map(|a| a.line.clone())
            .collect()
    }
    /// Start the record of a root goblin that reached running.
    fn record_started(&mut self, a: &Active) {
        if a.record.parent.is_some() || self.workspace.is_some() {
            return;
        }
        let mut running = self.live_lines();
        running.insert(a.line.clone());
        let keep = self.keep_sessions;
        let Ok(store) = &mut self.history else {
            return;
        };
        // A recreated goblin continues its record; a new one starts its own.
        let record = line_record(a, store.records.get(&a.line));
        store.write(record);
        store.retain(keep, &running);
    }
    /// Records for root goblins that reached running before this server
    /// owned the history store, so they can be recreated too.
    fn record_running(&mut self) {
        if self.workspace.is_some() {
            return;
        }
        let running = self.live_lines();
        let keep = self.keep_sessions;
        let Ok(store) = &mut self.history else {
            return;
        };
        for a in self.sessions.values() {
            if a.record.parent.is_none()
                && a.record.state == "running"
                && a.worker.is_some()
                && store
                    .records
                    .get(&a.line)
                    .is_none_or(|r| r.latest() != a.record.id)
            {
                let record = line_record(a, store.records.get(&a.line));
                store.write(record);
            }
        }
        store.retain(keep, &running);
    }
    /// Fill a recreate launch from its record, refusing what cannot be
    /// recreated now.
    fn prepare_recreate(&self, p: &mut Start, target: &str) -> std::result::Result<(), Fault> {
        if p.parent.is_some() {
            return Err((
                -32602,
                "resume cannot be combined with --parent: only root goblins are resumed".into(),
            ));
        }
        if !p.name.is_empty()
            || p.agent_name.is_some()
            || p.cwd.is_some()
            || p.scope.is_some()
            || p.dev_shell.is_some()
            || p.raid.is_some()
        {
            return Err((
                -32602,
                "resume takes the configuration, agent name, scope, dev shell and working directory from its record".into(),
            ));
        }
        if self.workspace.is_some() {
            return Err((
                -32009,
                "this server snapshots a --workspace for each goblin, so its goblins are neither recorded nor resumed".into(),
            ));
        }
        let store = self
            .history
            .as_ref()
            .map_err(|reason| (-32009, format!("no session history: {reason}")))?;
        let record = store.resolve(target).map_err(|e| (-32004, e))?;
        if let Some(cwd) = &record.cwd
            && !cwd.is_dir()
        {
            return Err((
                -32009,
                format!(
                    "the recorded working directory {} no longer exists",
                    cwd.display()
                ),
            ));
        }
        if let Some(reason) = self.recreate_conflict(record) {
            return Err((
                -32009,
                format!("record {} cannot be resumed now: {reason}", record.id),
            ));
        }
        p.name = record.configuration.clone();
        // Its own name when that is free, otherwise a new one.
        p.agent_name = Some(record.agent_name.clone())
            .filter(|name| self.allocate_name(Some(name), None).is_ok());
        p.cwd = record.cwd.clone();
        p.scope = record.scope.clone();
        p.dev_shell = record.dev_shell.clone();
        p.recorded = Some(Box::new(record.clone()));
        Ok(())
    }
    /// Why a record cannot be recreated: a live goblin (starting, running
    /// or stopping) runs it, so both would resume the same conversation.
    fn recreate_conflict(&self, record: &crate::history::Record) -> Option<String> {
        self.sessions
            .values()
            .filter(|a| a.worker.is_some() && a.record.parent.is_none())
            .find(|a| a.line == record.id)
            .map(|a| {
                format!(
                    "goblin {} ({}) is live and runs it",
                    a.record.agent_name, a.record.id
                )
            })
    }
    /// One record per agent conversation: when a goblin reports a
    /// conversation another record holds, and no goblin runs that one, it
    /// is merged into this goblin's record and removed.
    fn merge_conversation(&mut self, line: &str) {
        let live = self.live_lines();
        let Ok(store) = &mut self.history else {
            return;
        };
        let Some(mut merged) = store.records.get(line).cloned() else {
            return;
        };
        let Some(conversation) = merged.conversation().map(String::from) else {
            return;
        };
        let mut others: Vec<_> = store
            .records
            .values()
            .filter(|r| {
                r.id != line
                    && r.conversation() == Some(conversation.as_str())
                    && !live.contains(&r.id)
                    && !store.deleting.contains(&r.id)
            })
            .cloned()
            .collect();
        if others.is_empty() {
            return;
        }
        others.sort_by_key(|r| std::cmp::Reverse(r.created));
        for other in &others {
            // Their goblins ran first: their ids lead, and still find it.
            let mut sessions = other.sessions.clone();
            sessions.retain(|s| !merged.sessions.contains(s));
            sessions.append(&mut merged.sessions);
            merged.sessions = sessions;
            let excess = merged
                .sessions
                .len()
                .saturating_sub(crate::history::MAX_SESSIONS);
            merged.sessions.drain(..excess);
            for package in &other.packages.approved {
                if !merged.packages.denied.contains(package) {
                    let _ = merged.approve(package);
                }
            }
            merged.created = merged.created.min(other.created);
        }
        merged.updated = crate::messaging::now();
        let ids: Vec<_> = others.into_iter().map(|r| r.id).collect();
        store.merge(merged, &ids);
    }
    /// The history store's records for `goblins history`, newest first,
    /// each marked running or not, within the response size.
    fn history_list(&self) -> std::result::Result<Value, Fault> {
        let store = self
            .history
            .as_ref()
            .map_err(|reason| (-32009, format!("no session history: {reason}")))?;
        let running = self.live_lines();
        let mut records = vec![];
        let mut size = 0;
        let mut truncated = false;
        for record in crate::history::newest_first(&store.records) {
            let mut value = json!(record);
            value["running"] = json!(running.contains(&record.id));
            size += serde_json::to_vec(&value).map_or(0, |v| v.len());
            if size > 900 * 1024 {
                truncated = true;
                break;
            }
            records.push(value);
        }
        Ok(json!({"directory":store.directory,"records":records,"truncated":truncated}))
    }
    /// `history.prune`. Records go only once the writer has deleted them
    /// durably, so the caller waits for that and learns what failed.
    fn history_prune(
        &mut self,
        c: &mut Connection,
        id: Value,
        value: Value,
    ) -> std::result::Result<Option<Value>, Fault> {
        let p: crate::history::Prune = params(value)?;
        if p.is_empty() {
            return Err((-32602, "choose --older-than, --before or --keep".into()));
        }
        let mut excluded = self.live_lines();
        let serial = self.number();
        let store = self
            .history
            .as_mut()
            .map_err(|reason| (-32009, format!("no session history: {reason}")))?;
        excluded.extend(store.deleting.iter().cloned());
        let removed = p.select(&store.records, &excluded, crate::messaging::now());
        if p.dry_run || removed.is_empty() {
            return Ok(Some(
                json!({"removed":removed,"failed":[],"dry_run":p.dry_run}),
            ));
        }
        // Ordered with every write: the writer drops queued revisions.
        store.delete(&removed, Some(serial));
        c.waiting = Some((prune_key(serial), id));
        Ok(None)
    }
    /// Apply finished deletes and answer the prunes waiting for them.
    fn poll_deletes(&mut self) {
        let Ok(store) = &mut self.history else {
            return;
        };
        for result in store.poll() {
            let Some(tag) = result.tag else {
                continue;
            };
            let failed: Vec<_> = result
                .failed
                .iter()
                .map(|(id, reason)| json!({"id":id,"reason":reason}))
                .collect();
            let key = prune_key(tag);
            for c in &mut self.connections {
                if let Some((waiting, rpc_id)) = &c.waiting
                    && *waiting == key
                {
                    c.queue(rpc::result(
                        rpc_id.clone(),
                        json!({"removed":result.removed,"failed":failed,"dry_run":false}),
                    ));
                    c.waiting = None;
                }
            }
        }
    }
    /// Change the record of a live root goblin. Ok(false) when it has none;
    /// Err with the reason when the change does not fit, leaving it as it was.
    fn update_record(
        &mut self,
        id: &str,
        change: impl FnOnce(&mut crate::history::Record) -> std::result::Result<bool, String>,
    ) -> std::result::Result<bool, String> {
        let Ok(store) = &mut self.history else {
            return Ok(false);
        };
        // A stopped record is final, and a pruned one is not resurrected.
        let Some(mut record) = store
            .records
            .get(id)
            .filter(|r| r.stopped.is_none())
            .cloned()
        else {
            return Ok(false);
        };
        let changed = change(&mut record)?;
        if changed {
            record.updated = crate::messaging::now();
            store.write(record);
        }
        Ok(changed)
    }
    fn number(&mut self) -> u64 {
        self.next = self.next.checked_add(1).expect("daemon identity exhausted");
        self.next
    }
    fn id(&mut self, kind: &str) -> String {
        let n = self.number();
        format!("{}-{kind}{n}", self.instance)
    }
    pub fn snapshot(&self) -> Snapshot {
        Snapshot {
            instance: self.instance.clone(),
            sessions: self.sessions.values().map(|s| s.record.clone()).collect(),
            permissions: self.permissions.iter().cloned().collect(),
            raids: self.raid_records(),
        }
    }
    pub fn shutdown_ready(&self) -> bool {
        self.shutdown.is_some_and(|(connection, deadline)| {
            Instant::now() >= deadline
                || !self
                    .connections
                    .iter()
                    .any(|c| c.id == connection && c.queued != 0)
        })
    }
    fn publish(&mut self) {
        // A stop dispatched just before must not publish the old membership.
        self.reconcile_raids();
        if !self.dirty {
            return;
        }
        // Count-based retention is supplemented by a total snapshot byte cap.
        // Evictions are visible in the next replacement snapshot.
        while serde_json::to_vec(&self.snapshot()).is_ok_and(|v| v.len() > 900 * 1024) {
            if let Some(index) = self.permissions.iter().position(|p| {
                !["pending", "realizing", "mounting", "publishing"].contains(&p.state.as_str())
            }) {
                self.permissions.remove(index);
            } else if let Some(id) = self
                .sessions
                .iter()
                .filter(|(id, a)| {
                    a.worker.is_none()
                        && a.terminal.can_evict()
                        && !self
                            .sessions
                            .values()
                            .any(|child| child.record.parent.as_ref() == Some(id))
                })
                .min_by_key(|(_, a)| a.created)
                .map(|(id, _)| id.clone())
            {
                if let Some(a) = self.sessions.remove(&id) {
                    let _ = fs::remove_file(a.directory.join("terminal.sock"));
                    let _ = fs::remove_dir(a.directory);
                }
            } else {
                break;
            }
        }
        self.dirty = false;
        self.sequence += 1;
        let snapshot = self.snapshot();
        for c in &mut self.connections {
            if let Some(sub) = &c.subscription {
                c.queue(json!({"jsonrpc":"2.0","method":"state.changed","params":{"subscription":sub,"sequence":self.sequence,"snapshot":snapshot}}));
            }
        }
    }
    fn permission_mut(&mut self, id: &str) -> Option<&mut PermissionRecord> {
        self.permissions.iter_mut().find(|p| p.id == id)
    }
    fn operation(&mut self, request: &str, actor: &str, change: crate::operation::Change) {
        if let Some(r) = self.permissions.iter().find(|r| r.id == request) {
            self.operation_outbox.push_back(crate::operation::Event {
                request: request.into(),
                session: r.session.clone(),
                actor: actor.into(),
                change,
            });
        }
    }
    /// Queue the terminal outcome the record now holds. Call once per
    /// request, after its state is final; `message` is what the sandbox sees.
    fn operation_finished(&mut self, request: &str, notify: bool, message: Option<String>) {
        let Some(r) = self.permissions.iter().find(|r| r.id == request) else {
            return;
        };
        let change = crate::operation::Change::Completed {
            kind: r.kind.clone(),
            subject: r.package.clone(),
            status: r.state.clone(),
            message,
            notify,
        };
        self.operation(request, goblins_protocol::messages::NOTICE_SENDER, change);
    }
    fn allocate_name(
        &self,
        requested: Option<&str>,
        parent: Option<&str>,
    ) -> std::result::Result<String, Fault> {
        // Reserve ancestor names until the entire subtree finishes cleanup,
        // so reusing a parent name cannot produce duplicate live tree paths.
        // Allocation and insertion are serialized in this event loop.
        let available = |name: &str| {
            !self.sessions.values().any(|a| {
                a.record.parent.as_deref() == parent
                    && a.record.agent_name == name
                    && (a.worker.is_some()
                        || self.sessions.values().any(|child| {
                            child.worker.is_some()
                                && self.descendant(&child.record.id, &a.record.id)
                        }))
            })
        };
        if let Some(name) = requested {
            if !valid_agent_name(name) {
                return Err((-32602, "agent_name must be 1..32 ASCII lowercase letters, digits or hyphens, starting with a letter".into()));
            }
            if !available(name) {
                return Err((-32009, format!("agent name '{name}' is already in use")));
            }
            return Ok(name.into());
        }
        let available: Vec<_> = GOBLIN_NAMES
            .lines()
            .filter(|name| available(name))
            .collect();
        let mut random = File::open("/dev/urandom").map_err(|_| {
            (
                -32010,
                "cannot open randomness source for agent name".into(),
            )
        })?;
        random_name(&available, &mut random).map(str::to_owned)
    }
    fn descendant(&self, id: &str, ancestor: &str) -> bool {
        let mut current = self
            .sessions
            .get(id)
            .and_then(|a| a.record.parent.as_deref());
        while let Some(parent) = current {
            if parent == ancestor {
                return true;
            }
            current = self
                .sessions
                .get(parent)
                .and_then(|a| a.record.parent.as_deref());
        }
        false
    }
    fn relative_path<'a>(&'a self, record: &'a SessionRecord, scope: Option<&str>) -> &'a str {
        if let Some(root) = scope.and_then(|id| self.sessions.get(id)) {
            record
                .path
                .strip_prefix(&format!("{}/", root.record.path))
                .unwrap_or(&record.path)
        } else {
            &record.path
        }
    }
    fn resolve_scoped(
        &self,
        target: &str,
        scope: Option<&str>,
    ) -> std::result::Result<String, Fault> {
        let visible = |a: &&Active| scope.is_none_or(|root| self.descendant(&a.record.id, root));
        let records: Vec<_> = self.sessions.values().filter(visible).collect();
        if let Some(a) = records.iter().find(|a| a.record.id == target) {
            return Ok(a.record.id.clone());
        }
        if let Some(a) = records
            .iter()
            .find(|a| a.worker.is_some() && self.relative_path(&a.record, scope) == target)
        {
            return Ok(a.record.id.clone());
        }
        let mut matches = records
            .iter()
            .filter(|a| a.worker.is_some() && a.record.agent_name == target);
        let first = matches.next().ok_or_else(missing)?;
        if matches.next().is_some() {
            return Err((
                -32009,
                "ambiguous agent name; use its tree path or session ID".into(),
            ));
        }
        Ok(first.record.id.clone())
    }
    fn resolve_session(&self, target: &str) -> std::result::Result<String, Fault> {
        self.resolve_scoped(target, None)
    }
    fn sandbox_record(&self, record: &SessionRecord, scope: &str) -> Value {
        // No host paths, process IDs, terminal sockets or diagnostics cross the
        // sandbox endpoint. Paths are relative to the caller's subtree.
        json!({"id":record.id,"agent_name":record.agent_name,
            "parent":record.parent.as_deref().filter(|p| *p != scope),
            "path":self.relative_path(record, Some(scope)),"name":record.name,
            "description":record.description,"state":record.state,"agent_state":record.agent_state,
            "initial_packages":record.initial_packages,
            "packages":record.packages,"scope":record.scope,"docker_enabled":record.docker_enabled,
            "exit_code":record.exit_code,
            "stop_reason":record.stop_reason,
            "terminal_complete":record.terminal_complete,
            "terminal_interrupted":record.terminal_interrupted,
            "terminal_detached":record.terminal_detached,
            "terminal_attached":record.terminal_attached})
    }
    fn owner_exited_reason(&self, id: &str) -> String {
        format!(
            "stopped because owner goblin {} ({id}) exited",
            self.sessions[id].record.agent_name
        )
    }
    fn stop_tree(
        &mut self,
        id: &str,
        kill_children: bool,
        reason: Option<String>,
    ) -> std::result::Result<Value, Fault> {
        let descendants: Vec<_> = self
            .sessions
            .iter()
            .filter(|(child, a)| a.worker.is_some() && self.descendant(child, id))
            .map(|(id, _)| id.clone())
            .collect();
        if !kill_children && !descendants.is_empty() {
            return Err((
                -32009,
                format!(
                    "warning: sandbox has {} live descendant(s); use --kill-children to kill the subtree",
                    descendants.len()
                ),
            ));
        }
        // The event loop serializes this check with child creation. Stop the
        // whole tree before accepting any more calls from its sockets.
        let child_reason = reason
            .clone()
            .unwrap_or_else(|| self.owner_exited_reason(id));
        for child in descendants {
            self.stop(&child, Some(child_reason.clone()))?;
        }
        self.stop(id, reason)
    }
    /// Approval inheritance is deliberately isolated here: it applies downward
    /// on request, never mounts packages proactively or grants them upward.
    fn inherits_package(&self, session: &str, package: &str) -> Option<PathBuf> {
        let mut current = Some(session);
        while let Some(id) = current {
            let a = self.sessions.get(id)?;
            if let Some(output) = a.granted_outputs.get(package) {
                // Reuse the exact granted store output; approval inheritance
                // must not resolve a possibly changed attribute a second time.
                return Some(output.clone());
            }
            current = a.record.parent.as_deref();
        }
        None
    }
    fn detach(&mut self, id: &str) -> std::result::Result<Value, Fault> {
        let a = self.sessions.get_mut(id).ok_or_else(missing)?;
        a.terminal.detach().map_err(|e| (-32009, e.to_string()))?;
        a.record.terminal_attached = a.terminal.attached();
        a.record.terminal_detached = a.terminal.detached;
        self.dirty = true;
        Ok(json!({"accepted":true}))
    }
    fn stop(&mut self, id: &str, reason: Option<String>) -> std::result::Result<Value, Fault> {
        let a = self.sessions.get_mut(id).ok_or_else(missing)?;
        if let Some(w) = &a.worker {
            // Preserve the first cause, including a natural exit with no reason.
            if !["stopping", "stopped", "failed"].contains(&a.record.state.as_str()) {
                a.record.stop_reason = reason;
            }
            w.cancel.cancel();
            a.record.state = "stopping".into();
        }
        a.listener.take();
        a.activity_at = None;
        if a.record.set_agent_state(Default::default()) {
            self.dirty = true;
        }
        if let Some(p) = a.pending.take() {
            p.cancel.cancel();
            if let Some(r) = self.permission_mut(&p.id) {
                r.state = "cancelled".into();
            }
            self.reply_permission(&p.id, "error", Some("session stopped".into()));
            self.operation_finished(&p.id, p.notify, Some("session stopped".into()));
        }
        if let Some(w) = self.sessions.get_mut(id).and_then(|a| a.call.take()) {
            w.cancel.cancel();
            self.reply_waiting(&call_key(w.serial), Err("session stopped".into()));
        }
        let message = format!("goblin {} has stopped", self.sessions[id].record.path);
        self.end_execs(id, &message);
        self.end_edits(id);
        self.dirty = true;
        Ok(json!({"accepted":true}))
    }
    fn reply_permission(&mut self, id: &str, status: &str, message: Option<String>) {
        for c in &mut self.connections {
            if let Some((request, rpc_id)) = &c.waiting
                && request == id
            {
                c.queue(rpc::result(
                    rpc_id.clone(),
                    json!({"request":id,"status":status,"message":message}),
                ));
                c.close = true;
            }
        }
    }
    /// Answer and close the connection waiting under `key`.
    fn reply_waiting(&mut self, key: &str, result: std::result::Result<Value, String>) {
        for c in &mut self.connections {
            if let Some((waiting, rpc_id)) = &c.waiting
                && waiting == key
            {
                c.queue(match &result {
                    Ok(value) => rpc::result(rpc_id.clone(), value.clone()),
                    Err(message) => rpc::error(rpc_id.clone(), -32009, message),
                });
                c.close = true;
            }
        }
    }
    fn withdraw(&mut self, connection: u64) {
        self.exec_withdraw(connection);
        self.edit_withdraw(connection);
        // A disconnected caller no longer wants its app or preview.
        for a in self.sessions.values_mut() {
            if a.call.as_ref().is_some_and(|w| w.connection == connection) {
                a.call.take().unwrap().cancel.cancel();
            }
        }
        let mut gone = None;
        for a in self.sessions.values_mut() {
            if a.pending
                .as_ref()
                .is_some_and(|p| p.connection == connection)
            {
                let p = a.pending.as_ref().unwrap();
                if self
                    .permissions
                    .iter()
                    .any(|r| r.id == p.id && r.state == "pending")
                {
                    let p = a.pending.take().unwrap();
                    p.cancel.cancel();
                    gone = Some((p.id, p.notify));
                    break;
                }
            }
        }
        if let Some((id, notify)) = gone {
            if let Some(r) = self.permission_mut(&id) {
                r.state = "withdrawn".into();
            }
            self.operation_finished(&id, notify, None);
            self.dirty = true;
        }
    }
    fn start(&mut self, mut p: Start, scope: Option<&str>) -> std::result::Result<Value, Fault> {
        if self.shutdown.is_some() {
            return Err(conflict());
        }
        let key = format!("{}:{}", scope.unwrap_or("host"), p.key);
        // A retried recreate returns its original acceptance even though its
        // record is reserved by now; the record fills in everything else.
        let original = p.recreate.is_some().then(|| p.clone());
        if let Some(target) = p.recreate.clone() {
            if let Some((old, result)) = self.launches.get(&key) {
                return if old == &p {
                    Ok(result.clone())
                } else {
                    Err(conflict())
                };
            }
            self.prepare_recreate(&mut p, &target)?;
        }
        let parent = match (scope, p.parent.as_deref()) {
            (Some(root), None | Some(".")) => Some(root.to_string()),
            (_, Some(target)) => Some(self.resolve_scoped(target, scope)?),
            (None, None) => None,
        };
        if parent.is_some() && p.scope.is_some() {
            return Err((-32602, "children inherit their parent's scope".into()));
        }
        if parent.is_some() && p.dev_shell.is_some() {
            return Err((-32602, "children inherit their parent's dev shell".into()));
        }
        let inherited = if let Some(id) = &parent {
            let a = &self.sessions[id];
            if a.record.state != "running" || a.worker.is_none() {
                return Err(conflict());
            }
            // A host --parent launch uses the parent's branch snapshot rather
            // than a newer manifest.
            p.configuration = a.record.configuration.clone();
            p.cwd = None;
            Some(a.inheritance.clone().ok_or_else(conflict)?)
        } else {
            None
        };
        if !goblins_protocol::identifier(&p.key)
            || !goblins_protocol::identifier(&p.name)
            || !Path::new(&p.configuration).is_absolute()
            || p.cwd.as_ref().is_some_and(|cwd| !cwd.is_absolute())
            || p.scope
                .as_deref()
                .is_some_and(|s| !goblins_protocol::scope_name(s))
            || p.dev_shell
                .as_deref()
                .is_some_and(|d| crate::devshell::parse_reference(d).is_err())
            || !dimensions(p.rows, p.cols)
        {
            return Err((-32602, "invalid launch parameters".into()));
        }
        // The selected parent's own list, also for --parent from the host or an
        // ancestor: a caller's permissions are never substituted.
        if let Some(inherited) = &inherited {
            inherited.permit(&p.name).map_err(|e| (-32602, e))?;
        }
        if let Some((old, result)) = self.launches.get(&key) {
            return if old == &p {
                Ok(result.clone())
            } else {
                Err(conflict())
            };
        }
        let join = self.launch_raid(&p, parent.as_deref(), scope)?;
        let agent_name = self.allocate_name(p.agent_name.as_deref(), parent.as_deref())?;
        if self.launches.len() >= 4096
            || self
                .sessions
                .values()
                .filter(|a| a.worker.is_some())
                .count()
                >= SESSIONS
        {
            return Err(capacity());
        }
        if self.sessions.len() >= 128 {
            let old = self
                .sessions
                .iter()
                .filter(|(id, a)| {
                    a.worker.is_none()
                        && a.terminal.can_evict()
                        && !self
                            .sessions
                            .values()
                            .any(|child| child.record.parent.as_ref() == Some(id))
                })
                .min_by_key(|(_, a)| a.created)
                .map(|(id, _)| id.clone())
                .ok_or_else(capacity)?;
            if let Some(a) = self.sessions.remove(&old) {
                // A shared scope can still own a stopped member's resources.
                let _ = fs::remove_file(a.directory.join("terminal.sock"));
                let _ = fs::remove_dir(a.directory);
            }
        }
        let id = self.id("s");
        let directory = self.state.join(&id);
        unix::private_directory(&directory).map_err(|e| (-32010, e.to_string()))?;
        let path = directory.join("terminal.sock");
        let mut terminal = Terminal::new(&path).map_err(|e| (-32010, e.to_string()))?;
        terminal.detached = p.detached;
        let recorded = p.recorded.take();
        // A recreated goblin continues its record; a new root goblin gets a
        // short record id of its own, used once it reaches running.
        let line = match &recorded {
            Some(record) => record.id.clone(),
            None => {
                let store = self.history.as_ref().ok();
                crate::history::new_id(|candidate| {
                    store.is_some_and(|s| s.records.contains_key(candidate))
                        || self.sessions.values().any(|a| a.line == candidate)
                })
                .map_err(|e| (-32010, e.to_string()))?
            }
        };
        let source = match inherited {
            Some(inherited) => Source::Child {
                inherited,
                configuration: p.name.clone(),
            },
            None => Source::Host {
                configuration: p.configuration.clone().into(),
                name: p.name.clone(),
                scope: p.scope.clone(),
                dev_shell: p.dev_shell.clone(),
                workspace: self.workspace.clone(),
                cwd: p.cwd.clone(),
                state: self.state.clone(),
                history: self.history_directory.clone(),
                recreate: recorded.as_ref().map(|r| crate::worker::Recreate {
                    packages: r.packages.approved.clone(),
                    docker: r.docker_enabled,
                    variables: r.variables.clone(),
                }),
            },
        };
        let cwd = match &parent {
            Some(id) => self.sessions[id].record.cwd.clone(),
            None => p.cwd.clone().filter(|_| self.workspace.is_none()),
        };
        let worker = Worker::start(source, directory.join("resources"), (p.rows, p.cols));
        let record = SessionRecord {
            agent_state: Default::default(),
            agent_state_since: None,
            inbox: 0,
            docker_enabled: false,
            scope: None,
            dev_shell: None,
            cwd,
            id: id.clone(),
            path: parent
                .as_ref()
                .map(|id| format!("{}/{}", self.sessions[id].record.path, agent_name))
                .unwrap_or_else(|| agent_name.clone()),
            parent,
            agent_name,
            name: p.name.clone(),
            description: String::new(),
            configuration: p.configuration.clone(),
            state: "starting".into(),
            identity: None,
            initial_packages: vec![],
            packages: vec![],
            terminal: path.display().to_string(),
            pty_eof: false,
            terminal_complete: false,
            terminal_interrupted: false,
            terminal_attached: false,
            terminal_detached: p.detached,
            exit_code: None,
            stop_reason: None,
            detail: None,
            raid: None,
            shells: 0,
            execs: 0,
            recreated_from: recorded.as_ref().map(|r| r.id.clone()),
            skipped: vec![],
        };
        let result = json!({"session":id,"agent_name":record.agent_name,"path":record.path,"parent":record.parent,"state":"starting","terminal":record.terminal});
        self.sessions.insert(
            id.clone(),
            Active {
                activity_at: None,
                inheritance: None,
                exit_watch: None,
                init: None,
                granted_outputs: BTreeMap::new(),
                dev_identity: None,
                call: None,
                variables: recorded
                    .as_ref()
                    .map(|r| r.variables.clone())
                    .unwrap_or_default(),
                line,
                recreated_from: recorded,
                created: self.next,
                record,
                worker: Some(worker),
                listener: None,
                pending: None,
                terminal,
                directory,
            },
        );
        self.launches
            .insert(key, (original.unwrap_or(p), result.clone()));
        self.dirty = true;
        if let Some(join) = join {
            self.apply_launch_raid(&id, join)?;
        }
        Ok(result)
    }
    fn sandbox_control(
        &mut self,
        scope: &str,
        method: &str,
        value: Value,
    ) -> std::result::Result<Value, Fault> {
        match method {
            "sessions.set-state" => {
                let p: goblins_protocol::SetState = params(value)?;
                let a = self.sessions.get_mut(scope).ok_or_else(missing)?;
                if a.record.state != "running" || a.worker.is_none() {
                    return Err(conflict());
                }
                self.dirty |= a.report_activity(p.state);
                Ok(json!({"accepted":true}))
            }
            "sessions.set-variables" => {
                let p: goblins_protocol::SetVariables = params(value)?;
                p.validate().map_err(|e| (-32602, e))?;
                let a = self.sessions.get(scope).ok_or_else(missing)?;
                if a.record.state != "running" || a.worker.is_none() {
                    return Err(conflict());
                }
                if a.record.parent.is_some() {
                    // Children are not recorded; their hooks need no special case.
                    return Ok(json!({"accepted":true,"recorded":false}));
                }
                // Only the caller's own record. Without history the server
                // keeps them, for a record created when it takes the store.
                let line = a.line.clone();
                let existing = self
                    .history
                    .as_ref()
                    .ok()
                    .and_then(|store| store.records.get(&line));
                let recorded = existing.is_some();
                let mut scratch = line_record(a, existing);
                scratch
                    .set_variables(&p.variables)
                    .map_err(|e| (-32010, format!("variables not recorded: {e}")))?;
                self.update_record(&line, |r| r.set_variables(&p.variables))
                    .map_err(|e| (-32010, format!("variables not recorded: {e}")))?;
                self.sessions.get_mut(scope).unwrap().variables = scratch.variables;
                if recorded && p.variables.contains_key("session_id") {
                    self.merge_conversation(&line);
                }
                Ok(json!({"accepted":true,"recorded":recorded}))
            }
            "sessions.status" => {
                let _: Empty = params(value)?;
                let record = &self.sessions[scope].record;
                Ok(json!({"id":record.id,"agent_name":record.agent_name,
                    "name":record.name,"description":record.description,
                    "state":record.state,"agent_state":record.agent_state,"scope":record.scope,"docker_enabled":record.docker_enabled,
                    "dev_shell":record.dev_shell,
                    "allowed_children":self.sessions[scope].inheritance.as_ref()
                        .map_or(&[][..], |i| i.allowed_children()),
                    "raid":record.raid.as_ref().map(|r| json!({"id":r.id,"name":r.name,
                        "role":r.role,"owner":r.owner,
                        "owner_path":self.sessions.get(&r.owner).map(|a| &a.record.path)}))}))
            }
            "devshell.lock" => {
                let _: Empty = params(value)?;
                let dev = self.sessions[scope]
                    .inheritance
                    .as_ref()
                    .and_then(|i| i.dev_shell())
                    .ok_or((
                        -32602,
                        "this sandbox was not started with a dev shell".to_string(),
                    ))?;
                Ok(json!({"path":dev.path.join("flake.lock"),"lock":dev.lock,
                    "generation":dev.generation}))
            }
            "sessions.resize" => {
                let p: Resize = params(value)?;
                let id = self.resolve_scoped(&p.session, Some(scope))?;
                if !dimensions(p.rows, p.cols) {
                    return Err((-32602, "invalid dimensions".into()));
                }
                self.sessions
                    .get_mut(&id)
                    .unwrap()
                    .terminal
                    .resize(p.rows, p.cols)
                    .map_err(|_| conflict())?;
                Ok(json!({"accepted":true}))
            }
            "sessions.list" => {
                let _: Empty = params(value)?;
                Ok(json!(
                    self.sessions
                        .values()
                        .filter(|a| self.descendant(&a.record.id, scope))
                        .map(|a| self.sandbox_record(&a.record, scope))
                        .collect::<Vec<_>>()
                ))
            }
            "sessions.get" => {
                let p: SessionId = params(value)?;
                let id = self.resolve_scoped(&p.session, Some(scope))?;
                Ok(self.sandbox_record(&self.sessions[&id].record, scope))
            }
            "sessions.start" => {
                let p: ChildStart = params(value)?;
                let result = self.start(
                    Start {
                        key: p.key,
                        name: p.name,
                        agent_name: p.agent_name,
                        parent: p.parent,
                        configuration: String::new(),
                        cwd: None,
                        scope: None,
                        dev_shell: None,
                        rows: 24,
                        cols: 80,
                        detached: p.detached,
                        raid: None,
                        inherit_raid: p.inherit_raid,
                        recreate: None,
                        recorded: None,
                    },
                    Some(scope),
                )?;
                let path = result["path"].as_str().ok_or_else(missing)?;
                let relative = path
                    .strip_prefix(&format!("{}/", self.sessions[scope].record.path))
                    .ok_or_else(missing)?;
                Ok(
                    json!({"session":result["session"],"agent_name":result["agent_name"],
                    "path":relative,"state":result["state"]}),
                )
            }
            "sessions.stop" => {
                let p: Stop = params(value)?;
                let id = self.resolve_scoped(&p.session, Some(scope))?;
                let reason = format!(
                    "stopped by goblin {} ({scope})",
                    self.sessions[scope].record.agent_name
                );
                self.stop_tree(&id, p.kill_children, Some(reason))
            }
            m if m.starts_with("raids.") => self.sandbox_raids(scope, m, value),
            _ => Err((-32601, "method unavailable on sandbox endpoint".into())),
        }
    }
    /// `actor` is `host`, or the daemon for an inherited approval.
    fn decide(&mut self, p: Decision, actor: &str) -> std::result::Result<Value, Fault> {
        let r = self
            .permissions
            .iter()
            .find(|r| r.id == p.request)
            .ok_or_else(missing)?;
        if r.session != p.session || r.approval != p.approval || r.state != "pending" {
            return Err(conflict());
        }
        let a = self.sessions.get_mut(&p.session).ok_or_else(missing)?;
        let pending = a
            .pending
            .as_ref()
            .filter(|r| r.id == p.request)
            .ok_or_else(conflict)?;
        // The preview is the approver's whole basis for a refresh, and approval
        // applies the candidate it shows. Denial still cancels the evaluation.
        if p.approved && r.kind == "devshell" && pending.dev_identity.is_none() {
            return Err((
                -32009,
                "the dev shell refresh is still being evaluated; decide once its preview is shown"
                    .into(),
            ));
        }
        // Order a decision against bytes/EOF already present, consuming
        // unexpected input instead of letting it hide a disconnected peer.
        if let Some(peer) = self
            .connections
            .iter_mut()
            .find(|c| c.id == pending.connection)
        {
            let mut byte = [0];
            if !matches!(peer.peer.read(&mut byte),Err(e) if matches!(e.kind(),io::ErrorKind::WouldBlock|io::ErrorKind::Interrupted))
            {
                peer.dead = true;
                let id = peer.id;
                self.withdraw(id);
                return Err(conflict());
            }
        } else {
            return Err(conflict());
        }
        let request = self.permissions.iter().find(|r| r.id == p.request).unwrap();
        let inherited = if request.kind == "package" {
            self.inherits_package(&p.session, &request.package)
        } else {
            None
        };
        let a = self.sessions.get_mut(&p.session).unwrap();
        let pending = a.pending.as_ref().unwrap();
        let notify = pending.notify;
        pending.cancel.cancel();
        if p.approved {
            let r = self.permissions.iter().find(|r| r.id == p.request).unwrap();
            a.worker
                .as_ref()
                .ok_or_else(conflict)?
                .commands
                .try_send(Work::Decide {
                    request: Request {
                        kind: r.kind.clone(),
                        id: p.request.clone(),
                        package: r.package.clone(),
                        reason: r.reason.clone(),
                    },
                    approved: true,
                    output: inherited.or_else(|| pending.initial_output.clone()),
                })
                .map_err(|_| capacity())?;
        } else {
            a.pending.take();
        }
        let r = self.permission_mut(&p.request).unwrap();
        r.approved = Some(p.approved);
        r.state = if p.approved { "realizing" } else { "denied" }.into();
        self.operation(
            &p.request,
            actor,
            crate::operation::Change::Decided {
                approved: p.approved,
            },
        );
        if !p.approved {
            self.reply_permission(&p.request, "denied", None);
            self.operation_finished(&p.request, notify, None);
            let r = self.permissions.iter().find(|r| r.id == p.request).unwrap();
            if actor == "host" && r.kind == "package" {
                let package = r.package.clone();
                // Information only; a recreated goblin may ask again.
                let line = self.sessions[&p.session].line.clone();
                let _ = self.update_record(&line, |r| r.deny(&package));
            }
        }
        self.dirty = true;
        Ok(json!({"accepted":true,"request":p.request,"approved":p.approved}))
    }
    /// A refresh to a generation this sandbox or an ancestor already has
    /// grants no new trust: the host launched or approved that generation.
    fn inherits_dev_shell(&self, session: &str, identity: &str) -> bool {
        let mut current = Some(session);
        while let Some(id) = current {
            let Some(a) = self.sessions.get(id) else {
                return false;
            };
            if a.dev_identity.as_deref() == Some(identity) {
                return true;
            }
            current = a.record.parent.as_deref();
        }
        false
    }
    fn approve_inherited_requests(&mut self) {
        let decisions: Vec<_> = self
            .permissions
            .iter()
            .filter(|r| r.state == "pending" && ["package", "devshell"].contains(&r.kind.as_str()))
            .filter(|r| {
                self.sessions.get(&r.session).is_some_and(|a| {
                    a.record.state == "running"
                        && a.pending.as_ref().is_some_and(|p| {
                            if r.kind == "devshell" {
                                return p
                                    .dev_identity
                                    .as_deref()
                                    .is_some_and(|i| self.inherits_dev_shell(&r.session, i));
                            }
                            p.initial_output.is_some()
                                || self.inherits_package(&r.session, &r.package).is_some()
                        })
                })
            })
            .map(|r| Decision {
                session: r.session.clone(),
                request: r.id.clone(),
                approval: r.approval.clone(),
                approved: true,
            })
            .collect();
        for decision in decisions {
            let _ = self.decide(decision, goblins_protocol::messages::NOTICE_SENDER);
        }
    }
    fn host(
        &mut self,
        c: &mut Connection,
        method: &str,
        value: Value,
    ) -> std::result::Result<Value, Fault> {
        if self.shutdown.is_some() && !["server.status", "server.stop"].contains(&method) {
            return Err((-32009, "server is stopping".into()));
        }
        match method {
            "server.status" => {
                let _: Empty = params(value)?;
                Ok(
                    json!({"instance":self.instance,"state":if self.shutdown.is_some(){"stopping"}else{"running"},"sessions":self.sessions.values().filter(|a| a.worker.is_some()).count(),"history":self.history_status()}),
                )
            }
            "server.stop" => {
                let _: Empty = params(value)?;
                // Flush acceptance before teardown, but a non-reading host
                // cannot prevent shutdown by holding its response queue full.
                self.shutdown
                    .get_or_insert((c.id, Instant::now() + std::time::Duration::from_secs(1)));
                c.subscription = None;
                c.close = true;
                Ok(json!({"accepted":true}))
            }
            "sessions.list" => {
                let _: Empty = params(value)?;
                Ok(json!(self.snapshot().sessions))
            }
            "sessions.get" => {
                let p: SessionId = params(value)?;
                let id = self.resolve_session(&p.session)?;
                Ok(json!(self.sessions[&id].record))
            }
            "sessions.start" => self.start(params(value)?, None),
            "history.list" => {
                let _: Empty = params(value)?;
                self.history_list()
            }
            "sessions.stop" => {
                let p: Stop = params(value)?;
                let id = self.resolve_session(&p.session)?;
                self.stop_tree(&id, p.kill_children, Some("stopped by host".into()))
            }
            "sessions.detach" => {
                let p: SessionId = params(value)?;
                let id = self.resolve_session(&p.session)?;
                self.detach(&id)
            }
            "sessions.kill_execs" => {
                let p: SessionId = params(value)?;
                self.kill_execs(&p.session)
            }
            "sessions.resize" => {
                let p: Resize = params(value)?;
                if !dimensions(p.rows, p.cols) {
                    return Err((-32602, "invalid dimensions".into()));
                }
                self.sessions
                    .get_mut(&p.session)
                    .ok_or_else(missing)?
                    .terminal
                    .resize(p.rows, p.cols)
                    .map_err(|_| conflict())?;
                Ok(json!({"accepted":true}))
            }
            "permissions.list" => {
                let p: Filter = params(value)?;
                Ok(json!(
                    self.permissions
                        .iter()
                        .filter(|r| p.session.as_ref().is_none_or(|s| *s == r.session)
                            && p.state.as_ref().is_none_or(|s| *s == r.state))
                        .collect::<Vec<_>>()
                ))
            }
            "permissions.get" => {
                let p: RequestId = params(value)?;
                Ok(json!(
                    self.permissions
                        .iter()
                        .find(|r| r.id == p.request)
                        .ok_or_else(missing)?
                ))
            }
            "permissions.decide" => self.decide(params(value)?, "host"),
            "state.subscribe" => {
                let _: Empty = params(value)?;
                if c.subscription.is_some() {
                    return Err(conflict());
                }
                // Every earlier mutation is published before taking this snapshot.
                self.publish();
                let id = self.id("sub");
                c.subscription = Some(id.clone());
                Ok(json!({"subscription":id,"sequence":self.sequence,"snapshot":self.snapshot()}))
            }
            "state.unsubscribe" => {
                let p: Subscription = params(value)?;
                if c.subscription.as_deref() != Some(&p.subscription) {
                    return Err(conflict());
                }
                c.subscription = None;
                Ok(json!({"accepted":true}))
            }
            m if m.starts_with("raids.") => self.host_raids(m, value),
            _ => Err((-32601, "method unavailable on host endpoint".into())),
        }
    }
    fn dispatch(&mut self, c: &mut Connection, call: rpc::Call) {
        // Every authorization below sees current membership.
        self.reconcile_raids();
        // JSON-RPC notifications get no response and cannot authorize mutations.
        let Some(id) = call.id else {
            return;
        };
        let outcome = (|| -> std::result::Result<Option<Value>, Fault> {
            let messaging = call.method.starts_with("integration.")
                || matches!(
                    call.method.as_str(),
                    "messages.send"
                        | "messages.get"
                        | "messages.reply"
                        | "inbox.next"
                        | "inbox.status"
                        | "inbox.complete"
                        | "inbox.requeue"
                        | "communications.list"
                );
            // Sixteen variables of 256 bytes, escaped, exceed the usual frame.
            let legacy_limit =
                if matches!(c.role, Role::Host) || call.method == "sessions.set-variables" {
                    16384
                } else {
                    4096
                };
            if !messaging && c.body_bytes > legacy_limit {
                return Err((-32602, "request exceeds method frame limit".into()));
            }
            if call.method == "initialize" {
                let p: Initialize = params(call.params)?;
                if c.initialized {
                    return Err(conflict());
                }
                if p.api != 1 {
                    return Err((-32002, "unsupported Goblins API".into()));
                }
                c.initialized = true;
                c.decoder = if matches!(c.role, Role::Sandbox(_)) {
                    Decoder::new(
                        goblins_protocol::messages::MAX_REQUEST,
                        Some(Instant::now()),
                    )
                } else {
                    Decoder::new(goblins_protocol::messages::MAX_REQUEST, None)
                };
                return Ok(Some(
                    json!({"api":1,"instance":self.instance,"configuration":match &c.role { Role::Sandbox(id) => self.sessions.get(id).map(|a| a.record.name.as_str()), Role::Host => None },"role":if matches!(c.role,Role::Host){"host"}else{"sandbox"},"features":if matches!(c.role,Role::Host){vec!["scopes","docker-enable","package-grants","same-daemon-reconnect","state-subscribe","agent-activity-v1","raw-terminal","agent-names","server-control","terminal-reattach","agent-inbox-v1","agent-integration-v1","communications-log-v1","allowed-children","raids","exec","recreate"]}else{vec!["scopes","docker-enable","package-grants","terminal-detach","subtree-control","subtree-terminal","sandbox-status","agent-activity-v1","agent-inbox-v1","agent-integration-v1","flake-run","devshell-diff","allowed-children","raids","editor-open","variables"]},"limits":{"header":256,"body":goblins_protocol::messages::MAX_REQUEST,"frame_seconds":3,"depth":32,"response_body":rpc::MAX_BODY,"calls":if matches!(c.role,Role::Host){4096}else{2},"connections":if matches!(c.role,Role::Host){HOSTS}else{8},"sessions":SESSIONS,"output_queue":QUEUE,"snapshot":900*1024,"terminal_buffer":65536}}),
                ));
            }
            if !c.initialized {
                return Err((-32001, "initialize first".into()));
            }
            if messaging {
                if self.shutdown.is_some() {
                    return Err(conflict());
                }
                if call.method == "communications.list" && !matches!(c.role, Role::Host) {
                    return Err((-32601, "method unavailable on sandbox endpoint".into()));
                }
                let actor = match &c.role {
                    Role::Host => "host".to_owned(),
                    Role::Sandbox(id) => id.clone(),
                };
                let mut call_params = call.params;
                let view = if call.method.starts_with("integration.") {
                    let target = if actor == "host" {
                        let target = call_params["session"].as_str().ok_or_else(missing)?;
                        let resolved = self.resolve_session(target)?;
                        call_params["session"] = json!(resolved);
                        resolved
                    } else {
                        actor.clone()
                    };
                    let a = self.sessions.get(&target).ok_or_else(missing)?;
                    if a.record.state != "running" {
                        return Err(conflict());
                    }
                    let integration = a.inheritance.as_ref().and_then(|i| i.integration());
                    if !matches!(integration, Some("codex" | "claude")) {
                        return Err((-32009, "session has no supported integration".into()));
                    }
                    if call.method == "integration.register"
                        && call_params["driver"].as_str() != integration
                    {
                        return Err((-32009, "integration driver does not match session".into()));
                    }
                    a.terminal.integration_view()
                } else {
                    crate::integration::View::default()
                };
                // The worker records queued membership before this call.
                let work = crate::messaging::Work::Call {
                    connection: c.id,
                    id: id.clone(),
                    actor,
                    method: call.method,
                    params: call_params,
                    directory: self.message_directory(),
                    view,
                    raids: self.raid_outbox.drain(..).collect(),
                    operations: self.operation_outbox.drain(..).collect(),
                };
                if let Err(e) = self.messaging.commands.try_send(work) {
                    if let crate::messaging::Work::Call {
                        raids, operations, ..
                    } = unsent(e)
                    {
                        self.raid_outbox = raids.into();
                        self.operation_outbox = operations.into();
                    }
                    return Err(capacity());
                }
                c.waiting = Some(("mailbox".into(), id.clone()));
                return Ok(None);
            }
            match &c.role {
                Role::Host if call.method == "sessions.exec" => {
                    self.exec_start(c, id.clone(), call.params).map(|()| None)
                }
                Role::Host if call.method == "history.prune" => {
                    self.history_prune(c, id.clone(), call.params)
                }
                Role::Host => self.host(c, &call.method, call.params).map(Some),
                Role::Sandbox(session) => {
                    let session = session.clone();
                    if self.shutdown.is_some()
                        || !self
                            .sessions
                            .get(&session)
                            .is_some_and(|a| a.record.state == "running" && a.worker.is_some())
                    {
                        return Err(conflict());
                    }
                    if call.method == "sessions.detach" {
                        let _: Empty = params(call.params)?;
                        if !c.input.is_empty() {
                            c.dead = true;
                            return Err((-32600, "unexpected trailing input".into()));
                        }
                        // Endpoint identity selects this sandbox. No caller-
                        // supplied session/name can affect another sandbox.
                        let session = session.clone();
                        let result = self.detach(&session)?;
                        c.close = true;
                        return Ok(Some(result));
                    }
                    if call.method == "sessions.attach" {
                        let p: SessionId = params(call.params)?;
                        let target = self.resolve_scoped(&p.session, Some(&session))?;
                        // Upgrade only after initialize has drained. Reject
                        // pipelined frames instead of interpreting them as keys.
                        if !c.input.is_empty() || !c.output.is_empty() {
                            return Err((-32600, "unexpected pipelined input".into()));
                        }
                        let peer = c.peer.try_clone().map_err(|_| conflict())?;
                        self.sessions
                            .get_mut(&target)
                            .unwrap()
                            .terminal
                            .attach(peer, id.clone())
                            .map_err(|e| (-32009, e.to_string()))?;
                        c.dead = true;
                        self.dirty = true;
                        return Ok(None);
                    }
                    if call.method == "editor.open" {
                        if !c.input.is_empty() {
                            c.dead = true;
                            return Err((-32600, "unexpected trailing input".into()));
                        }
                        return self
                            .editor_open(c, &session, id.clone(), call.params)
                            .map(|()| None);
                    }
                    if matches!(call.method.as_str(), "flake.run" | "devshell.diff") {
                        if !c.input.is_empty() {
                            c.dead = true;
                            return Err((-32600, "unexpected trailing input".into()));
                        }
                        let serial = self.number();
                        let work: Box<dyn FnOnce(Cancel) -> Work> = if call.method == "flake.run" {
                            let p: FlakeRunParams = params(call.params)?;
                            if !p.validate() {
                                return Err((-32602, "invalid app name".into()));
                            }
                            // The trusted generation's own source and lock grant
                            // no new trust, so nothing is asked of the host.
                            Box::new(move |cancel| Work::Run {
                                serial,
                                app: p.app,
                                cancel,
                            })
                        } else {
                            let p: DevShellDiffParams = params(call.params)?;
                            if !p.validate() {
                                return Err((-32602, "invalid flake inputs".into()));
                            }
                            // A preview only: no record, no candidate, no change.
                            Box::new(move |cancel| Work::Diff {
                                serial,
                                changes: crate::devshell::Changes {
                                    update: p.update,
                                    lock: p.lock,
                                },
                                cancel,
                            })
                        };
                        let a = self.sessions.get_mut(&session).ok_or_else(missing)?;
                        if a.record.dev_shell.is_none() {
                            return Err((
                                -32602,
                                "this sandbox was not started with a dev shell".into(),
                            ));
                        }
                        if a.call.is_some() {
                            return Err((
                                -32009,
                                "another flake run or dev shell diff is in progress".into(),
                            ));
                        }
                        let w = a.worker.as_ref().ok_or_else(conflict)?;
                        let cancel = w.cancel.child();
                        w.commands
                            .try_send(work(cancel.clone()))
                            .map_err(|_| capacity())?;
                        a.call = Some(Call {
                            serial,
                            connection: c.id,
                            cancel,
                        });
                        c.waiting = Some((call_key(serial), id.clone()));
                        return Ok(None);
                    }
                    if call.method != "permissions.request" {
                        let result = self.sandbox_control(&session, &call.method, call.params)?;
                        c.close = true;
                        return Ok(Some(result));
                    }
                    if !c.input.is_empty() {
                        c.dead = true;
                        return Err((-32600, "unexpected trailing input".into()));
                    }
                    let mut p: PermissionParams = params(call.params)?;
                    if !p.validate() {
                        return Err((-32602, "invalid permission request or reason".into()));
                    }
                    let session = session.clone();
                    let a = self.sessions.get(&session).ok_or_else(missing)?;
                    if a.record.state != "running" || a.pending.is_some() {
                        return Err(conflict());
                    }
                    let docker_scope = a
                        .inheritance
                        .as_ref()
                        .filter(|i| i.docker_available())
                        .and_then(|i| i.scope_name())
                        .map(str::to_string);
                    if p.kind == "docker" {
                        let Some(scope) = &docker_scope else {
                            return Err((
                                -32602,
                                "Docker requires a scope with docker.enable".into(),
                            ));
                        };
                        p.package = format!("Docker scope: {scope}");
                    }
                    if p.kind == "package" && a.record.packages.len() >= 64 {
                        return Err(capacity());
                    }
                    if p.kind == "devshell" {
                        if a.record.dev_shell.is_none() {
                            return Err((
                                -32602,
                                "this sandbox was not started with a dev shell".into(),
                            ));
                        }
                        p.package = match (&p.update, p.lock) {
                            (Some(inputs), _) if inputs.is_empty() => {
                                "refresh --update (all inputs)".into()
                            }
                            (Some(inputs), _) => format!("refresh --update {}", inputs.join(" ")),
                            (None, true) => "refresh --lock".into(),
                            (None, false) => "refresh".into(),
                        };
                    }
                    // Completions must not be dropped, so a backlog the
                    // messaging worker has not accepted refuses new requests.
                    if self.operation_outbox.len() >= 1024 {
                        return Err(capacity());
                    }
                    if self.permissions.len() >= 256 {
                        let index = self
                            .permissions
                            .iter()
                            .position(|r| {
                                !["pending", "realizing", "mounting", "publishing"]
                                    .contains(&r.state.as_str())
                            })
                            .ok_or_else(capacity)?;
                        self.permissions.remove(index);
                    }
                    let serial = self.number();
                    let request = format!("{}-r{serial}", self.instance);
                    let approval = format!("{}-a{serial}", self.instance);
                    let inherited_output = if p.kind == "package" {
                        self.inherits_package(&session, &p.package)
                    } else {
                        None
                    };
                    let auto_approve = inherited_output.is_some()
                        || (p.kind == "docker" && self.sessions[&session].record.docker_enabled);
                    let a = self.sessions.get_mut(&session).unwrap();
                    let notify = a
                        .inheritance
                        .as_ref()
                        .and_then(|i| i.integration())
                        .is_some();
                    let w = a.worker.as_ref().ok_or_else(conflict)?;
                    let cancel = w.cancel.child();
                    if p.kind == "devshell" {
                        w.commands
                            .try_send(Work::Refresh {
                                approval: serial,
                                changes: crate::devshell::Changes {
                                    update: p.update.clone(),
                                    lock: p.lock,
                                },
                                cancel: cancel.clone(),
                            })
                            .map_err(|_| capacity())?;
                    }
                    if auto_approve || p.kind == "package" {
                        w.commands
                            .try_send(if auto_approve {
                                Work::Decide {
                                    request: Request {
                                        kind: p.kind.clone(),
                                        id: request.clone(),
                                        package: p.package.clone(),
                                        reason: p.reason.clone(),
                                    },
                                    approved: true,
                                    output: inherited_output,
                                }
                            } else {
                                Work::Preview {
                                    approval: serial,
                                    package: p.package.clone(),
                                    cancel: cancel.clone(),
                                }
                            })
                            .map_err(|_| capacity())?;
                    }
                    a.pending = Some(Pending {
                        initial_output: None,
                        dev_identity: None,
                        id: request.clone(),
                        serial,
                        connection: c.id,
                        cancel,
                        notify,
                    });
                    self.permissions.push_back(PermissionRecord {
                        kind: p.kind.clone(),
                        id: request.clone(),
                        session,
                        agent_name: a.record.agent_name.clone(),
                        approval,
                        package: p.package,
                        reason: p.reason,
                        state: if auto_approve { "realizing" } else { "pending" }.into(),
                        approved: if auto_approve { Some(true) } else { None },
                        preview: docker_scope.filter(|_| p.kind == "docker").map(|scope| json!({"description": format!("Attach to the shared Docker engine of scope '{scope}': members control the same containers, volumes, network, and combined filesystem grants. Children inherit the attachment.")})),
                        message: None,
                    });
                    let r = self.permissions.back().unwrap();
                    let change = crate::operation::Change::Requested {
                        kind: r.kind.clone(),
                        subject: r.package.clone(),
                        reason: r.reason.clone(),
                        automatic: auto_approve,
                    };
                    let session = r.session.clone();
                    self.operation(&request, &session, change);
                    c.waiting = Some((request, id.clone()));
                    self.dirty = true;
                    Ok(None)
                }
            }
        })();
        match outcome {
            Ok(Some(result)) => c.queue(rpc::result(id, result)),
            Ok(None) => (),
            Err((code, message)) => {
                c.queue(rpc::error(id, code, &message));
                if matches!(c.role, Role::Sandbox(_)) {
                    c.close = true;
                }
            }
        }
    }
    fn message_directory(&self) -> crate::mailbox::Directory {
        self.sessions
            .iter()
            .map(|(id, a)| {
                (
                    id.clone(),
                    crate::mailbox::Session {
                        id: id.clone(),
                        parent: a.record.parent.clone(),
                        path: a.record.path.clone(),
                        running: matches!(a.record.state.as_str(), "starting" | "running"),
                    },
                )
            })
            .collect()
    }
    /// Read and handle at most one complete frame from `c`.
    fn service(&mut self, c: &mut Connection) {
        if let Some(call) = c.read() {
            match c.exec {
                Some(exec) => self.exec_input(c, exec, call),
                None => self.dispatch(c, call),
            }
            // Descriptors arrive with their frame's first bytes, possibly
            // several reads before it completes: they wait until it does,
            // and those the completed frame did not take are closed.
            c.fds.clear();
        }
    }
    /// Claude's integration hooks and monitor ticks are its activity reports.
    fn claude_activity(&mut self, response: &crate::messaging::Completed) {
        if let Ok(result) = &response.result
            && let Ok(state) = serde_json::from_value(result["agent_state"].clone())
            && let Some(a) = self.sessions.get_mut(&response.actor)
            && a.record.state == "running"
            && a.worker.is_some()
            && a.inheritance.as_ref().and_then(|i| i.integration()) == Some("claude")
        {
            self.dirty |= a.report_activity(state);
        }
    }
    /// Each goblin's queued inbox count, as the messaging worker reported it.
    fn inbox_counts(&mut self, counts: &BTreeMap<String, u64>) {
        for (id, a) in &mut self.sessions {
            let queued = counts.get(id).copied().unwrap_or(0);
            if a.record.inbox != queued {
                a.record.inbox = queued;
                self.dirty = true;
            }
        }
    }
    fn expire_activity(&mut self, now: Instant) {
        for a in self.sessions.values_mut() {
            if a.record.state != "running"
                || a.activity_at.is_none_or(|at| {
                    now.saturating_duration_since(at) >= std::time::Duration::from_secs(30)
                })
            {
                a.activity_at = None;
                self.dirty |= a.record.set_agent_state(Default::default());
            }
        }
    }
    pub fn tick(&mut self) -> Result<()> {
        self.expire_activity(Instant::now());
        self.poll_history();
        self.poll_deletes();
        // Drain human input and native output before checking wake revisions.
        for a in self.sessions.values_mut() {
            if let Err(e) = a.terminal.tick() {
                a.record.detail = Some(bounded(&e.to_string()));
                a.terminal.interrupted = true;
            }
        }
        if let Some(counts) = self.messaging.inbox.try_iter().last() {
            self.inbox_counts(&counts);
        }
        let responses: Vec<_> = self.messaging.results.try_iter().collect();
        for mut response in responses {
            self.claude_activity(&response);
            if let Ok(result) = &mut response.result
                && result["notify"] == true
                && result["delivery"] != "notification"
            {
                let outcome = self
                    .sessions
                    .get_mut(&response.actor)
                    .filter(|a| a.record.state == "running")
                    .ok_or("session exited")
                    .and_then(|a| {
                        a.terminal
                            .notify(result["revision"].as_u64().unwrap_or(0))
                            .map_err(|_| "terminal changed")
                    });
                let _ = self
                    .messaging
                    .commands
                    .try_send(crate::messaging::Work::Delivery {
                        actor: response.actor.clone(),
                        revision: result["revision"].as_u64().unwrap_or(0),
                        submitted: outcome.is_ok(),
                    });
                result["delivery"] = json!(if outcome.is_ok() {
                    "submitted"
                } else {
                    "deferred"
                });
            }
            if let Some(c) = self
                .connections
                .iter_mut()
                .find(|c| c.id == response.connection)
            {
                c.waiting = None;
                c.close = matches!(c.role, Role::Sandbox(_));
                c.queue(match response.result {
                    Ok(result) => rpc::result(response.id, result),
                    Err((code, message)) => rpc::error(response.id, code, &message),
                });
            }
        }
        self.poll_init();
        self.poll_edits();
        for _ in 0..HOSTS {
            match self.listener.accept() {
                Ok((p, _))
                    if self
                        .connections
                        .iter()
                        .filter(|c| matches!(c.role, Role::Host))
                        .count()
                        < HOSTS =>
                {
                    let id = self.number();
                    self.connections.push(Connection::new(id, Role::Host, p)?);
                }
                Ok(_) => (),
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => break,
                Err(e) => return Err(e.into()),
            }
        }
        let ids: Vec<_> = self.sessions.keys().cloned().collect();
        for id in ids {
            let mut a = self.sessions.remove(&id).unwrap();
            for _ in 0..8 {
                let Some(listener) = &a.listener else {
                    break;
                };
                match listener.accept() {
                    Ok((p, _))
                        if self
                            .connections
                            .iter()
                            .filter(|c| matches!(&c.role,Role::Sandbox(s) if s==&id))
                            .count()
                            < 8 =>
                    {
                        let cid = self.number();
                        self.connections
                            .push(Connection::new(cid, Role::Sandbox(id.clone()), p)?);
                    }
                    Ok(_) => (),
                    Err(e) if e.kind() == io::ErrorKind::WouldBlock => break,
                    Err(e) => return Err(e.into()),
                }
            }
            let (results, worker_finished) =
                a.worker.as_ref().map(Worker::poll).unwrap_or_default();
            for result in results {
                match result {
                    Completed::Started {
                        docker_enabled,
                        scope,
                        dev_shell,
                        dev_identity,
                        initial_packages,
                        master,
                        control,
                        listener,
                        identity,
                        inheritance,
                        exit_watch,
                        recreated,
                    } => {
                        a.init = Some(control);
                        // Recreated grants are this goblin's own: children
                        // asking for them are approved automatically.
                        if let Some(recreated) = recreated {
                            for (name, output) in recreated.packages {
                                if !a.record.packages.contains(&name) {
                                    a.record.packages.push(name.clone());
                                }
                                a.granted_outputs.insert(name, output);
                            }
                            a.record.skipped = recreated.skipped;
                            a.record.detail = recreated.fresh.map(|reason| {
                                bounded(&format!("resumed in a fresh conversation: {reason}"))
                            });
                        }
                        a.record.docker_enabled = docker_enabled;
                        a.record.scope = scope;
                        a.record.dev_shell = dev_shell;
                        a.dev_identity = dev_identity;
                        a.record.description = inheritance.description().to_owned();
                        a.exit_watch = Some(exit_watch);
                        a.inheritance = Some(inheritance);
                        a.terminal.master(master)?;
                        a.record.initial_packages = initial_packages;
                        a.record.identity = Some(identity);
                        if a.record.state == "starting" {
                            a.record.state = "running".into();
                            a.listener = Some(listener);
                            self.record_started(&a);
                        }
                        self.dirty = true;
                    }
                    Completed::Preview { approval, result } => {
                        if let Some(p) = &mut a.pending
                            && p.serial == approval
                            && let Some(r) = self.permission_mut(&p.id)
                            && r.state == "pending"
                        {
                            r.preview = Some(match result {
                                Ok(preview) => {
                                    p.initial_output = preview
                                        .output_path
                                        .as_ref()
                                        .filter(|path| {
                                            a.inheritance
                                                .as_ref()
                                                .is_some_and(|i| i.initially_available(path))
                                        })
                                        .cloned();
                                    json!(preview)
                                }
                                Err(e) => json!({"error":bounded(&e)}),
                            });
                            self.dirty = true;
                        }
                    }
                    Completed::Refresh { approval, result } => {
                        let Some(p) = a.pending.as_mut().filter(|p| p.serial == approval) else {
                            continue;
                        };
                        let id = p.id.clone();
                        if !self
                            .permissions
                            .iter()
                            .any(|r| r.id == id && r.state == "pending")
                        {
                            continue;
                        }
                        match result {
                            Ok((preview, identity)) => {
                                p.dev_identity = Some(identity);
                                let r = self.permission_mut(&id).unwrap();
                                r.preview = Some(preview);
                            }
                            Err(error) => {
                                // A foreign lock or failed evaluation fails the
                                // request; the agent gets the reason to act on.
                                let p = a.pending.take().unwrap();
                                p.cancel.cancel();
                                let r = self.permission_mut(&id).unwrap();
                                r.state = "failed".into();
                                r.message = Some(bounded(&error));
                                self.reply_permission(&id, "error", Some(bounded(&error)));
                                self.operation_finished(&id, p.notify, Some(bounded(&error)));
                            }
                        }
                        self.dirty = true;
                    }
                    Completed::Call { serial, result } => {
                        if a.call.as_ref().is_some_and(|w| w.serial == serial) {
                            a.call.take();
                            self.reply_waiting(&call_key(serial), result.map_err(|e| bounded(&e)));
                        }
                    }
                    Completed::DevShell {
                        identity,
                        inheritance,
                    } => {
                        a.dev_identity = Some(identity);
                        a.inheritance = Some(inheritance);
                        self.dirty = true;
                    }
                    Completed::Progress { request, state } => {
                        if a.pending.as_ref().is_some_and(|p| p.id == request)
                            && let Some(r) = self.permission_mut(&request)
                            && ["realizing", "mounting", "publishing"].contains(&r.state.as_str())
                        {
                            r.state = state.into();
                            self.dirty = true;
                        }
                    }
                    Completed::Granted {
                        reply,
                        detail,
                        output,
                    } => {
                        let mut granted = None;
                        if let Some(p) = a.pending.take() {
                            p.cancel.cancel();
                            if let Some(r) = self.permission_mut(&p.id) {
                                r.state = if reply.status == "ready" {
                                    "ready"
                                } else {
                                    "failed"
                                }
                                .into();
                                // The sandbox gets a concise category; host
                                // frontends need the diagnostic for this request.
                                r.message = detail
                                    .as_deref()
                                    .map(bounded)
                                    .or_else(|| reply.message.clone());
                                if reply.status == "ready" {
                                    granted = Some((r.kind.clone(), r.package.clone()));
                                }
                                if reply.status == "ready" && r.kind == "docker" {
                                    a.record.docker_enabled = true;
                                }
                                if reply.status == "ready"
                                    && r.kind == "package"
                                    && !a.record.packages.contains(&r.package)
                                {
                                    a.record.packages.push(r.package.clone());
                                    if let Some(output) = output {
                                        a.granted_outputs.insert(r.package.clone(), output);
                                    }
                                }
                            }
                            self.operation_finished(&p.id, p.notify, reply.message.clone());
                            self.reply_permission(&p.id, &reply.status, reply.message);
                        }
                        a.record.detail = detail.map(|e| bounded(&e));
                        if let Some((kind, package)) = granted {
                            let recorded = if kind == "docker" {
                                self.update_record(&a.line, |r| {
                                    r.update(|r| r.docker_enabled = true)
                                })
                            } else if kind == "package" {
                                self.update_record(&a.line, |r| r.approve(&package))
                            } else {
                                Ok(false)
                            };
                            if let Err(reason) = recorded {
                                let subject = if kind == "docker" {
                                    "Docker".to_string()
                                } else {
                                    format!("package '{package}'")
                                };
                                a.record.detail = Some(bounded(&format!(
                                    "{subject} is granted but not recorded for resume: {reason}"
                                )));
                            }
                        }
                        self.dirty = true;
                    }
                    Completed::Failed(e) => {
                        a.record.state = "failed".into();
                        a.record.detail = Some(bounded(&e));
                        a.terminal.failed_start();
                        self.dirty = true;
                    }
                    Completed::Stopped { exit_code } => {
                        if a.record.identity.is_none() {
                            a.terminal.failed_start();
                        }
                        if a.record.state != "failed" {
                            a.record.state = "stopped".into();
                        }
                        a.record.exit_code = exit_code;
                        a.listener.take();
                        if let Some(p) = a.pending.take() {
                            p.cancel.cancel();
                            if let Some(r) = self.permission_mut(&p.id) {
                                r.state = "cancelled".into();
                            }
                            self.reply_permission(&p.id, "error", Some("payload stopped".into()));
                            self.operation_finished(
                                &p.id,
                                p.notify,
                                Some("payload stopped".into()),
                            );
                        }
                        if let Some(w) = a.call.take() {
                            w.cancel.cancel();
                            self.reply_waiting(&call_key(w.serial), Err("payload stopped".into()));
                        }
                        self.dirty = true;
                    }
                }
            }
            if worker_finished {
                a.worker.take();
                a.inheritance.take();
                a.exit_watch.take();
                a.init.take();
                // A worker can lose its final try_send when its bounded result
                // queue is full (or panic). Never retain a live session after
                // its producer is gone, even without a Stopped result.
                if !["stopped", "failed"].contains(&a.record.state.as_str()) {
                    a.record.state = "failed".into();
                    a.record.detail =
                        Some("session worker exited without a final lifecycle result".into());
                    a.listener.take();
                    if a.record.identity.is_none() {
                        a.terminal.failed_start();
                    }
                    if let Some(p) = a.pending.take() {
                        p.cancel.cancel();
                        if let Some(r) = self.permission_mut(&p.id) {
                            r.state = "cancelled".into();
                        }
                        self.reply_permission(&p.id, "error", Some("session worker exited".into()));
                        self.operation_finished(
                            &p.id,
                            p.notify,
                            Some("session worker exited".into()),
                        );
                    }
                    self.dirty = true;
                }
            }
            let old = (
                a.terminal.eof,
                a.terminal.complete,
                a.terminal.interrupted,
                a.terminal.attached(),
                a.terminal.detached,
            );
            if let Err(e) = a.terminal.tick() {
                a.record.detail = Some(bounded(&e.to_string()));
                a.terminal.interrupted = true;
            }
            if old
                != (
                    a.terminal.eof,
                    a.terminal.complete,
                    a.terminal.interrupted,
                    a.terminal.attached(),
                    a.terminal.detached,
                )
            {
                self.dirty = true;
            }
            a.record.pty_eof = a.terminal.eof;
            a.record.terminal_complete = a.terminal.complete;
            a.record.terminal_interrupted = a.terminal.interrupted;
            a.record.terminal_attached = a.terminal.attached();
            a.record.terminal_detached = a.terminal.detached;
            // Observe process exit independently of a worker blocked on Nix.
            // A pidfd pins the process identity, avoiding PID-reuse races.
            let exited = a.record.state == "running"
                && a.exit_watch
                    .as_ref()
                    .is_some_and(|fd| unix::readable(fd.as_raw_fd(), 0).unwrap_or(true));
            let stopped = ["stopped", "failed"].contains(&a.record.state.as_str());
            if a.record.state != "running" {
                a.activity_at = None;
                self.dirty |= a.record.set_agent_state(Default::default());
            }
            self.sessions.insert(id.clone(), a);
            if stopped {
                let now = crate::messaging::now();
                let line = self.sessions[&id].line.clone();
                // Only the goblin the record last ran as ends it.
                let _ = self.update_record(&line, |r| {
                    if r.latest() == id {
                        r.update(|r| r.stopped = Some(now))
                    } else {
                        Ok(false)
                    }
                });
            }
            if exited {
                let _ = self.stop_tree(&id, true, None);
            } else if stopped {
                if self.execs.values().any(|e| e.session() == id) {
                    let message = format!("goblin {} has stopped", self.sessions[&id].record.path);
                    self.end_execs(&id, &message);
                }
                self.end_edits(&id);
                let children: Vec<_> = self
                    .sessions
                    .iter()
                    .filter(|(child, a)| {
                        a.worker.is_some()
                            && !["stopped", "failed", "stopping"].contains(&a.record.state.as_str())
                            && self.descendant(child, &id)
                    })
                    .map(|(id, _)| id.clone())
                    .collect();
                for child in children {
                    let reason = self.owner_exited_reason(&id);
                    let _ = self.stop(&child, Some(reason));
                }
            }
        }
        self.reconcile_raids();
        self.approve_inherited_requests();
        let directory = self.message_directory();
        // Queued raid events go out even when the directory is unchanged. A
        // batch is cleared once accepted and kept for the next tick otherwise.
        if directory != self.messaging_directory
            || !self.raid_outbox.is_empty()
            || !self.operation_outbox.is_empty()
        {
            let work = crate::messaging::Work::Sync {
                directory: directory.clone(),
                raids: self.raid_outbox.drain(..).collect(),
                operations: self.operation_outbox.drain(..).collect(),
            };
            match self.messaging.commands.try_send(work) {
                Ok(()) => self.messaging_directory = directory,
                Err(e) => {
                    if let crate::messaging::Work::Sync {
                        raids, operations, ..
                    } = unsent(e)
                    {
                        self.raid_outbox = raids.into();
                        self.operation_outbox = operations.into();
                    }
                }
            }
        }
        // Service pending sandbox connections before host decisions in this tick.
        self.connections
            .sort_by_key(|c| matches!(c.role, Role::Host));
        let ids: Vec<_> = self.connections.iter().map(|c| c.id).collect();
        for id in ids {
            let Some(index) = self.connections.iter().position(|c| c.id == id) else {
                continue;
            };
            let mut c = self.connections.remove(index);
            self.service(&mut c);
            c.flush();
            if c.dead {
                self.withdraw(c.id);
            } else {
                self.connections.push(c);
            }
            // Publishing here ensures a subscription snapshot and later events
            // share one serialization order, including competing host clients.
            self.publish();
        }
        self.publish();
        Ok(())
    }
}
/// A root goblin's record as it stands. A recreated goblin continues the
/// record it came from, `existing` when the store still has it; a new one
/// starts its own. Either way it adds what this goblin was granted.
fn line_record(a: &Active, existing: Option<&crate::history::Record>) -> crate::history::Record {
    let now = crate::messaging::now();
    let mut record = match existing.or(a.recreated_from.as_deref()) {
        Some(record) => record.clone(),
        None => crate::history::Record::new(&a.line, &a.record.name, &a.record.agent_name, now),
    };
    record.ran_as(&a.record.id);
    record.configuration = a.record.name.clone();
    record.agent_name = a.record.agent_name.clone();
    record.cwd = a.record.cwd.clone();
    record.scope = a.record.scope.clone();
    record.dev_shell = a.record.dev_shell.clone();
    // Recorded Docker stays, so an attach skipped now is tried again.
    record.docker_enabled |= a.record.docker_enabled;
    record.variables = a.variables.clone();
    record.skipped = a.record.skipped.clone();
    record.stopped = None;
    record.updated = now;
    for package in &a.record.packages {
        // A full record keeps what fits, as a live grant would.
        let _ = record.approve(package);
    }
    record
}
fn unsent<T>(error: std::sync::mpsc::TrySendError<T>) -> T {
    match error {
        std::sync::mpsc::TrySendError::Full(work)
        | std::sync::mpsc::TrySendError::Disconnected(work) => work,
    }
}
fn bounded(text: &str) -> String {
    text.chars().take(512).collect()
}
impl Drop for Controller {
    fn drop(&mut self) {
        // Hand queued raid events over before messaging::Service closes its
        // channel. Drain results meanwhile so a full queue cannot deadlock.
        // Send the current directory, not the last synced one: a member that
        // joined this tick needs its session.started record first.
        // Operations still pending end with the daemon; record that outcome.
        let pending: Vec<_> = self
            .sessions
            .values_mut()
            .filter_map(|a| a.pending.take())
            .collect();
        for p in pending {
            p.cancel.cancel();
            if let Some(r) = self.permission_mut(&p.id) {
                r.state = "cancelled".into();
            }
            self.operation_finished(&p.id, p.notify, Some("daemon stopped".into()));
        }
        if !self.raid_outbox.is_empty() || !self.operation_outbox.is_empty() {
            let mut directory = self.message_directory();
            // Every session is going away: queue no notice that nobody can read.
            if !self.operation_outbox.is_empty() {
                for session in directory.values_mut() {
                    session.running = false;
                }
            }
            let mut work = crate::messaging::Work::Sync {
                directory,
                raids: self.raid_outbox.drain(..).collect(),
                operations: self.operation_outbox.drain(..).collect(),
            };
            while let Err(std::sync::mpsc::TrySendError::Full(back)) =
                self.messaging.commands.try_send(work)
            {
                work = back;
                for _ in self.messaging.results.try_iter() {}
                std::thread::sleep(std::time::Duration::from_millis(1));
            }
        }
        for a in self.sessions.values() {
            if let Some(w) = &a.worker {
                w.cancel.cancel();
            }
        }
        for (_, mut a) in std::mem::take(&mut self.sessions) {
            if let Some(p) = a.pending.take() {
                p.cancel.cancel();
            }
            a.worker.take();
            drop(a.terminal);
            let _ = fs::remove_dir_all(a.directory);
        }
        let _ = fs::remove_file(self.state.join("host.sock"));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn daemon() -> Controller {
        let path = unix::temp_directory().unwrap();
        Controller::new(&path, None).unwrap()
    }
    fn connection(role: Role) -> (Connection, UnixStream) {
        let (a, b) = UnixStream::pair().unwrap();
        (Connection::new(1, role, a).unwrap(), b)
    }
    fn dispatch(
        d: &mut Controller,
        c: &mut Connection,
        method: &str,
        params: Value,
        id: Option<Value>,
    ) -> Value {
        d.dispatch(
            c,
            rpc::Call {
                jsonrpc: "2.0".into(),
                id,
                method: method.into(),
                params,
            },
        );
        let data = c.output.pop_back().unwrap();
        let body = data.windows(4).position(|w| w == b"\r\n\r\n").unwrap() + 4;
        rpc::parse(&data[body..]).unwrap()
    }
    fn session_with_results(d: &mut Controller, results: Vec<Completed>) -> String {
        session_with_results_id(d, "test-session", results)
    }
    fn session_with_results_id(d: &mut Controller, id: &str, results: Vec<Completed>) -> String {
        let id = id.to_string();
        let directory = d.state.join(&id);
        unix::private_directory(&directory).unwrap();
        let terminal = Terminal::new(&directory.join("terminal.sock")).unwrap();
        d.sessions.insert(
            id.clone(),
            Active {
                activity_at: None,
                inheritance: None,
                exit_watch: None,
                init: None,
                granted_outputs: BTreeMap::new(),
                dev_identity: None,
                call: None,
                recreated_from: None,
                variables: BTreeMap::new(),
                line: id.clone(),
                created: 0,
                record: SessionRecord {
                    agent_state: Default::default(),
                    agent_state_since: None,
                    inbox: 0,
                    docker_enabled: false,
                    scope: None,
                    dev_shell: None,
                    cwd: None,
                    id: id.clone(),
                    agent_name: "snikk".into(),
                    parent: None,
                    path: "snikk".into(),
                    name: "shell".into(),
                    description: "Test shell running in a Goblins sandbox".into(),
                    configuration: "test".into(),
                    state: "running".into(),
                    identity: None,
                    initial_packages: vec![],
                    packages: vec![],
                    terminal: String::new(),
                    pty_eof: false,
                    terminal_complete: false,
                    terminal_interrupted: false,
                    terminal_attached: false,
                    terminal_detached: false,
                    exit_code: None,
                    stop_reason: None,
                    detail: None,
                    raid: None,
                    shells: 0,
                    execs: 0,
                    recreated_from: None,
                    skipped: vec![],
                },
                worker: Some(Worker::completed(results)),
                listener: None,
                pending: Some(Pending {
                    initial_output: None,
                    dev_identity: None,
                    id: "r".into(),
                    serial: 1,
                    connection: 99,
                    cancel: Cancel::default(),
                    notify: false,
                }),
                terminal,
                directory,
            },
        );
        d.permissions.push_back(PermissionRecord {
            kind: "package".into(),
            id: "r".into(),
            session: id.clone(),
            agent_name: "snikk".into(),
            approval: "a".into(),
            package: "hello".into(),
            reason: "test".into(),
            state: "realizing".into(),
            approved: Some(true),
            preview: None,
            message: None,
        });
        id
    }
    fn launch(key: &str, agent_name: Option<&str>) -> Value {
        json!({"key":key,"name":"shell","configuration":"/missing-goblins-test-manifest",
               "agent_name":agent_name,"rows":24,"cols":100})
    }
    #[test]
    fn exec_descriptors_wait_for_their_whole_request() {
        let mut d = daemon();
        let path = d.state.clone();
        let id = session_with_results_id(&mut d, "target", vec![]);
        let (init_daemon, init_sandbox) = goblins_protocol::init::pair().unwrap();
        d.sessions.get_mut(&id).unwrap().init = Some(init_daemon);
        let (mut c, mut peer) = connection(Role::Host);
        peer.write_all(
            &rpc::encode(&rpc::request(json!(1), "initialize", json!({"api":1}))).unwrap(),
        )
        .unwrap();
        d.service(&mut c);
        assert!(c.initialized);
        // A request near the frame limit, split across reads, with the
        // descriptors attached to its first part only.
        let argv = vec!["x".repeat(15000)];
        let frame = rpc::encode(&rpc::request(
            json!(2),
            "sessions.exec",
            json!({"session":"target","argv":argv}),
        ))
        .unwrap();
        let (a, b) = unix::pipe().unwrap();
        let (cut, rest) = frame.split_at(100);
        let fds = [a.as_raw_fd(), b.as_raw_fd(), b.as_raw_fd()];
        goblins_protocol::init::send(peer.as_raw_fd(), cut, &fds).unwrap();
        d.service(&mut c);
        assert_eq!(c.fds.len(), 3);
        assert!(c.exec.is_none());
        peer.write_all(rest).unwrap();
        for _ in 0..4 {
            d.service(&mut c);
        }
        assert!(c.exec.is_some(), "{:?}", c.output);
        let mut buffer = vec![0; goblins_protocol::init::MAX_MESSAGE];
        let (n, received) =
            goblins_protocol::init::receive(init_sandbox.as_raw_fd(), &mut buffer, 0).unwrap();
        assert_eq!(received.len(), 3);
        assert!(String::from_utf8_lossy(&buffer[..n]).contains(&argv[0]));
        drop(d);
        fs::remove_dir_all(path).unwrap();
    }
    #[test]
    fn child_configurations_follow_the_selected_parents_list() {
        let mut d = daemon();
        let goblin = |children: Value| {
            json!({"build_spec": "/nonexistent/spec.json", "args": [], "env": {},
                "client_package": "/unused", "helper": "/unused", "flake": "unused",
                "allowed_children": children})
        };
        let manifest: crate::config::Manifest = serde_json::from_value(json!({
            "api": 2, "helper_api": 5,
            "goblins": {
                "coordinator": goblin(json!(["coordinator", "reviewer", "scoped"])),
                "reviewer": goblin(json!(["shell"])),
                "shell": goblin(json!([])),
                "scoped": {"scope": "work", "scopes": {"work": goblin(Value::Null)},
                    "build_spec": "/unused", "args": [], "env": {}, "client_package": "/unused",
                    "helper": "/unused", "flake": "unused"},
            },
            "scopes": {"work": {"persistent": false, "pid": true}},
        }))
        .unwrap();
        let branch = std::sync::Arc::new(manifest.branch("coordinator", None).unwrap().1);
        let root = session_with_results_id(&mut d, "root", vec![]);
        let child = session_with_results_id(&mut d, "child", vec![]);
        let leaf = session_with_results_id(&mut d, "leaf", vec![]);
        for (id, parent, name) in [
            (&root, None, "coordinator"),
            (&child, Some(&root), "reviewer"),
            (&leaf, Some(&child), "shell"),
        ] {
            let a = d.sessions.get_mut(id).unwrap();
            a.record.parent = parent.cloned();
            a.record.name = name.into();
            a.record.agent_name = id.clone();
            a.record.configuration = "/loaded.json".into();
            a.inheritance = Some(Inheritance::fixture(name, branch.clone()));
        }
        let keys = std::cell::Cell::new(0);
        let start = |name: &str, parent: &str| -> Start {
            keys.set(keys.get() + 1);
            serde_json::from_value(json!({"key": format!("k{}", keys.get()),
                "configuration": "/newer.json", "name": name, "agent_name": null,
                "parent": parent, "rows": 24, "cols": 80}))
            .unwrap()
        };
        let refused = |d: &mut Controller, p: Start, scope: Option<&str>| {
            let error = d.start(p, scope).unwrap_err();
            assert_eq!(error.0, -32602);
            error.1
        };
        // The host selecting a descendant uses that descendant's list.
        assert_eq!(
            refused(&mut d, start("coordinator", "child"), None),
            "configuration 'coordinator' is not an allowed child of 'reviewer'; allowed: shell"
        );
        assert_eq!(
            refused(&mut d, start("reviewer", "leaf"), None),
            "configuration 'shell' permits no children"
        );
        // So does a sandbox selecting its descendant with --parent.
        assert_eq!(
            refused(&mut d, start("reviewer", "child"), Some(&root)),
            "configuration 'reviewer' is not an allowed child of 'reviewer'; allowed: shell"
        );
        assert_eq!(
            refused(&mut d, start("scoped", "."), Some(&root)),
            "configuration 'scoped' cannot be launched here: 'scoped' requires scope 'work'; its parent has no scope"
        );
        // Nothing was recorded or allocated for a refused launch.
        assert!(d.launches.is_empty());
        assert_eq!(d.sessions.len(), 3);
        let accepted = d.start(start("shell", "child"), None).unwrap();
        let record = &d.sessions[accepted["session"].as_str().unwrap()].record;
        assert_eq!(record.name, "shell");
        assert_eq!(record.parent.as_deref(), Some(child.as_str()));
        // The record keeps the parent's manifest, the snapshot's origin.
        assert_eq!(record.configuration, "/loaded.json");
    }
    #[test]
    fn parent_name_stays_reserved_until_descendant_cleanup_finishes() {
        let mut d = daemon();
        let path = d.state.clone();
        let root = session_with_results_id(&mut d, "root", vec![]);
        let child = session_with_results_id(&mut d, "child", vec![]);
        let a = d.sessions.get_mut(&child).unwrap();
        a.record.parent = Some(root.clone());
        a.record.agent_name = "kid".into();
        a.record.path = "snikk/kid".into();
        a.record.state = "stopping".into();
        d.sessions.get_mut(&root).unwrap().worker.take();
        assert_eq!(d.allocate_name(Some("snikk"), None).unwrap_err().0, -32009);
        // The same name in a different sibling group remains independent.
        assert_eq!(
            d.allocate_name(Some("snikk"), Some(&root)).unwrap(),
            "snikk"
        );
        d.sessions.get_mut(&child).unwrap().worker.take();
        assert_eq!(d.allocate_name(Some("snikk"), None).unwrap(), "snikk");
        drop(d);
        fs::remove_dir_all(path).unwrap();
    }
    #[test]
    fn agent_name_grammar_and_pool_cover_session_limit() {
        let names: std::collections::BTreeSet<_> = GOBLIN_NAMES.lines().collect();
        assert_eq!(names.len(), GOBLIN_NAMES.lines().count());
        assert!(names.len() >= SESSIONS);
        assert!(names.iter().all(|n| valid_agent_name(n)));
        for name in ["a", "snikk-2", &"x".repeat(32)] {
            assert!(valid_agent_name(name));
        }
        for name in [
            "",
            "Snikk",
            "1snikk",
            "-snikk",
            "two words",
            "../snikk",
            "gøb",
            "x\n",
            &"x".repeat(33),
        ] {
            assert!(!valid_agent_name(name), "{name:?}");
        }
    }
    #[test]
    fn random_selection_covers_candidates_and_bounds_exhaustion_and_entropy_failures() {
        let candidates = ["snikk", "grib", "zoggit"];
        for (index, name) in candidates.iter().enumerate() {
            let bytes = (index as u64).to_le_bytes();
            assert_eq!(
                random_name(&candidates, &mut bytes.as_slice()).unwrap(),
                *name
            );
        }
        // Reject the top incomplete bucket, then use a fresh sample.
        let bytes = [u64::MAX.to_le_bytes(), 2_u64.to_le_bytes()].concat();
        assert_eq!(
            random_name(&candidates, &mut bytes.as_slice()).unwrap(),
            "zoggit"
        );
        assert_eq!(
            random_name(&[], &mut io::empty()).unwrap_err(),
            (-32010, "automatic agent name pool exhausted".into())
        );
        assert!(
            random_name(&candidates, &mut io::empty())
                .unwrap_err()
                .1
                .contains("cannot read randomness")
        );
        let rejected = [255; 64];
        assert!(
            random_name(&candidates, &mut rejected.as_slice())
                .unwrap_err()
                .1
                .contains("after 8 attempts")
        );
        assert_eq!(
            random_name(&["grib"], &mut 0_u64.to_le_bytes().as_slice()).unwrap(),
            "grib"
        );
    }
    #[test]
    fn allocation_is_bounded_and_retries_precede_capacity_and_collisions() {
        let mut d = daemon();
        let path = d.state.clone();
        let (mut c, _peer) = connection(Role::Host);
        let mut results = Vec::new();
        let mut allocated = std::collections::BTreeSet::new();
        for i in 0..SESSIONS {
            let result = d
                .host(&mut c, "sessions.start", launch(&format!("key{i}"), None))
                .unwrap();
            let name = result["agent_name"].as_str().unwrap();
            assert!(GOBLIN_NAMES.lines().any(|candidate| candidate == name));
            assert!(allocated.insert(name.to_owned()));
            let record = &d.sessions[result["session"].as_str().unwrap()].record;
            assert_eq!(record.agent_name, name);
            assert_eq!(record.name, "shell");
            assert_eq!(record.configuration, "/missing-goblins-test-manifest");
            results.push(result);
        }
        assert_eq!(
            d.host(&mut c, "sessions.start", launch("overflow", None))
                .unwrap_err(),
            capacity()
        );
        assert_eq!(
            d.host(
                &mut c,
                "sessions.start",
                launch("collision", results[0]["agent_name"].as_str())
            )
            .unwrap_err()
            .0,
            -32009
        );
        assert_eq!(
            d.host(&mut c, "sessions.start", launch("capacity", Some("custom")))
                .unwrap_err()
                .0,
            -32010
        );
        assert_eq!(
            d.host(&mut c, "sessions.start", launch("key0", None))
                .unwrap(),
            results[0]
        );
        assert_eq!(
            d.host(&mut c, "sessions.start", launch("key0", Some("custom")))
                .unwrap_err()
                .0,
            -32009
        );
        assert_eq!(d.launches.len(), SESSIONS);
        drop(d);
        fs::remove_dir_all(path).unwrap();
    }
    #[test]
    fn explicit_names_reserve_through_cleanup_and_history_keeps_identity() {
        let mut d = daemon();
        let path = d.state.clone();
        let (mut c, _peer) = connection(Role::Host);
        for name in ["", "Snikk", "../snikk", &"a".repeat(33)] {
            assert_eq!(
                d.host(&mut c, "sessions.start", launch("bad", Some(name)))
                    .unwrap_err()
                    .0,
                -32602
            );
        }
        assert!(d.sessions.is_empty());
        let first = d
            .host(&mut c, "sessions.start", launch("first", Some("shell")))
            .unwrap();
        let id = first["session"].as_str().unwrap();
        assert_eq!(d.resolve_session("shell").unwrap(), id);
        assert_eq!(d.resolve_session(id).unwrap(), id);
        d.host(&mut c, "sessions.stop", json!({"session":"shell"}))
            .unwrap();
        for state in ["starting", "running", "stopping", "stopped", "failed"] {
            d.sessions.get_mut(id).unwrap().record.state = state.into();
            assert_eq!(d.allocate_name(Some("shell"), None).unwrap_err().0, -32009);
        }
        d.sessions.get_mut(id).unwrap().worker.take();
        assert!(d.resolve_session("shell").is_err());
        let second = d
            .host(&mut c, "sessions.start", launch("second", Some("shell")))
            .unwrap();
        assert_ne!(first["session"], second["session"]);
        assert_eq!(d.resolve_session("shell").unwrap(), second["session"]);
        assert_eq!(
            d.host(&mut c, "sessions.get", json!({"session":id}))
                .unwrap()["agent_name"],
            "shell"
        );
        // Even eviction and name reuse cannot redirect a retry to the successor.
        d.sessions.remove(id);
        assert_eq!(
            d.host(&mut c, "sessions.start", launch("first", Some("shell")))
                .unwrap(),
            first
        );
        drop(d);
        fs::remove_dir_all(path).unwrap();
    }
    #[test]
    fn failed_grant_retains_diagnostic_on_permission_record() {
        let mut d = daemon();
        let path = d.state.clone();
        let detail = "could not build package 'hello': actual Nix diagnostic";
        let id = session_with_results(
            &mut d,
            vec![Completed::Granted {
                reply: goblins_protocol::Reply::new(
                    Some("r".into()),
                    "error",
                    Some("grant failed; see tui terminal".into()),
                ),
                detail: Some(detail.into()),
                output: None,
            }],
        );
        d.tick().unwrap();
        assert_eq!(d.permissions[0].state, "failed");
        assert_eq!(d.permissions[0].message.as_deref(), Some(detail));
        // A later session diagnostic must not erase this request's explanation.
        d.sessions.get_mut(&id).unwrap().record.detail = None;
        assert_eq!(d.snapshot().permissions[0].message.as_deref(), Some(detail));
        drop(d);
        fs::remove_dir_all(path).unwrap();
    }
    #[test]
    fn activity_is_endpoint_owned_fresh_and_separate_from_lifecycle() {
        use goblins_protocol::AgentState;
        let mut d = daemon();
        let path = d.state.clone();
        let id = session_with_results(&mut d, vec![]);
        d.dirty = false;
        let (mut host, _) = connection(Role::Host);
        assert!(
            d.host(&mut host, "sessions.set-state", json!({"state":"working"}))
                .is_err()
        );
        for value in [
            json!({"state":"busy"}),
            json!({"state":"idle","session":id}),
            json!({"state":"idle","target":"other"}),
        ] {
            assert!(d.sandbox_control(&id, "sessions.set-state", value).is_err());
        }
        d.sandbox_control(&id, "sessions.set-state", json!({"state":"working"}))
            .unwrap();
        assert!(d.dirty);
        assert_eq!(d.sessions[&id].record.state, "running");
        assert_eq!(d.sessions[&id].record.agent_state, AgentState::Working);
        let at = d.sessions[&id].activity_at.unwrap();
        d.dirty = false;
        d.sandbox_control(&id, "sessions.set-state", json!({"state":"working"}))
            .unwrap();
        assert!(!d.dirty); // heartbeats refresh freshness without publishing
        assert!(d.sessions[&id].activity_at.unwrap() >= at);
        d.expire_activity(at + std::time::Duration::from_secs(31));
        assert!(d.dirty);
        assert_eq!(d.sessions[&id].record.agent_state, AgentState::Unknown);
        d.sandbox_control(&id, "sessions.set-state", json!({"state":"waiting-input"}))
            .unwrap();
        d.stop(&id, None).unwrap();
        assert_eq!(d.sessions[&id].record.agent_state, AgentState::Unknown);
        assert!(
            d.sandbox_control(&id, "sessions.set-state", json!({"state":"idle"}))
                .is_err()
        );
        drop(d);
        fs::remove_dir_all(path).unwrap();
    }
    #[test]
    fn claude_integration_results_report_and_refresh_activity() {
        use goblins_protocol::AgentState;
        let mut d = daemon();
        let path = d.state.clone();
        let manifest: crate::config::Manifest = serde_json::from_value(json!({
            "api": 2, "helper_api": 5,
            "goblins": {"shell": {"build_spec": "/unused", "args": [], "env": {},
                "client_package": "/unused", "helper": "/unused", "flake": "unused"}},
            "scopes": {},
        }))
        .unwrap();
        let branch = std::sync::Arc::new(manifest.branch("shell", None).unwrap().1);
        let id = session_with_results(&mut d, vec![]);
        let response = |result: Value| crate::messaging::Completed {
            connection: 0,
            id: json!(1),
            result: Ok(result),
            actor: id.clone(),
        };
        let a = d.sessions.get_mut(&id).unwrap();
        a.inheritance = Some(Inheritance::fixture("shell", branch.clone()));
        d.dirty = false;
        // Only a Claude-integrated session takes activity from its integration.
        d.claude_activity(&response(json!({"agent_state":"working"})));
        assert!(!d.dirty);
        assert!(d.sessions[&id].activity_at.is_none());
        let a = d.sessions.get_mut(&id).unwrap();
        a.inheritance = Some(Inheritance::fixture_with_integration(
            "shell", branch, "claude",
        ));
        d.claude_activity(&response(json!({"notify":false})));
        assert!(d.sessions[&id].activity_at.is_none());
        d.claude_activity(&response(json!({"agent_state":"working"})));
        assert!(d.dirty);
        assert_eq!(d.sessions[&id].record.agent_state, AgentState::Working);
        let at = d.sessions[&id].activity_at.unwrap();
        d.dirty = false;
        // Monitor ticks repeat the activity: fresher, but not republished.
        d.claude_activity(&response(json!({"agent_state":"working"})));
        assert!(!d.dirty);
        assert!(d.sessions[&id].activity_at.unwrap() >= at);
        // Once the monitor stops ticking, activity expires as for any endpoint.
        d.expire_activity(at + std::time::Duration::from_secs(31));
        assert_eq!(d.sessions[&id].record.agent_state, AgentState::Unknown);
        drop(d);
        fs::remove_dir_all(path).unwrap();
    }
    #[test]
    fn activity_changes_are_timestamped() {
        use goblins_protocol::AgentState;
        let mut d = daemon();
        let path = d.state.clone();
        let id = session_with_results(&mut d, vec![]);
        let a = d.sessions.get_mut(&id).unwrap();
        assert!(a.report_activity(AgentState::Working));
        let since = a.record.agent_state_since.unwrap();
        // A repeated report is a heartbeat, not a new state.
        a.record.agent_state_since = Some(since - 5000);
        assert!(!a.report_activity(AgentState::Working));
        assert_eq!(a.record.agent_state_since, Some(since - 5000));
        assert!(a.report_activity(AgentState::Idle));
        assert!(a.record.agent_state_since.unwrap() >= since);
        d.stop(&id, None).unwrap();
        assert_eq!(d.sessions[&id].record.agent_state, AgentState::Unknown);
        assert!(d.sessions[&id].record.agent_state_since.unwrap() >= since);
        drop(d);
        fs::remove_dir_all(path).unwrap();
    }
    #[test]
    fn inbox_counts_follow_the_worker_report() {
        let mut d = daemon();
        let path = d.state.clone();
        let id = session_with_results(&mut d, vec![]);
        d.dirty = false;
        d.inbox_counts(&[(id.clone(), 2), ("gone".into(), 1)].into());
        assert!(d.dirty);
        assert_eq!(d.sessions[&id].record.inbox, 2);
        d.dirty = false;
        d.inbox_counts(&[(id.clone(), 2)].into());
        assert!(!d.dirty);
        d.inbox_counts(&BTreeMap::new());
        assert!(d.dirty);
        assert_eq!(d.sessions[&id].record.inbox, 0);
        drop(d);
        fs::remove_dir_all(path).unwrap();
    }
    #[test]
    fn old_session_snapshots_default_activity_to_unknown() {
        let mut d = daemon();
        let path = d.state.clone();
        let id = session_with_results(&mut d, vec![]);
        let mut value = serde_json::to_value(&d.sessions[&id].record).unwrap();
        for field in ["agent_state", "agent_state_since", "inbox"] {
            value.as_object_mut().unwrap().remove(field);
        }
        let record: SessionRecord = serde_json::from_value(value).unwrap();
        assert_eq!(record.agent_state, goblins_protocol::AgentState::Unknown);
        assert_eq!(record.agent_state_since, None);
        assert_eq!(record.inbox, 0);
        drop(d);
        fs::remove_dir_all(path).unwrap();
    }
    #[test]
    fn queued_progress_cannot_restore_cancelled_permission() {
        let mut d = daemon();
        let path = d.state.clone();
        let id = session_with_results(
            &mut d,
            vec![
                Completed::Progress {
                    request: "r".into(),
                    state: "mounting",
                },
                Completed::Progress {
                    request: "r".into(),
                    state: "publishing",
                },
                Completed::Granted {
                    reply: goblins_protocol::Reply::new(Some("r".into()), "ready", None),
                    detail: None,
                    output: None,
                },
                Completed::Stopped { exit_code: None },
            ],
        );
        d.stop(&id, Some("stopped by host".into())).unwrap();
        d.stop(&id, Some("stopped by another caller".into()))
            .unwrap();
        d.tick().unwrap();
        assert_eq!(
            d.sessions[&id].record.stop_reason.as_deref(),
            Some("stopped by host")
        );
        assert_eq!(d.permissions[0].state, "cancelled");
        assert_eq!(d.sessions[&id].record.state, "stopped");
        assert!(d.sessions[&id].worker.is_none());
        assert!(d.sessions[&id].record.packages.is_empty());
        drop(d);
        fs::remove_dir_all(path).unwrap();
    }
    #[test]
    fn full_result_queue_without_stopped_cannot_leave_session_running() {
        let mut d = daemon();
        let path = d.state.clone();
        // A full bounded queue can reject the worker's final try_send(Stopped).
        let id = session_with_results(
            &mut d,
            (0..16)
                .map(|_| Completed::Progress {
                    request: "r".into(),
                    state: "mounting",
                })
                .collect(),
        );
        d.tick().unwrap();
        assert!(d.sessions[&id].worker.is_some());
        d.tick().unwrap();
        assert!(d.sessions[&id].worker.is_none());
        assert_eq!(d.sessions[&id].record.state, "failed");
        assert!(
            d.sessions[&id]
                .record
                .detail
                .as_ref()
                .unwrap()
                .contains("without a final")
        );
        assert!(d.sessions[&id].pending.is_none());
        assert_eq!(d.permissions[0].state, "cancelled");
        drop(d);
        fs::remove_dir_all(path).unwrap();
    }
    #[test]
    fn endpoint_authority_initialization_and_notifications() {
        let mut d = daemon();
        let path = d.state.clone();
        let session = session_with_results(&mut d, vec![]);
        let (mut c, _peer) = connection(Role::Sandbox(session));
        assert_eq!(
            dispatch(&mut d, &mut c, "sessions.list", json!({}), Some(json!(1)))["error"]["code"],
            -32001
        );
        assert_eq!(
            dispatch(
                &mut d,
                &mut c,
                "initialize",
                json!({"api":2}),
                Some(json!(2))
            )["error"]["code"],
            -32002
        );
        assert_eq!(
            dispatch(
                &mut d,
                &mut c,
                "initialize",
                json!({"api":1,"role":"host"}),
                Some(json!(3))
            )["error"]["code"],
            -32602
        );
        assert_eq!(
            dispatch(
                &mut d,
                &mut c,
                "initialize",
                json!({"api":1}),
                Some(json!(4))
            )["result"]["role"],
            "sandbox"
        );
        let (mut host, _host_peer) = connection(Role::Host);
        let _ = dispatch(
            &mut d,
            &mut host,
            "initialize",
            json!({"api":1}),
            Some(json!(1)),
        );
        assert!(
            !host
                .decoder
                .expired(Instant::now() + std::time::Duration::from_secs(4))
        );
        for method in [
            "server.status",
            "server.stop",
            "permissions.decide",
            "state.subscribe",
        ] {
            assert_eq!(
                dispatch(&mut d, &mut c, method, json!({}), Some(json!(5)))["error"]["code"],
                -32601
            );
        }
        let before = d.snapshot().sessions.len();
        d.dispatch(
            &mut c,
            rpc::Call {
                jsonrpc: "2.0".into(),
                id: None,
                method: "sessions.start".into(),
                params: json!({}),
            },
        );
        assert!(c.output.is_empty());
        assert_eq!(d.snapshot().sessions.len(), before);
        drop(d);
        fs::remove_dir_all(path).unwrap();
    }
    #[test]
    fn server_shutdown_flushes_acceptance_and_rejects_later_work() {
        let mut d = daemon();
        let path = d.state.clone();
        let (mut c, _peer) = connection(Role::Host);
        c.initialized = true;
        assert_eq!(
            d.host(&mut c, "server.status", json!({})).unwrap()["state"],
            "running"
        );
        d.dispatch(
            &mut c,
            rpc::Call {
                jsonrpc: "2.0".into(),
                id: Some(json!(2)),
                method: "server.stop".into(),
                params: json!({}),
            },
        );
        assert!(c.queued > 0);
        d.connections.push(c);
        assert!(!d.shutdown_ready());
        let (mut other, _other_peer) = connection(Role::Host);
        assert_eq!(
            d.host(&mut other, "sessions.start", launch("late", None))
                .unwrap_err()
                .0,
            -32009
        );
        assert!(d.sessions.is_empty());
        assert_eq!(
            d.host(&mut other, "server.status", json!({})).unwrap()["state"],
            "stopping"
        );
        d.connections[0].flush();
        assert!(d.shutdown_ready());
        drop(d);
        fs::remove_dir_all(path).unwrap();
    }
    #[test]
    fn trailing_input_is_consumed_and_queue_overflow_disconnects() {
        let (mut c, mut peer) = connection(Role::Sandbox("s".into()));
        c.waiting = Some(("r".into(), json!(1)));
        peer.write_all(b"x").unwrap();
        drop(peer);
        assert!(c.read().is_none());
        assert!(c.dead);
        let (mut c, _peer) = connection(Role::Host);
        for _ in 0..4 {
            c.queue(json!({"data":"x".repeat(700_000)}));
        }
        assert!(c.dead);
        assert!(c.queued <= QUEUE);
    }
    #[test]
    fn snapshot_and_event_share_one_order() {
        let mut d = daemon();
        let path = d.state.clone();
        let (mut c, _peer) = connection(Role::Host);
        c.initialized = true;
        d.dirty = true;
        let initial = dispatch(&mut d, &mut c, "state.subscribe", json!({}), Some(json!(1)));
        assert_eq!(initial["result"]["sequence"], 1);
        d.connections.push(c);
        d.dirty = true;
        d.publish();
        let data = d.connections[0].output.pop_back().unwrap();
        let body = data.windows(4).position(|w| w == b"\r\n\r\n").unwrap() + 4;
        let event = rpc::parse(&data[body..]).unwrap();
        assert_eq!(event["params"]["sequence"], 2);
        assert_eq!(
            event["params"]["subscription"],
            initial["result"]["subscription"]
        );
        drop(d);
        fs::remove_dir_all(path).unwrap();
    }

    /// A running session whose worker results the test supplies.
    fn operations(
        queue: &std::sync::mpsc::Receiver<crate::messaging::Work>,
    ) -> Vec<crate::operation::Event> {
        queue
            .try_iter()
            .flat_map(|work| match work {
                crate::messaging::Work::Sync { operations, .. }
                | crate::messaging::Work::Call { operations, .. } => operations,
                crate::messaging::Work::Delivery { .. } => vec![],
            })
            .collect()
    }
    fn outcome(event: &crate::operation::Event) -> (&str, Option<&str>, bool) {
        match &event.change {
            crate::operation::Change::Completed {
                status,
                message,
                notify,
                ..
            } => (status, message.as_deref(), *notify),
            other => panic!("not a completion: {other:?}"),
        }
    }
    #[test]
    fn every_terminal_outcome_is_queued_once_with_the_sandbox_message() {
        let path = unix::temp_directory().unwrap();
        for (results, status, message) in [
            (
                vec![Completed::Granted {
                    reply: goblins_protocol::Reply::new(Some("r".into()), "ready", None),
                    detail: None,
                    output: None,
                }],
                "ready",
                None,
            ),
            (
                vec![Completed::Granted {
                    reply: goblins_protocol::Reply::new(
                        Some("r".into()),
                        "error",
                        Some("grant failed; see tui terminal".into()),
                    ),
                    detail: Some("host-only Nix diagnostic".into()),
                    output: None,
                }],
                "failed",
                Some("grant failed; see tui terminal"),
            ),
            (
                vec![Completed::Stopped { exit_code: Some(0) }],
                "cancelled",
                Some("payload stopped"),
            ),
        ] {
            let mut d = Controller::new(&path, None).unwrap();
            let (service, queue) = crate::messaging::Service::detached(8);
            d.messaging = service;
            let id = session_with_results(&mut d, results);
            d.sessions
                .get_mut(&id)
                .unwrap()
                .pending
                .as_mut()
                .unwrap()
                .notify = true;
            d.tick().unwrap();
            d.tick().unwrap();
            let events = operations(&queue);
            assert_eq!(events.len(), 1, "{status}");
            assert_eq!(events[0].request, "r");
            assert_eq!(events[0].session, id);
            assert_eq!(events[0].kind(), "operation.completed");
            assert_eq!(outcome(&events[0]), (status, message, true));
            drop(d);
            // Nothing was pending at shutdown, so nothing more is reported.
            assert!(operations(&queue).is_empty());
        }
        fs::remove_dir_all(path).unwrap();
    }
    #[test]
    fn stop_withdrawal_and_shutdown_end_pending_operations() {
        let mut d = daemon();
        let path = d.state.clone();
        let (service, queue) = crate::messaging::Service::detached(64);
        d.messaging = service;
        let stopped = session_with_results_id(&mut d, "stopped", vec![]);
        d.stop(&stopped, Some("stopped by host".into())).unwrap();
        let withdrawn = session_with_results_id(&mut d, "withdrawn", vec![]);
        d.permissions.back_mut().unwrap().state = "pending".into();
        d.permissions.back_mut().unwrap().id = "w".into();
        d.sessions
            .get_mut(&withdrawn)
            .unwrap()
            .pending
            .as_mut()
            .unwrap()
            .id = "w".into();
        d.withdraw(99);
        // A second loss of the same connection reports nothing new.
        d.withdraw(99);
        let shutdown = session_with_results_id(&mut d, "shutdown", vec![]);
        let (worker, _results) = Worker::idle();
        d.sessions.get_mut(&shutdown).unwrap().worker = Some(worker);
        d.permissions.back_mut().unwrap().id = "s".into();
        d.sessions
            .get_mut(&shutdown)
            .unwrap()
            .pending
            .as_mut()
            .unwrap()
            .id = "s".into();
        d.tick().unwrap();
        let events = operations(&queue);
        let outcomes: Vec<_> = events
            .iter()
            .map(|e| (e.session.as_str(), outcome(e).0, outcome(e).1))
            .collect();
        assert_eq!(
            outcomes,
            [
                ("stopped", "cancelled", Some("session stopped")),
                ("withdrawn", "withdrawn", None),
            ]
        );
        drop(d);
        let events = operations(&queue);
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].request, "s");
        assert_eq!(outcome(&events[0]).1, Some("daemon stopped"));
        fs::remove_dir_all(path).unwrap();
    }
    #[test]
    fn records_follow_grants_denials_variables_and_stop() {
        let mut d = daemon();
        let path = d.state.clone();
        d.open_history(Ok(path.join("history")), 100);
        let _results = live(&mut d, "a", None);
        let _child = live(&mut d, "kid", Some("a"));
        let record = |d: &Controller| d.history.as_ref().unwrap().records["a"].clone();
        d.history.as_mut().unwrap().write({
            let mut record = crate::history::Record::new("a", "shell", "a", 1);
            record.ran_as("a");
            record
        });
        // Variables: only the caller's own record, all keys together.
        let set = |d: &mut Controller, id: &str, variables: Value| {
            d.sandbox_control(id, "sessions.set-variables", json!({"variables":variables}))
        };
        assert_eq!(
            set(
                &mut d,
                "a",
                json!({"session_id":"c819","session_name":"Plan it"})
            )
            .unwrap(),
            json!({"accepted":true,"recorded":true})
        );
        assert_eq!(record(&d).variables["session_name"], "Plan it");
        assert!(record(&d).updated > 1);
        assert_eq!(set(&mut d, "a", json!({"Bad":"x"})).unwrap_err().0, -32602);
        assert_eq!(
            set(&mut d, "a", json!({"session_name":""})).unwrap()["recorded"],
            true
        );
        assert!(!record(&d).variables.contains_key("session_name"));
        assert_eq!(
            set(&mut d, "kid", json!({"session_id":"other"})).unwrap(),
            json!({"accepted":true,"recorded":false})
        );
        assert!(!d.history.as_ref().unwrap().records.contains_key("kid"));
        // A host denial, then an approval of the same package.
        let (worker, sender, _commands) = Worker::listening();
        d.sessions.get_mut("a").unwrap().worker = Some(worker);
        let request = |d: &mut Controller, kind: &str| {
            let (mut c, peer) = connection(Role::Sandbox("a".into()));
            c.initialized = true;
            d.dispatch(
                &mut c,
                rpc::Call {
                    jsonrpc: "2.0".into(),
                    id: Some(json!(1)),
                    method: "permissions.request".into(),
                    params: json!({"kind":kind,"package":if kind == "package" {"hello"} else {""},"reason":"test"}),
                },
            );
            d.connections.push(c);
            (d.permissions.back().unwrap().clone(), peer)
        };
        let (denied, _peer) = request(&mut d, "package");
        d.decide(
            Decision {
                session: "a".into(),
                request: denied.id.clone(),
                approval: denied.approval.clone(),
                approved: false,
            },
            "host",
        )
        .unwrap();
        assert_eq!(record(&d).packages.denied, ["hello"]);
        let _peer = request(&mut d, "package");
        sender
            .send(Completed::Granted {
                reply: goblins_protocol::Reply::new(None, "ready", None),
                detail: None,
                output: None,
            })
            .unwrap();
        d.tick().unwrap();
        assert_eq!(record(&d).packages.approved, ["hello"]);
        assert!(record(&d).packages.denied.is_empty());
        // A full record leaves the grant in place and says so.
        let mut full = record(&d);
        full.packages.approved = (0..256).map(|i| format!("p{i}")).collect();
        d.history.as_mut().unwrap().write(full);
        d.sessions.get_mut("a").unwrap().record.packages.clear();
        let _peer = request(&mut d, "package");
        sender
            .send(Completed::Granted {
                reply: goblins_protocol::Reply::new(None, "ready", None),
                detail: None,
                output: None,
            })
            .unwrap();
        d.tick().unwrap();
        assert_eq!(d.sessions["a"].record.packages, ["hello"]);
        assert!(
            d.sessions["a"]
                .record
                .detail
                .as_deref()
                .unwrap()
                .contains("not recorded for resume")
        );
        assert!(!record(&d).packages.approved.contains(&"hello".to_string()));
        // The stop is recorded once, and the record is final afterwards.
        sender
            .send(Completed::Stopped { exit_code: Some(0) })
            .unwrap();
        d.tick().unwrap();
        let stopped = record(&d).stopped.unwrap();
        assert_eq!(
            set(&mut d, "a", json!({"session_id":"late"})).unwrap_err(),
            conflict()
        );
        assert_eq!(
            d.update_record("a", |r| r.update(|r| r.stopped = Some(0))),
            Ok(false)
        );
        assert_eq!(record(&d).stopped, Some(stopped));
        drop(d);
        let records = crate::history::Store::open(&path.join("history"), "test").unwrap();
        assert_eq!(records.records["a"].stopped, Some(stopped));
        assert_eq!(records.records["a"].variables["session_id"], "c819");
        drop(records);
        fs::remove_dir_all(path).unwrap();
    }
    #[test]
    fn a_recreated_goblin_continues_its_record_and_merges_a_resumed_conversation() {
        use crate::history::Record;
        let mut d = daemon();
        let path = d.state.clone();
        d.open_history(Ok(path.join("history")), 100);
        let mut line = Record::new("r0", "shell", "morb", 1);
        line.ran_as("g0");
        line.packages.approved = vec!["jq".into()];
        line.variables.insert("session_id".into(), "c1".into());
        line.stopped = Some(2);
        let mut other = Record::new("o1", "shell", "grib", 0);
        other.ran_as("g1");
        other.packages.approved = vec!["tree".into()];
        other.variables.insert("session_id".into(), "c2".into());
        other.stopped = Some(1);
        let store = d.history.as_mut().unwrap();
        store.write(line.clone());
        store.write(other);
        // Goblin s5, recreated from r0, reaches running with one more grant.
        session_with_results_id(&mut d, "s5", vec![]);
        let mut a = d.sessions.remove("s5").unwrap();
        a.pending = None;
        a.line = "r0".into();
        a.variables = line.variables.clone();
        a.recreated_from = Some(Box::new(line));
        a.record.packages = vec!["jq".into(), "hello".into()];
        d.record_started(&a);
        d.sessions.insert("s5".into(), a);
        let record =
            |d: &Controller, id: &str| d.history.as_ref().unwrap().records.get(id).cloned();
        let r0 = record(&d, "r0").unwrap();
        assert_eq!(r0.sessions, ["g0", "s5"]);
        assert_eq!(r0.packages.approved, ["jq", "hello"]);
        assert_eq!(r0.stopped, None);
        assert!(record(&d, "s5").is_none());
        // It resumes o1's conversation: one record per conversation.
        assert_eq!(
            d.sandbox_control(
                "s5",
                "sessions.set-variables",
                json!({"variables":{"session_id":"c2"}})
            )
            .unwrap()["recorded"],
            true
        );
        let r0 = record(&d, "r0").unwrap();
        assert_eq!(r0.sessions, ["g1", "g0", "s5"]);
        assert_eq!(r0.packages.approved, ["jq", "hello", "tree"]);
        assert_eq!(r0.created, 0);
        assert!(d.history.as_ref().unwrap().deleting.contains("o1"));
        assert_eq!(d.history.as_ref().unwrap().resolve("g1").unwrap().id, "r0");
        drop(d);
        fs::remove_dir_all(path).unwrap();
    }
    #[test]
    fn a_record_is_refused_while_a_goblin_runs_it_and_retries_return_the_launch() {
        use crate::history::Record;
        let mut d = daemon();
        let path = d.state.clone();
        d.open_history(Ok(path.join("history")), 100);
        let store = d.history.as_mut().unwrap();
        // A record that ran as goblins r0, r1 and r2.
        let mut root = Record::new("r0", "shell", "morb", 1);
        root.ran_as("r0");
        root.packages.approved = vec!["hello".into()];
        root.variables.insert("session_id".into(), "c819".into());
        root.ran_as("r1");
        root.ran_as("r2");
        store.write(root);
        store.write(Record::new("a3", "shell", "ugluk", 1));
        let mut moved = Record::new("moved", "shell", "zog", 3);
        moved.cwd = Some("/nonexistent-goblins-recreate-cwd".into());
        store.write(moved);
        let (mut host, _peer) = host_connection();
        let recreate = |key: &str, target: &str| {
            json!({"key":key,"configuration":"/missing-goblins-test-manifest","recreate":target,
                "rows":24,"cols":80})
        };
        let mut start = |d: &mut Controller, p: Value| d.host(&mut host, "sessions.start", p);
        let mut with_parent = recreate("k0", "r0");
        with_parent["parent"] = json!("r0");
        assert!(
            start(&mut d, with_parent)
                .unwrap_err()
                .1
                .contains("--parent")
        );
        let mut with_name = recreate("k0", "r0");
        with_name["name"] = json!("shell");
        assert_eq!(start(&mut d, with_name).unwrap_err().0, -32602);
        assert!(
            start(&mut d, recreate("k0", "moved"))
                .unwrap_err()
                .1
                .contains("no longer exists")
        );
        assert_eq!(
            start(&mut d, recreate("k0", "nobody")).unwrap_err().0,
            -32004
        );
        // Names are labels only.
        assert_eq!(start(&mut d, recreate("k0", "morb")).unwrap_err().0, -32004);
        // The record names the configuration and agent name; an old goblin
        // id finds it.
        let first = start(&mut d, recreate("k1", "r1")).unwrap();
        let id = first["session"].as_str().unwrap().to_string();
        let record = &d.sessions[&id].record;
        assert_eq!(
            (record.name.as_str(), record.agent_name.as_str()),
            ("shell", "morb")
        );
        assert_eq!(record.recreated_from.as_deref(), Some("r0"));
        assert_eq!(d.sessions[&id].line, "r0");
        assert_eq!(d.sessions[&id].variables["session_id"], "c819");
        // A retry is the same launch; another recreate is refused.
        assert_eq!(start(&mut d, recreate("k1", "r1")).unwrap(), first);
        let refused =
            |d: &mut Controller,
             start: &mut dyn FnMut(&mut Controller, Value) -> std::result::Result<Value, Fault>,
             key: &str,
             target: &str| { start(d, recreate(key, target)).unwrap_err().1 };
        for target in ["r0", "r1", "r2", "c819"] {
            assert!(refused(&mut d, &mut start, "k2", target).contains("is live and runs it"));
        }
        // Released when that session ends.
        d.sessions.get_mut(&id).unwrap().worker.take();
        // The original goblin running its own record refuses too.
        let _a3 = live(&mut d, "a3", None);
        assert!(refused(&mut d, &mut start, "k7", "a3").contains("goblin a3 (a3)"));
        d.sessions.get_mut("a3").unwrap().worker.take();
        // A goblin already using the name leaves a new name.
        let _morb = live(&mut d, "morb", None);
        let again = start(&mut d, recreate("k4", "r0")).unwrap();
        assert_ne!(again["agent_name"], "morb");
        drop(d);
        fs::remove_dir_all(path).unwrap();
        // A --workspace daemon records nothing and recreates nothing.
        let path = unix::temp_directory().unwrap();
        let mut d = Controller::new(&path, Some(path.clone())).unwrap();
        d.open_history(Ok(path.join("history")), 100);
        let (mut host, _peer) = host_connection();
        assert!(
            d.host(&mut host, "sessions.start", recreate("k1", "r0"))
                .unwrap_err()
                .1
                .contains("--workspace")
        );
        drop(d);
        fs::remove_dir_all(path).unwrap();
    }
    #[test]
    fn a_second_daemon_runs_without_history_and_records_its_goblins_on_takeover() {
        let mut first = daemon();
        let mut second = daemon();
        let shared = first.state.join("history");
        first.open_history(Ok(shared.clone()), 100);
        second.open_history(Ok(shared.clone()), 100);
        assert_eq!(first.history_status()["owner"], true);
        let status = second.history_status();
        assert_eq!(status["owner"], false);
        let reason = status["reason"].as_str().unwrap();
        assert!(
            reason.contains(&format!("--state-dir {}) owns", first.state.display())),
            "{reason}"
        );
        assert!(reason.contains("takes it over once it is released"));
        // Its own goblins still cannot bind the store.
        assert_eq!(
            second.history_directory.as_deref(),
            Some(fs::canonicalize(&shared).unwrap().as_path())
        );
        // A goblin running meanwhile keeps its variables in the server.
        let _g = live(&mut second, "g", None);
        let _kid = live(&mut second, "kid", Some("g"));
        second.sessions.get_mut("g").unwrap().record.packages = vec!["hello".into()];
        for id in ["g", "kid"] {
            assert_eq!(
                second
                    .sandbox_control(
                        id,
                        "sessions.set-variables",
                        json!({"variables":{"session_id":"c819"}})
                    )
                    .unwrap()["recorded"],
                false
            );
        }
        // Released, the store is taken over without a restart, and the
        // running root goblin gets its record.
        let paths = [first.state.clone(), second.state.clone()];
        drop(first);
        let deadline = Instant::now() + std::time::Duration::from_secs(10);
        while second.history.is_err() && Instant::now() < deadline {
            second.tick().unwrap();
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
        assert_eq!(second.history_status()["owner"], true);
        let store = second.history.as_ref().unwrap();
        assert_eq!(store.records.keys().collect::<Vec<_>>(), ["g"]);
        let record = &store.records["g"];
        assert_eq!(record.packages.approved, ["hello"]);
        assert_eq!(record.variables["session_id"], "c819");
        assert_eq!(record.sessions, ["g"]);
        drop(second);
        for path in paths {
            fs::remove_dir_all(path).unwrap();
        }
    }
    #[test]
    fn requests_are_logged_and_a_full_queue_keeps_outcomes_in_order() {
        let mut d = daemon();
        let path = d.state.clone();
        let (service, queue) = crate::messaging::Service::detached(1);
        d.messaging = service;
        let _results = live(&mut d, "a", None);
        let (worker, _results, _commands) = Worker::listening();
        d.sessions.get_mut("a").unwrap().worker = Some(worker);
        d.tick().unwrap();
        while queue.try_recv().is_ok() {}
        // Fill the queue so the next batches cannot be handed over.
        d.messaging
            .commands
            .try_send(crate::messaging::Work::Delivery {
                actor: "a".into(),
                revision: 0,
                submitted: false,
            })
            .unwrap();
        let (mut c, _peer) = connection(Role::Sandbox("a".into()));
        c.initialized = true;
        d.dispatch(
            &mut c,
            rpc::Call {
                jsonrpc: "2.0".into(),
                id: Some(json!(1)),
                method: "permissions.request".into(),
                params: json!({"kind":"package","package":"hello","reason":"test"}),
            },
        );
        // The request waits for its terminal result.
        assert!(c.output.is_empty());
        let request = d.permissions.back().unwrap().clone();
        assert_eq!(request.state, "pending");
        d.connections.push(c);
        d.decide(
            Decision {
                session: "a".into(),
                request: request.id.clone(),
                approval: request.approval.clone(),
                approved: false,
            },
            "host",
        )
        .unwrap();
        d.tick().unwrap();
        assert_eq!(d.operation_outbox.len(), 3);
        assert!(operations(&queue).is_empty());
        d.tick().unwrap();
        let kinds: Vec<_> = operations(&queue).iter().map(|e| e.kind()).collect();
        assert_eq!(
            kinds,
            [
                "operation.requested",
                "operation.decided",
                "operation.completed"
            ]
        );
        assert!(d.operation_outbox.is_empty());
        // A backlog refuses new requests rather than dropping outcomes.
        let filler = crate::operation::Event {
            request: "x".into(),
            session: "a".into(),
            actor: "a".into(),
            change: crate::operation::Change::Decided { approved: true },
        };
        d.operation_outbox.extend(std::iter::repeat_n(filler, 1024));
        let (mut c, _peer) = connection(Role::Sandbox("a".into()));
        c.initialized = true;
        assert_eq!(
            dispatch(
                &mut d,
                &mut c,
                "permissions.request",
                json!({"kind":"package","package":"hello","reason":"again"}),
                Some(json!(2))
            )["error"]["code"],
            -32010
        );
        drop(d);
        fs::remove_dir_all(path).unwrap();
    }
    fn live(
        d: &mut Controller,
        id: &str,
        parent: Option<&str>,
    ) -> std::sync::mpsc::SyncSender<Completed> {
        let path = match parent {
            Some(p) => format!("{}/{id}", d.sessions[p].record.path),
            None => id.into(),
        };
        session_with_results_id(d, id, vec![]);
        let (worker, results) = Worker::idle();
        let a = d.sessions.get_mut(id).unwrap();
        a.worker = Some(worker);
        a.pending = None;
        a.record.agent_name = id.into();
        a.record.parent = parent.map(Into::into);
        a.record.path = path;
        results
    }
    fn host_connection() -> (Connection, UnixStream) {
        let (mut c, peer) = connection(Role::Host);
        c.initialized = true;
        (c, peer)
    }
    /// Raid events the controller handed to the (detached) messaging queue.
    fn sent(queue: &std::sync::mpsc::Receiver<crate::messaging::Work>) -> Vec<(String, Value)> {
        queue
            .try_iter()
            .flat_map(|work| match work {
                crate::messaging::Work::Sync { raids, .. }
                | crate::messaging::Work::Call { raids, .. } => raids,
                crate::messaging::Work::Delivery { .. } => vec![],
            })
            .map(|e| (e.kind().to_string(), e.data()))
            .collect()
    }
    #[test]
    fn raid_membership_is_reconciled_after_every_way_a_session_ends() {
        let mut d = daemon();
        let path = d.state.clone();
        let (service, queue) = crate::messaging::Service::detached(64);
        d.messaging = service;
        let (mut host, _peer) = host_connection();
        let _a = live(&mut d, "a", None);
        let exits = live(&mut d, "b", None);
        let _c = live(&mut d, "c", None);
        let fails = live(&mut d, "e", None);
        let _f = live(&mut d, "f", None);
        session_with_results_id(&mut d, "w", vec![]);
        d.sessions.get_mut("w").unwrap().pending = None;
        let record = d
            .host(
                &mut host,
                "raids.invite",
                json!({"raid":"review","sessions":["a","b","c","e","f","w"]}),
            )
            .unwrap();
        assert_eq!(record["owner"], "a");
        assert_eq!(d.sessions["f"].record.raid.as_ref().unwrap().owner, "a");
        d.tick().unwrap();
        let events = sent(&queue);
        assert_eq!(events[0].0, "raid.created");
        // Worker loss: the completed test worker finishes on the first tick.
        assert_eq!(events.last().unwrap().0, "raid.left");
        assert_eq!(events.last().unwrap().1["session"], "w");
        assert!(d.sessions["w"].record.raid.is_none());
        // Stop, natural exit and failed launch.
        d.stop("c", Some("stopped by host".into())).unwrap();
        exits
            .send(Completed::Stopped { exit_code: Some(0) })
            .unwrap();
        fails.send(Completed::Failed("boom".into())).unwrap();
        d.tick().unwrap();
        let left: Vec<_> = sent(&queue)
            .into_iter()
            .map(|(kind, data)| (kind, data["session"].clone(), data["reason"].clone()))
            .collect();
        assert_eq!(
            left,
            [
                ("raid.left".into(), json!("b"), json!("stopped")),
                ("raid.left".into(), json!("c"), json!("stopped")),
                ("raid.left".into(), json!("e"), json!("launch-failed")),
            ]
        );
        // The owner stopping dissolves the raid; the other member keeps running.
        d.stop("a", None).unwrap();
        d.tick().unwrap();
        let events = sent(&queue);
        assert_eq!(events.last().unwrap().0, "raid.dissolved");
        assert_eq!(events.last().unwrap().1["reason"], "owner-left");
        assert_eq!(d.sessions["f"].record.state, "running");
        assert!(d.sessions["f"].record.raid.is_none());
        assert!(d.snapshot().raids.is_empty());
        drop(d);
        fs::remove_dir_all(path).unwrap();
    }
    #[test]
    fn a_stop_never_publishes_the_old_membership() {
        let mut d = daemon();
        let path = d.state.clone();
        let (mut host, _peer) = host_connection();
        let _a = live(&mut d, "a", None);
        let _b = live(&mut d, "b", None);
        d.host(
            &mut host,
            "raids.invite",
            json!({"raid":"review","sessions":["a","b"]}),
        )
        .unwrap();
        let (mut subscriber, _sub_peer) = host_connection();
        dispatch(
            &mut d,
            &mut subscriber,
            "state.subscribe",
            json!({}),
            Some(json!(1)),
        );
        d.connections.push(subscriber);
        dispatch(
            &mut d,
            &mut host,
            "sessions.stop",
            json!({"session":"b"}),
            Some(json!(2)),
        );
        d.publish();
        let data = d.connections[0].output.pop_back().unwrap();
        let body = data.windows(4).position(|w| w == b"\r\n\r\n").unwrap() + 4;
        let snapshot = &rpc::parse(&data[body..]).unwrap()["params"]["snapshot"];
        let b = snapshot["sessions"]
            .as_array()
            .unwrap()
            .iter()
            .find(|r| r["id"] == "b")
            .unwrap();
        assert_eq!(b["state"], "stopping");
        assert_eq!(b["raid"], Value::Null);
        assert_eq!(snapshot["raids"][0]["members"].as_array().unwrap().len(), 1);
        drop(d);
        fs::remove_dir_all(path).unwrap();
    }
    #[test]
    fn launch_joins_follow_the_parent_and_retries_do_not_join_again() {
        let mut d = daemon();
        let path = d.state.clone();
        let manifest: crate::config::Manifest = serde_json::from_value(json!({
            "api": 2, "helper_api": 5,
            "goblins": {"shell": {"build_spec": "/nonexistent/spec.json", "args": [], "env": {},
                "client_package": "/unused", "helper": "/unused", "flake": "unused",
                "allowed_children": ["shell"]}},
            "scopes": {},
        }))
        .unwrap();
        let branch = std::sync::Arc::new(manifest.branch("shell", None).unwrap().1);
        let _root = live(&mut d, "root", None);
        let _kid = live(&mut d, "kid", Some("root"));
        let _grandkid = live(&mut d, "grandkid", Some("kid"));
        let _other = live(&mut d, "other", None);
        for id in ["root", "kid", "grandkid", "other"] {
            let a = d.sessions.get_mut(id).unwrap();
            a.inheritance = Some(Inheritance::fixture("shell", branch.clone()));
            a.record.configuration = "/loaded.json".into();
        }
        let child = |key: &str| json!({"key":key,"name":"shell","agent_name":null,"parent":null,"inherit_raid":true});
        assert_eq!(
            d.sandbox_control("root", "sessions.start", child("k1"))
                .unwrap_err(),
            (-32009, "parent is not in a raid".into())
        );
        assert!(d.launches.is_empty());
        d.sandbox_control("root", "raids.create", json!({"raid":"review"}))
            .unwrap();
        let before = d.raid_outbox.len();
        let accepted = d
            .sandbox_control("root", "sessions.start", child("k1"))
            .unwrap();
        let id = accepted["session"].as_str().unwrap().to_string();
        assert_eq!(d.sessions[&id].record.state, "starting");
        let raid = d.sessions[&id].record.raid.clone().unwrap();
        assert_eq!(
            (raid.name.as_str(), raid.owner.as_str()),
            ("review", "root")
        );
        let joined = d.raid_outbox.back().unwrap();
        assert_eq!(
            (joined.kind(), joined.actor.as_str()),
            ("raid.joined", "root")
        );
        assert_eq!(d.raid_outbox.len(), before + 1);
        // An identical retry returns the original acceptance without joining.
        assert_eq!(
            d.sandbox_control("root", "sessions.start", child("k1"))
                .unwrap(),
            accepted
        );
        assert_eq!(d.raid_outbox.len(), before + 1);
        // Direct children only; other branches are not even visible.
        assert_eq!(
            d.sandbox_control("root", "raids.invite", json!({"sessions":["kid/grandkid"]}))
                .unwrap_err(),
            (-32602, "only direct children can be invited".into())
        );
        assert_eq!(
            d.sandbox_control("root", "raids.invite", json!({"sessions":["other"]}))
                .unwrap_err()
                .0,
            -32004
        );
        assert_eq!(
            d.sandbox_control("root", "raids.invite", json!({"sessions":["kid"]}))
                .unwrap()["owner"],
            "root"
        );
        // The invited child may invite its own child; ownership is unchanged.
        let record = d
            .sandbox_control("kid", "raids.invite", json!({"sessions":["grandkid"]}))
            .unwrap();
        assert_eq!(record["owner"], "root");
        assert_eq!(record["members"].as_array().unwrap().len(), 4);
        // A goblin in another raid cannot be moved.
        d.sandbox_control("other", "raids.create", json!({"raid":"elsewhere"}))
            .unwrap();
        let (mut host, _peer) = host_connection();
        assert_eq!(
            d.host(
                &mut host,
                "raids.invite",
                json!({"raid":"review","sessions":["other"]})
            )
            .unwrap_err(),
            (
                -32009,
                "goblin 'other' is already in raid 'elsewhere'".into()
            )
        );
        // Host launches create or join by name; a retry after dissolution
        // still returns the original acceptance.
        let mut start = launch("h1", Some("fresh"));
        start["raid"] = json!("brand-new");
        let accepted = d.host(&mut host, "sessions.start", start.clone()).unwrap();
        let fresh = accepted["session"].as_str().unwrap();
        assert_eq!(d.raids.get("brand-new").unwrap().owner, fresh);
        d.host(&mut host, "raids.destroy", json!({"raid":"brand-new"}))
            .unwrap();
        assert_eq!(
            d.host(&mut host, "sessions.start", start).unwrap(),
            accepted
        );
        assert!(d.raids.get("brand-new").is_none());
        let mut changed = launch("h1", Some("fresh"));
        changed["raid"] = json!("other-name");
        assert_eq!(
            d.host(&mut host, "sessions.start", changed).unwrap_err(),
            conflict()
        );
        drop(d);
        fs::remove_dir_all(path).unwrap();
    }
    #[test]
    fn host_invites_validate_every_target_and_roles_never_move_ownership() {
        let mut d = daemon();
        let path = d.state.clone();
        let (mut host, _peer) = host_connection();
        let _a = live(&mut d, "a", None);
        let _b = live(&mut d, "b", None);
        session_with_results_id(&mut d, "dead", vec![]);
        d.sessions.get_mut("dead").unwrap().record.state = "stopped".into();
        for (sessions, code) in [
            (json!(["a", "absent"]), -32004),
            (json!(["a", "dead"]), -32009),
            (json!([]), -32602),
        ] {
            let error = d
                .host(
                    &mut host,
                    "raids.invite",
                    json!({"raid":"review","sessions":sessions}),
                )
                .unwrap_err();
            assert_eq!(error.0, code, "{error:?}");
            assert!(d.raids.get("review").is_none());
        }
        assert_eq!(
            d.host(
                &mut host,
                "raids.invite",
                json!({"raid":"Bad Name","sessions":["a"]})
            )
            .unwrap_err()
            .0,
            -32602
        );
        d.host(
            &mut host,
            "raids.invite",
            json!({"raid":"review","sessions":["a","b"]}),
        )
        .unwrap();
        let record = d
            .host(
                &mut host,
                "raids.set_role",
                json!({"raid":"review","session":"b","role":"owner"}),
            )
            .unwrap();
        assert_eq!(record["owner"], "a");
        assert_eq!(record["members"][1]["role"], "owner");
        assert_eq!(record["members"][1]["owner"], false);
        for role in [json!(null), json!("")] {
            assert_eq!(
                d.host(
                    &mut host,
                    "raids.set_role",
                    json!({"raid":"review","session":"b","role":role})
                )
                .unwrap_err()
                .0,
                -32602
            );
        }
        assert_eq!(
            d.sandbox_control("b", "raids.create", json!({"raid":"another"}))
                .unwrap_err()
                .1,
            "goblin 'b' is already in raid 'review'"
        );
        assert_eq!(
            d.sandbox_control("b", "raids.leave", json!({})).unwrap(),
            json!({"accepted":true,"dissolved":false})
        );
        assert_eq!(
            d.sandbox_control("b", "raids.members", json!({}))
                .unwrap_err()
                .0,
            -32009
        );
        assert_eq!(
            d.sandbox_control("a", "raids.leave", json!({})).unwrap(),
            json!({"accepted":true,"dissolved":true})
        );
        assert_eq!(d.sessions["b"].record.state, "running");
        drop(d);
        fs::remove_dir_all(path).unwrap();
    }
    #[test]
    fn the_raid_outbox_survives_a_full_queue_and_bounds_only_additions() {
        let mut d = daemon();
        let path = d.state.clone();
        let (service, queue) = crate::messaging::Service::detached(1);
        d.messaging = service;
        let (mut host, _peer) = host_connection();
        let _a = live(&mut d, "a", None);
        let _b = live(&mut d, "b", None);
        d.tick().unwrap();
        assert!(sent(&queue).is_empty());
        // Sent even though the directory is unchanged.
        d.host(
            &mut host,
            "raids.invite",
            json!({"raid":"one","sessions":["a"]}),
        )
        .unwrap();
        d.tick().unwrap();
        let events = sent(&queue);
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].0, "raid.created");
        // A full queue keeps the batch, in order, for the next tick.
        d.messaging
            .commands
            .try_send(crate::messaging::Work::Delivery {
                actor: "a".into(),
                revision: 0,
                submitted: false,
            })
            .unwrap();
        d.host(
            &mut host,
            "raids.invite",
            json!({"raid":"one","sessions":["b"]}),
        )
        .unwrap();
        d.sandbox_control("b", "raids.set_role", json!({"role":"builder"}))
            .unwrap();
        d.tick().unwrap();
        assert_eq!(d.raid_outbox.len(), 2);
        assert!(sent(&queue).is_empty());
        d.tick().unwrap();
        let kinds: Vec<_> = sent(&queue).into_iter().map(|(k, _)| k).collect();
        assert_eq!(kinds, ["raid.joined", "raid.role"]);
        assert!(d.raid_outbox.is_empty());
        // A full outbox or an exhausted budget refuses additions, not removals.
        let filler = crate::raid::Event {
            actor: "host".into(),
            change: crate::raid::Change::Role {
                raid: "x".into(),
                session: "x".into(),
                role: "x".into(),
            },
        };
        d.raid_outbox
            .extend(std::iter::repeat_n(filler, raids::RAID_OUTBOX));
        assert_eq!(
            d.sandbox_control("b", "raids.set_role", json!({"role":"reviewer"}))
                .unwrap_err()
                .0,
            -32010
        );
        d.sandbox_control("b", "raids.leave", json!({})).unwrap();
        assert_eq!(d.raid_outbox.len(), raids::RAID_OUTBOX + 1);
        d.raid_outbox.clear();
        d.raid_events = 4096;
        assert_eq!(
            d.host(
                &mut host,
                "raids.invite",
                json!({"raid":"one","sessions":["b"]})
            )
            .unwrap_err()
            .0,
            -32010
        );
        d.host(&mut host, "raids.destroy", json!({"raid":"one"}))
            .unwrap();
        assert_eq!(d.raid_outbox.back().unwrap().kind(), "raid.dissolved");
        // Orderly shutdown hands the outbox over before messaging closes.
        while queue.try_recv().is_ok() {}
        drop(d);
        assert_eq!(sent(&queue).last().unwrap().0, "raid.dissolved");
        fs::remove_dir_all(path).unwrap();
    }
}
