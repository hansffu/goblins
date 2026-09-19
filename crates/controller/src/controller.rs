//! Single authority/event loop, with independent blocking workers per session.
use crate::{
    Result,
    session::{Cancel, Identity},
    terminal::Terminal,
    unix,
    worker::{Completed, Work, Worker},
};
use goblins_protocol::{
    Request,
    rpc::{self, Decoder, PermissionParams},
};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, VecDeque},
    fs::{self, File},
    io::{self, Read, Write},
    os::unix::{
        fs::PermissionsExt,
        net::{UnixListener, UnixStream},
    },
    path::{Path, PathBuf},
    time::Instant,
};

const HOSTS: usize = 32;
const SESSIONS: usize = 16;
const GOBLIN_NAMES: &str = include_str!("goblin-names.txt");
const QUEUE: usize = 2 * 1024 * 1024;
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SessionRecord {
    pub id: String,
    pub agent_name: String,
    /// Reusable Nix configuration key, distinct from the per-launch agent name.
    pub name: String,
    pub configuration: String,
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
    pub detail: Option<String>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PermissionRecord {
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
}
struct Pending {
    id: String,
    serial: u64,
    connection: u64,
    cancel: Cancel,
}
struct Active {
    created: u64,
    record: SessionRecord,
    worker: Option<Worker>,
    listener: Option<UnixListener>,
    pending: Option<Pending>,
    terminal: Terminal,
    directory: PathBuf,
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
    subscription: Option<String>,
}
impl Connection {
    fn new(id: u64, role: Role, peer: UnixStream) -> Result<Self> {
        peer.set_nonblocking(true)?;
        let limit = if matches!(role, Role::Host) {
            16384
        } else {
            4096
        };
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
            subscription: None,
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
        match self.peer.read(&mut bytes) {
            Ok(0) => {
                self.dead = true;
                return None;
            }
            Ok(n) => {
                if self.waiting.is_some() || self.input.len() + n > 32768 {
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
        if self.waiting.is_some() {
            return None;
        }
        match self.decoder.push(&self.input, Instant::now()) {
            Ok((n, body)) => {
                self.input.drain(..n);
                if let Some(body) = body {
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
                    let limit = if matches!(self.role, Role::Host) {
                        16384
                    } else {
                        4096
                    };
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
    name: String,
    agent_name: Option<String>,
    #[serde(default)]
    cwd: Option<PathBuf>,
    rows: u16,
    cols: u16,
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
fn valid_agent_name(name: &str) -> bool {
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
    state: PathBuf,
    workspace: Option<PathBuf>,
    _lock: File,
    listener: UnixListener,
    instance: String,
    next: u64,
    sequence: u64,
    connections: Vec<Connection>,
    sessions: BTreeMap<String, Active>,
    permissions: VecDeque<PermissionRecord>,
    launches: BTreeMap<String, (Start, Value)>,
    shutdown: Option<(u64, Instant)>,
    dirty: bool,
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
        let instance = bytes.iter().map(|b| format!("{b:02x}")).collect();
        Ok(Self {
            state,
            workspace,
            _lock: lock,
            listener,
            instance,
            next: 0,
            sequence: 0,
            connections: vec![],
            sessions: BTreeMap::new(),
            permissions: VecDeque::new(),
            launches: BTreeMap::new(),
            shutdown: None,
            dirty: false,
        })
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
                .filter(|(_, a)| a.worker.is_none() && a.terminal.can_evict())
                .min_by_key(|(_, a)| a.created)
                .map(|(id, _)| id.clone())
            {
                if let Some(a) = self.sessions.remove(&id) {
                    let _ = fs::remove_dir_all(&a.directory);
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
    fn allocate_name(&self, requested: Option<&str>) -> std::result::Result<String, Fault> {
        // Worker ownership includes startup and teardown, even if a final state
        // has arrived before cleanup finishes. Allocation and insertion both
        // happen in this event loop before another host call can run.
        let available = |name: &str| {
            !self
                .sessions
                .values()
                .any(|a| a.worker.is_some() && a.record.agent_name == name)
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
    fn resolve_session(&self, target: &str) -> std::result::Result<String, Fault> {
        if self.sessions.contains_key(target) {
            return Ok(target.into());
        }
        // Names address live sessions only. Historical names may be shared;
        // historical operations must use the immutable session ID.
        self.sessions
            .values()
            .find(|a| a.worker.is_some() && a.record.agent_name == target)
            .map(|a| a.record.id.clone())
            .ok_or_else(missing)
    }
    fn detach(&mut self, id: &str) -> std::result::Result<Value, Fault> {
        let a = self.sessions.get_mut(id).ok_or_else(missing)?;
        a.terminal.detach().map_err(|e| (-32009, e.to_string()))?;
        a.record.terminal_attached = a.terminal.attached();
        a.record.terminal_detached = a.terminal.detached;
        self.dirty = true;
        Ok(json!({"accepted":true}))
    }
    fn stop(&mut self, id: &str) -> std::result::Result<Value, Fault> {
        let a = self.sessions.get_mut(id).ok_or_else(missing)?;
        if let Some(w) = &a.worker {
            w.cancel.cancel();
            a.record.state = "stopping".into();
        }
        a.listener.take();
        if let Some(p) = a.pending.take() {
            p.cancel.cancel();
            if let Some(r) = self.permission_mut(&p.id) {
                r.state = "cancelled".into();
            }
            self.reply_permission(&p.id, "error", Some("session stopped".into()));
        }
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
    fn withdraw(&mut self, connection: u64) {
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
                    gone = Some(p.id);
                    break;
                }
            }
        }
        if let Some(id) = gone {
            if let Some(r) = self.permission_mut(&id) {
                r.state = "withdrawn".into();
            }
            self.dirty = true;
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
                    json!({"instance":self.instance,"state":if self.shutdown.is_some(){"stopping"}else{"running"},"sessions":self.sessions.values().filter(|a| a.worker.is_some()).count()}),
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
            "sessions.start" => {
                let p: Start = params(value)?;
                if !goblins_protocol::identifier(&p.key)
                    || !goblins_protocol::identifier(&p.name)
                    || !Path::new(&p.configuration).is_absolute()
                    || p.cwd.as_ref().is_some_and(|cwd| !cwd.is_absolute())
                    || !dimensions(p.rows, p.cols)
                {
                    return Err((-32602, "invalid launch parameters".into()));
                }
                if let Some((old, result)) = self.launches.get(&p.key) {
                    return if old == &p {
                        Ok(result.clone())
                    } else {
                        Err(conflict())
                    };
                }
                let agent_name = self.allocate_name(p.agent_name.as_deref())?;
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
                        .filter(|(_, a)| a.worker.is_none() && a.terminal.can_evict())
                        .min_by_key(|(_, a)| a.created)
                        .map(|(id, _)| id.clone())
                        .ok_or_else(capacity)?;
                    if let Some(a) = self.sessions.remove(&old) {
                        let _ = fs::remove_dir_all(&a.directory);
                    }
                }
                let id = self.id("s");
                let directory = self.state.join(&id);
                unix::private_directory(&directory).map_err(|e| (-32010, e.to_string()))?;
                let path = directory.join("terminal.sock");
                let terminal = Terminal::new(&path).map_err(|e| (-32010, e.to_string()))?;
                let worker = Worker::start(
                    p.configuration.clone().into(),
                    p.name.clone(),
                    self.workspace.clone(),
                    p.cwd.clone(),
                    self.state.clone(),
                    directory.join("resources"),
                    (p.rows, p.cols),
                );
                let record = SessionRecord {
                    id: id.clone(),
                    agent_name,
                    name: p.name.clone(),
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
                    terminal_detached: false,
                    exit_code: None,
                    detail: None,
                };
                let result = json!({"session":id,"agent_name":record.agent_name,"state":"starting","terminal":record.terminal});
                self.sessions.insert(
                    id,
                    Active {
                        created: self.next,
                        record,
                        worker: Some(worker),
                        listener: None,
                        pending: None,
                        terminal,
                        directory,
                    },
                );
                self.launches.insert(p.key.clone(), (p, result.clone()));
                self.dirty = true;
                Ok(result)
            }
            "sessions.stop" => {
                let p: SessionId = params(value)?;
                let id = self.resolve_session(&p.session)?;
                self.stop(&id)
            }
            "sessions.detach" => {
                let p: SessionId = params(value)?;
                let id = self.resolve_session(&p.session)?;
                self.detach(&id)
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
            "permissions.decide" => {
                let p: Decision = params(value)?;
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
                let a = self.sessions.get_mut(&p.session).unwrap();
                let pending = a.pending.as_ref().unwrap();
                pending.cancel.cancel();
                if p.approved {
                    let r = self.permissions.iter().find(|r| r.id == p.request).unwrap();
                    a.worker
                        .as_ref()
                        .ok_or_else(conflict)?
                        .commands
                        .try_send(Work::Decide {
                            request: Request {
                                id: p.request.clone(),
                                package: r.package.clone(),
                                reason: r.reason.clone(),
                            },
                            approved: true,
                        })
                        .map_err(|_| capacity())?;
                } else {
                    a.pending.take();
                }
                let r = self.permission_mut(&p.request).unwrap();
                r.approved = Some(p.approved);
                r.state = if p.approved { "realizing" } else { "denied" }.into();
                if !p.approved {
                    self.reply_permission(&p.request, "denied", None);
                }
                self.dirty = true;
                Ok(json!({"accepted":true,"request":p.request,"approved":p.approved}))
            }
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
            _ => Err((-32601, "method unavailable on host endpoint".into())),
        }
    }
    fn dispatch(&mut self, c: &mut Connection, call: rpc::Call) {
        // JSON-RPC notifications get no response and cannot authorize mutations.
        let Some(id) = call.id else {
            return;
        };
        let outcome = (|| -> std::result::Result<Option<Value>, Fault> {
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
                    Decoder::new(4096, Some(Instant::now()))
                } else {
                    Decoder::new(16384, None)
                };
                return Ok(Some(
                    json!({"api":1,"instance":self.instance,"role":if matches!(c.role,Role::Host){"host"}else{"sandbox"},"features":if matches!(c.role,Role::Host){vec!["package-grants","same-daemon-reconnect","state-subscribe","raw-terminal","agent-names","server-control","terminal-reattach"]}else{vec!["package-grants","terminal-detach"]},"limits":{"header":256,"body":if matches!(c.role,Role::Host){16384}else{4096},"frame_seconds":3,"depth":32,"response_body":rpc::MAX_BODY,"calls":if matches!(c.role,Role::Host){4096}else{2},"connections":if matches!(c.role,Role::Host){HOSTS}else{8},"sessions":SESSIONS,"output_queue":QUEUE,"snapshot":900*1024,"terminal_buffer":65536}}),
                ));
            }
            if !c.initialized {
                return Err((-32001, "initialize first".into()));
            }
            match &c.role {
                Role::Host => self.host(c, &call.method, call.params).map(Some),
                Role::Sandbox(session) => {
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
                    if call.method != "permissions.request" {
                        return Err((-32601, "method unavailable on sandbox endpoint".into()));
                    }
                    if !c.input.is_empty() {
                        c.dead = true;
                        return Err((-32600, "unexpected trailing input".into()));
                    }
                    let p: PermissionParams = params(call.params)?;
                    if !p.validate() {
                        return Err((-32602, "invalid package or reason".into()));
                    }
                    let session = session.clone();
                    let a = self.sessions.get(&session).ok_or_else(missing)?;
                    if a.record.state != "running" || a.pending.is_some() {
                        return Err(conflict());
                    }
                    if a.record.packages.len() >= 64 {
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
                    let a = self.sessions.get_mut(&session).unwrap();
                    let w = a.worker.as_ref().ok_or_else(conflict)?;
                    let cancel = w.cancel.child();
                    w.commands
                        .try_send(Work::Preview {
                            approval: serial,
                            package: p.package.clone(),
                            cancel: cancel.clone(),
                        })
                        .map_err(|_| capacity())?;
                    a.pending = Some(Pending {
                        id: request.clone(),
                        serial,
                        connection: c.id,
                        cancel,
                    });
                    self.permissions.push_back(PermissionRecord {
                        id: request.clone(),
                        session,
                        agent_name: a.record.agent_name.clone(),
                        approval,
                        package: p.package,
                        reason: p.reason,
                        state: "pending".into(),
                        approved: None,
                        preview: None,
                        message: None,
                    });
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
    pub fn tick(&mut self) -> Result<()> {
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
                        initial_packages,
                        master,
                        listener,
                        identity,
                    } => {
                        a.terminal.master(master)?;
                        a.record.initial_packages = initial_packages;
                        a.record.identity = Some(identity);
                        if a.record.state == "starting" {
                            a.record.state = "running".into();
                            a.listener = Some(listener);
                        }
                        self.dirty = true;
                    }
                    Completed::Preview { approval, result } => {
                        if let Some(p) = &a.pending
                            && p.serial == approval
                            && let Some(r) = self.permission_mut(&p.id)
                            && r.state == "pending"
                        {
                            r.preview = Some(match result {
                                Ok(p) => json!(p),
                                Err(e) => json!({"error":bounded(&e)}),
                            });
                            self.dirty = true;
                        }
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
                    Completed::Granted { reply, detail } => {
                        if let Some(p) = a.pending.take() {
                            p.cancel.cancel();
                            if let Some(r) = self.permission_mut(&p.id) {
                                r.state = if reply.status == "ready" {
                                    "ready"
                                } else {
                                    "failed"
                                }
                                .into();
                                r.message = reply.message.clone();
                                if reply.status == "ready"
                                    && !a.record.packages.contains(&r.package)
                                {
                                    a.record.packages.push(r.package.clone());
                                }
                            }
                            self.reply_permission(&p.id, &reply.status, reply.message);
                        }
                        a.record.detail = detail.map(|e| bounded(&e));
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
                        }
                        self.dirty = true;
                    }
                }
            }
            if worker_finished {
                a.worker.take();
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
            self.sessions.insert(id, a);
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
            if let Some(call) = c.read() {
                self.dispatch(&mut c, call);
            }
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
fn bounded(text: &str) -> String {
    text.chars().take(512).collect()
}
impl Drop for Controller {
    fn drop(&mut self) {
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
        let id = "test-session".to_string();
        let directory = d.state.join(&id);
        unix::private_directory(&directory).unwrap();
        let terminal = Terminal::new(&directory.join("terminal.sock")).unwrap();
        d.sessions.insert(
            id.clone(),
            Active {
                created: 0,
                record: SessionRecord {
                    id: id.clone(),
                    agent_name: "snikk".into(),
                    name: "shell".into(),
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
                    detail: None,
                },
                worker: Some(Worker::completed(results)),
                listener: None,
                pending: Some(Pending {
                    id: "r".into(),
                    serial: 1,
                    connection: 99,
                    cancel: Cancel::default(),
                }),
                terminal,
                directory,
            },
        );
        d.permissions.push_back(PermissionRecord {
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
            assert_eq!(d.allocate_name(Some("shell")).unwrap_err().0, -32009);
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
                },
                Completed::Stopped { exit_code: None },
            ],
        );
        d.stop(&id).unwrap();
        d.tick().unwrap();
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
        let (mut c, _peer) = connection(Role::Sandbox("endpoint-session".into()));
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
            "sessions.start",
            "sessions.stop",
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
}
