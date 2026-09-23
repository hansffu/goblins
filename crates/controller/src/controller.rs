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
    rpc::{self, Decoder, PermissionParams},
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

const HOSTS: usize = 32;
const SESSIONS: usize = 16;
const GOBLIN_NAMES: &str = include_str!("goblin-names.txt");
const QUEUE: usize = 2 * 1024 * 1024;
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SessionRecord {
    #[serde(default)]
    pub docker_enabled: bool,
    #[serde(default)]
    pub docker_scope: Option<String>,
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
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PermissionRecord {
    #[serde(default = "goblins_protocol::package_kind")]
    pub kind: String,
    #[serde(default)]
    pub scope: Option<String>,
    #[serde(default)]
    pub anonymous: bool,
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
    initial_output: Option<PathBuf>,
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
    inheritance: Option<Inheritance>,
    exit_watch: Option<OwnedFd>,
    granted_outputs: BTreeMap<String, PathBuf>,
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
    name: String,
    agent_name: Option<String>,
    #[serde(default)]
    parent: Option<String>,
    #[serde(default)]
    cwd: Option<PathBuf>,
    rows: u16,
    cols: u16,
    #[serde(default)]
    detached: bool,
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
            "description":record.description,"state":record.state,
            "initial_packages":record.initial_packages,
            "packages":record.packages,"docker_scope":record.docker_scope,"docker_enabled":record.docker_enabled,
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
    fn start(&mut self, mut p: Start, scope: Option<&str>) -> std::result::Result<Value, Fault> {
        if self.shutdown.is_some() {
            return Err(conflict());
        }
        let key = format!("{}:{}", scope.unwrap_or("host"), p.key);
        let parent = match (scope, p.parent.as_deref()) {
            (Some(root), None | Some(".")) => Some(root.to_string()),
            (_, Some(target)) => Some(self.resolve_scoped(target, scope)?),
            (None, None) => None,
        };
        let inherited = if let Some(id) = &parent {
            let a = &self.sessions[id];
            if a.record.state != "running" || a.worker.is_none() {
                return Err(conflict());
            }
            if p.name != a.record.name {
                return Err((
                    -32602,
                    "children must use their parent's configuration".into(),
                ));
            }
            // A host --parent launch follows the same restriction, using the
            // parent's already selected config rather than a newer manifest.
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
            || !dimensions(p.rows, p.cols)
        {
            return Err((-32602, "invalid launch parameters".into()));
        }
        if let Some((old, result)) = self.launches.get(&key) {
            return if old == &p {
                Ok(result.clone())
            } else {
                Err(conflict())
            };
        }
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
        let source = match inherited {
            Some(inherited) => Source::Child(inherited),
            None => Source::Host {
                configuration: p.configuration.clone().into(),
                name: p.name.clone(),
                workspace: self.workspace.clone(),
                cwd: p.cwd.clone(),
                state: self.state.clone(),
            },
        };
        let worker = Worker::start(source, directory.join("resources"), (p.rows, p.cols));
        let record = SessionRecord {
            docker_enabled: false,
            docker_scope: None,
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
        };
        let result = json!({"session":id,"agent_name":record.agent_name,"path":record.path,"parent":record.parent,"state":"starting","terminal":record.terminal});
        self.sessions.insert(
            id,
            Active {
                inheritance: None,
                exit_watch: None,
                granted_outputs: BTreeMap::new(),
                created: self.next,
                record,
                worker: Some(worker),
                listener: None,
                pending: None,
                terminal,
                directory,
            },
        );
        self.launches.insert(key, (p, result.clone()));
        self.dirty = true;
        Ok(result)
    }
    fn sandbox_control(
        &mut self,
        scope: &str,
        method: &str,
        value: Value,
    ) -> std::result::Result<Value, Fault> {
        match method {
            "sessions.status" => {
                let _: Empty = params(value)?;
                let record = &self.sessions[scope].record;
                Ok(json!({"id":record.id,"agent_name":record.agent_name,
                    "name":record.name,"description":record.description,
                    "state":record.state,"docker_scope":record.docker_scope,"docker_enabled":record.docker_enabled}))
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
                        rows: 24,
                        cols: 80,
                        detached: p.detached,
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
            _ => Err((-32601, "method unavailable on sandbox endpoint".into())),
        }
    }
    fn decide(&mut self, p: Decision) -> std::result::Result<Value, Fault> {
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
        let request = self.permissions.iter().find(|r| r.id == p.request).unwrap();
        let inherited = if request.kind == "package" {
            self.inherits_package(&p.session, &request.package)
        } else {
            None
        };
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
                        kind: r.kind.clone(),
                        id: p.request.clone(),
                        package: r.package.clone(),
                        reason: r.reason.clone(),
                        scope: r.scope.clone(),
                        anonymous: r.anonymous,
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
        if !p.approved {
            self.reply_permission(&p.request, "denied", None);
        }
        self.dirty = true;
        Ok(json!({"accepted":true,"request":p.request,"approved":p.approved}))
    }
    fn approve_inherited_requests(&mut self) {
        let decisions: Vec<_> = self
            .permissions
            .iter()
            .filter(|r| r.state == "pending" && r.kind == "package")
            .filter(|r| {
                self.sessions.get(&r.session).is_some_and(|a| {
                    a.record.state == "running"
                        && a.pending.as_ref().is_some_and(|p| {
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
            let _ = self.decide(decision);
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
            "sessions.start" => self.start(params(value)?, None),
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
            "permissions.decide" => self.decide(params(value)?),
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
            let legacy_limit = if matches!(c.role, Role::Host) {
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
                    json!({"api":1,"instance":self.instance,"configuration":match &c.role { Role::Sandbox(id) => self.sessions.get(id).map(|a| a.record.name.as_str()), Role::Host => None },"role":if matches!(c.role,Role::Host){"host"}else{"sandbox"},"features":if matches!(c.role,Role::Host){vec!["docker-scopes","docker-enable","package-grants","same-daemon-reconnect","state-subscribe","raw-terminal","agent-names","server-control","terminal-reattach","agent-inbox-v1","agent-integration-v1","communications-log-v1"]}else{vec!["docker-scopes","docker-enable","package-grants","terminal-detach","subtree-control","subtree-terminal","sandbox-status","agent-inbox-v1","agent-integration-v1"]},"limits":{"header":256,"body":goblins_protocol::messages::MAX_REQUEST,"frame_seconds":3,"depth":32,"response_body":rpc::MAX_BODY,"calls":if matches!(c.role,Role::Host){4096}else{2},"connections":if matches!(c.role,Role::Host){HOSTS}else{8},"sessions":SESSIONS,"output_queue":QUEUE,"snapshot":900*1024,"terminal_buffer":65536}}),
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
                self.messaging
                    .commands
                    .try_send(crate::messaging::Work::Call {
                        connection: c.id,
                        id: id.clone(),
                        actor,
                        method: call.method,
                        params: call_params,
                        directory: self.message_directory(),
                        view,
                    })
                    .map_err(|_| capacity())?;
                c.waiting = Some(("mailbox".into(), id.clone()));
                return Ok(None);
            }
            match &c.role {
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
                    if p.kind == "docker" {
                        if !p.anonymous && p.scope.is_none() {
                            p.scope = a.record.docker_scope.clone();
                        }
                        if let Some(name) = &p.scope {
                            if !a
                                .inheritance
                                .as_ref()
                                .is_some_and(|i| i.docker_allowed(name))
                            {
                                return Err((
                                    -32602,
                                    "Docker scope is not allowed by this configuration".into(),
                                ));
                            }
                        }
                        p.package = p
                            .scope
                            .as_ref()
                            .map(|s| format!("Docker scope: {s}"))
                            .unwrap_or_else(|| "Docker engine".into());
                    }
                    if p.kind == "package" && a.record.packages.len() >= 64 {
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
                        || (p.kind == "docker"
                            && !p.anonymous
                            && self.sessions[&session].record.docker_enabled
                            && p.scope == self.sessions[&session].record.docker_scope);
                    let a = self.sessions.get_mut(&session).unwrap();
                    let w = a.worker.as_ref().ok_or_else(conflict)?;
                    let cancel = w.cancel.child();
                    if auto_approve || p.kind == "package" {
                        w.commands
                            .try_send(if auto_approve {
                                Work::Decide {
                                    request: Request {
                                        kind: p.kind.clone(),
                                        id: request.clone(),
                                        package: p.package.clone(),
                                        reason: p.reason.clone(),
                                        scope: p.scope.clone(),
                                        anonymous: p.anonymous,
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
                        id: request.clone(),
                        serial,
                        connection: c.id,
                        cancel,
                    });
                    self.permissions.push_back(PermissionRecord {
                        kind: p.kind.clone(),
                        scope: p.scope.clone(),
                        anonymous: p.anonymous,
                        id: request.clone(),
                        session,
                        agent_name: a.record.agent_name.clone(),
                        approval,
                        package: p.package,
                        reason: p.reason,
                        state: if auto_approve { "realizing" } else { "pending" }.into(),
                        approved: if auto_approve { Some(true) } else { None },
                        preview: if p.kind == "docker" { Some(json!({"description": if let Some(scope) = &p.scope { format!("Join Docker trust group '{scope}': members control the same containers, volumes, network, and combined filesystem grants. Named data persists after the last user leaves.") } else { "Use an anonymous rootless Docker scope, inherited by children; delete its data after the last attached sandbox exits.".into() }})) } else { None },
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
    pub fn tick(&mut self) -> Result<()> {
        // Drain human input and native output before checking wake revisions.
        for a in self.sessions.values_mut() {
            if let Err(e) = a.terminal.tick() {
                a.record.detail = Some(bounded(&e.to_string()));
                a.terminal.interrupted = true;
            }
        }
        for mut response in self.messaging.results.try_iter() {
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
                        docker_scope,
                        initial_packages,
                        master,
                        listener,
                        identity,
                        inheritance,
                        exit_watch,
                    } => {
                        a.record.docker_enabled = docker_enabled;
                        a.record.docker_scope = docker_scope;
                        a.record.description = inheritance.description().to_owned();
                        a.exit_watch = Some(exit_watch);
                        a.inheritance = Some(inheritance);
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
                        docker_scope,
                    } => {
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
                                if reply.status == "ready" && r.kind == "docker" {
                                    a.record.docker_enabled = true;
                                    a.record.docker_scope = docker_scope;
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
                a.inheritance.take();
                a.exit_watch.take();
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
            // Observe process exit independently of a worker blocked on Nix.
            // A pidfd pins the process identity, avoiding PID-reuse races.
            let exited = a.record.state == "running"
                && a.exit_watch
                    .as_ref()
                    .is_some_and(|fd| unix::readable(fd.as_raw_fd(), 0).unwrap_or(true));
            let stopped = ["stopped", "failed"].contains(&a.record.state.as_str());
            self.sessions.insert(id.clone(), a);
            if exited {
                let _ = self.stop_tree(&id, true, None);
            } else if stopped {
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
        self.approve_inherited_requests();
        let directory = self.message_directory();
        if directory != self.messaging_directory
            && self
                .messaging
                .commands
                .try_send(crate::messaging::Work::Sync(directory.clone()))
                .is_ok()
        {
            self.messaging_directory = directory;
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
                inheritance: None,
                exit_watch: None,
                granted_outputs: BTreeMap::new(),
                created: 0,
                record: SessionRecord {
                    docker_enabled: false,
                    docker_scope: None,
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
                },
                worker: Some(Worker::completed(results)),
                listener: None,
                pending: Some(Pending {
                    initial_output: None,
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
            kind: "package".into(),
            scope: None,
            anonymous: false,
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
                docker_scope: None,
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
                    docker_scope: None,
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
}
