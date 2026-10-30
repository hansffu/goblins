//! Codex App Server adapter. Runs entirely inside the reporting sandbox: it
//! owns the goblin's conversation on its own App Server connection, follows
//! the attached terminal's conversation switches, and delivers inbox reminders.
use clap::{Parser, Subcommand};
use goblins_codex::{Delivery, activity};
use goblins_protocol::{AgentState, rpc};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, BTreeSet, VecDeque},
    fs::{self, File, OpenOptions},
    io::{self, Write},
    os::{
        fd::AsRawFd,
        unix::{
            fs::{OpenOptionsExt, PermissionsExt},
            net::UnixStream,
        },
    },
    path::{Path, PathBuf},
    sync::mpsc::{self, Receiver, Sender},
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};
use tungstenite::{Message, WebSocket, protocol::WebSocketConfig};

/// Match Codex's remote client receive limits. The handshake's advertised
/// 16 MiB cap limits client uploads; server model/configuration responses can
/// be larger. Tungstenite's default 16 MiB frame / 64 MiB message limits would
/// disconnect before it receives those responses.
fn websocket_config() -> WebSocketConfig {
    WebSocketConfig::default()
        .max_frame_size(Some(128 << 20))
        .max_message_size(Some(128 << 20))
        .max_write_buffer_size((128 << 20) + (128 << 10))
}

type Result<T> = std::result::Result<T, Box<dyn std::error::Error>>;
#[derive(Parser)]
struct Cli {
    #[arg(long, global = true)]
    runtime_dir: Option<PathBuf>,
    /// The daemon's sandbox endpoint (tests only)
    #[arg(long, global = true, hide = true)]
    request_socket: Option<PathBuf>,
    #[command(subcommand)]
    command: Command,
}
#[derive(Subcommand)]
enum Command {
    Run {
        #[arg(long)]
        socket: PathBuf,
        /// Load this recorded conversation instead of starting one
        #[arg(long)]
        thread: Option<String>,
        /// The goblin bypasses Codex's approvals (bypassPermissions)
        #[arg(long)]
        bypass_permissions: bool,
    },
    /// Deliver reminders to this conversation until the terminal's next message
    Bind {
        thread: String,
    },
    Status,
    Pause,
    Resume,
    /// Internal launcher helper: preserve native profile configuration on the server.
    #[command(hide = true)]
    ProfileArgs {
        name: String,
    },
}
#[derive(Default, Serialize, Deserialize)]
struct Controls {
    paused: bool,
    resume: String,
}
#[derive(Default, Serialize, Deserialize)]
struct Ledger {
    threads: BTreeMap<String, Delivery>,
    resume: String,
}
fn read<T: for<'a> Deserialize<'a>>(path: &Path) -> Result<T> {
    Ok(serde_json::from_slice(&fs::read(path)?)?)
}
fn atomic(path: &Path, value: &impl Serialize) -> Result<()> {
    let temp = path.with_extension(format!(
        "tmp-{}",
        goblins_protocol::messages::operation_key(None)?
    ));
    let result = (|| -> Result<()> {
        let mut f = OpenOptions::new()
            .create_new(true)
            .write(true)
            .mode(0o600)
            .open(&temp)?;
        serde_json::to_writer(&mut f, value)?;
        f.write_all(b"\n")?;
        f.sync_all()?;
        fs::rename(&temp, path)?;
        File::open(path.parent().ok_or("runtime directory missing")?)?.sync_all()?;
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temp);
    }
    result
}
fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .min(u64::MAX as u128) as u64
}
static REQUEST_SOCKET: std::sync::OnceLock<PathBuf> = std::sync::OnceLock::new();
fn daemon(method: &str, value: Value) -> Result<Value> {
    let socket = REQUEST_SOCKET.get_or_init(|| goblins_protocol::REQUEST_SOCKET.into());
    let mut s = UnixStream::connect(socket)?;
    s.set_read_timeout(Some(Duration::from_secs(3)))?;
    s.set_write_timeout(Some(Duration::from_secs(3)))?;
    rpc::exchange(&mut s, json!(0), "initialize", json!({"api":1}))?;
    Ok(rpc::exchange(&mut s, json!(1), method, value)?)
}
/// A separate reporter keeps ten-second freshness heartbeats independent of
/// provider RPC delays and reports every observed transition, even short turns.
struct Reporter {
    transitions: Sender<AgentState>,
    errors: Receiver<Option<String>>,
}
impl Reporter {
    fn start() -> Self {
        let (transitions, states) = mpsc::channel();
        let (diagnostics, errors) = mpsc::channel();
        std::thread::spawn(move || {
            let mut state = AgentState::Unknown;
            let mut reported = None;
            let mut at = Instant::now() - Duration::from_secs(10);
            loop {
                let wait = Duration::from_secs(10).saturating_sub(at.elapsed());
                match states.recv_timeout(wait) {
                    Ok(next) => state = next,
                    Err(mpsc::RecvTimeoutError::Disconnected) => break,
                    Err(mpsc::RecvTimeoutError::Timeout) => (),
                }
                if reported != Some(state) || at.elapsed() >= Duration::from_secs(10) {
                    let error = daemon("sessions.set-state", json!({"state":state}))
                        .err()
                        .map(|e| format!("activity report failed: {e}"));
                    reported = if error.is_none() { Some(state) } else { None };
                    let _ = diagnostics.send(error);
                    at = Instant::now();
                }
            }
        });
        Self {
            transitions,
            errors,
        }
    }
}
/// The foreground thread's id and name, for `goblins recreate`. They are
/// reported together, off the proxy loop, and a failed report is retried with
/// a bounded backoff, always with the latest values: an obsolete retry cannot
/// overwrite a newer thread.
fn conversation_reporter() -> Sender<(String, String)> {
    let (desired, updates) = mpsc::channel::<(String, String)>();
    std::thread::spawn(move || {
        let first = Duration::from_millis(500);
        let mut backoff = first;
        let mut wanted: Option<(String, String)> = None;
        let mut acknowledged = None;
        loop {
            let next = if wanted.is_some() && wanted != acknowledged {
                match updates.recv_timeout(backoff) {
                    Ok(next) => Some(next),
                    Err(mpsc::RecvTimeoutError::Timeout) => None,
                    Err(mpsc::RecvTimeoutError::Disconnected) => break,
                }
            } else {
                match updates.recv() {
                    Ok(next) => Some(next),
                    Err(_) => break,
                }
            };
            if let Some(next) = next {
                wanted = Some(updates.try_iter().last().unwrap_or(next));
                backoff = first;
            }
            let Some((id, name)) = wanted.clone() else {
                continue;
            };
            if acknowledged.as_ref() == wanted.as_ref() {
                continue;
            }
            match daemon(
                "sessions.set-variables",
                json!({"variables":{"session_id":id,"session_name":variable_text(&name)}}),
            ) {
                Ok(_) => acknowledged = wanted.clone(),
                Err(e) => {
                    eprintln!("conversation report failed: {e}");
                    backoff = (backoff * 2).min(Duration::from_secs(30));
                }
            }
        }
    });
    desired
}
/// One line of text within the daemon's 256-byte variable value limit.
fn variable_text(text: &str) -> String {
    let mut value = String::new();
    for c in text.chars() {
        let c = if c.is_control() { ' ' } else { c };
        if value.len() + c.len_utf8() > 256 {
            break;
        }
        value.push(c);
    }
    value
}
#[derive(Debug)]
struct Rejected(String);
impl std::fmt::Display for Rejected {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}
impl std::error::Error for Rejected {}
struct Server {
    ws: WebSocket<UnixStream>,
    serial: u64,
    thread: Option<String>,
    status: Value,
    subscribed: bool,
    transitions: Sender<AgentState>,
    /// Notifications the adapter follows conversations by, oldest first.
    events: VecDeque<Value>,
}
impl Server {
    fn connect(path: &Path, transitions: Sender<AgentState>) -> Result<Self> {
        let stream = UnixStream::connect(path)?;
        stream.set_read_timeout(Some(Duration::from_secs(3)))?;
        stream.set_write_timeout(Some(Duration::from_secs(3)))?;
        // Codex's explicit Unix transport is RFC6455, unlike its NDJSON stdio.
        let (ws, _) = tungstenite::client::client_with_config(
            "ws://localhost/",
            stream,
            Some(websocket_config()),
        )?;
        ws.get_ref()
            .set_read_timeout(Some(Duration::from_millis(50)))?;
        let mut server = Self {
            ws,
            serial: 0,
            thread: None,
            status: Value::Null,
            subscribed: false,
            transitions,
            events: VecDeque::new(),
        };
        server.call("initialize",json!({"clientInfo":{"name":"goblins-codex","version":env!("CARGO_PKG_VERSION")},"capabilities":{"experimentalApi":true}}))?;
        server
            .ws
            .send(Message::text(json!({"method":"initialized"}).to_string()))?;
        Ok(server)
    }
    fn receive(&mut self) -> Result<Option<Value>> {
        match self.ws.read() {
            Ok(Message::Text(t)) => Ok(Some(serde_json::from_str(&t)?)),
            Ok(Message::Close(_)) => Err("App Server disconnected".into()),
            Ok(_) => Ok(None),
            Err(tungstenite::Error::Io(e))
                if matches!(
                    e.kind(),
                    io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut
                ) =>
            {
                Ok(None)
            }
            Err(e) => Err(e.into()),
        }
    }
    fn event(&mut self, v: &Value) {
        // Never reply to server requests: the native terminal owns approvals/input.
        if v.get("id").is_some() {
            return;
        }
        if matches!(
            v["method"].as_str(),
            Some(
                "thread/started" | "thread/status/changed" | "item/started" | "thread/name/updated"
            )
        ) && self.events.len() < 4096
        {
            self.events.push_back(v.clone());
        }
        if v["params"]["threadId"].as_str() != self.thread.as_deref() {
            return;
        }
        if v["method"] == "thread/status/changed" {
            self.set_status(v["params"]["status"].clone());
        }
    }
    fn set_status(&mut self, status: Value) {
        if activity(&self.status) != activity(&status) {
            let _ = self.transitions.send(activity(&status));
        }
        self.status = status;
    }
    fn call(&mut self, method: &str, params: Value) -> Result<Value> {
        self.serial += 1;
        let id = self.serial;
        self.ws.send(Message::text(
            json!({"id":id,"method":method,"params":params}).to_string(),
        ))?;
        // Starting or loading a thread waits for Codex's model refresh, which
        // takes about five seconds without network access.
        let wait = if matches!(method, "thread/start" | "thread/resume") {
            30
        } else {
            3
        };
        let deadline = Instant::now() + Duration::from_secs(wait);
        while Instant::now() < deadline {
            if let Some(v) = self.receive()? {
                // Server requests and client responses have separate ID namespaces.
                if v.get("method").is_none() && v["id"] == id {
                    if let Some(e) = v.get("error") {
                        return Err(Box::new(Rejected(e.to_string())));
                    }
                    return v
                        .get("result")
                        .cloned()
                        .ok_or_else(|| "missing RPC result".into());
                }
                self.event(&v);
            }
        }
        Err("App Server request timed out; acceptance may be uncertain".into())
    }
    fn bind(&mut self, thread: &str) -> Result<()> {
        // The previous thread stays subscribed: the terminal may return to it.
        if self.thread.as_deref() != Some(thread) {
            self.thread = Some(thread.to_owned());
            self.subscribed = false;
            self.set_status(Value::Null);
        }
        // A metadata read must prove this is loaded before resume; never resurrect
        // a historical task or change its configuration to obtain a subscription.
        let view = self.call(
            "thread/read",
            json!({"threadId":thread,"includeTurns":false}),
        )?;
        self.set_status(view["thread"]["status"].clone());
        if matches!(activity(&self.status), AgentState::Unknown) {
            return Err("bound thread unavailable".into());
        }
        if !self.subscribed {
            match self.call(
                "thread/resume",
                json!({"threadId":thread,"excludeTurns":true}),
            ) {
                Ok(v) => {
                    self.subscribed = true;
                    self.set_status(v["thread"]["status"].clone());
                }
                // New threads have no rollout until their first turn. Global
                // status events and metadata reads still report their activity.
                Err(e)
                    if e.downcast_ref::<Rejected>()
                        .is_some_and(|e| e.0.contains("no rollout found")) => {}
                Err(e) => return Err(e),
            }
        }
        Ok(())
    }
    fn queue(&mut self, thread: &str) -> Result<Vec<Value>> {
        let mut queue = vec![];
        let mut cursor = Value::Null;
        for _ in 0..32 {
            let v = self.call(
                "thread/queue/list",
                json!({"threadId":thread,"limit":100,"cursor":cursor}),
            )?;
            queue.extend(
                v["data"]
                    .as_array()
                    .ok_or("invalid queue response")?
                    .iter()
                    .cloned(),
            );
            cursor = v["nextCursor"].clone();
            if cursor.is_null() {
                return Ok(queue);
            }
        }
        Err("queue pagination limit exceeded".into())
    }
    fn reconcile(&mut self, thread: &str, d: &mut Delivery) -> Result<()> {
        if d.pending.is_none() {
            return Ok(());
        }
        let queue = self.queue(thread)?;
        let mut history = vec![];
        let mut cursor = Value::Null;
        let id = d.pending.as_ref().unwrap().client_id.clone();
        if !queue.iter().any(|q| q["clientUserMessageId"] == id)
            && !d.pending.as_ref().unwrap().seen_in_history
        {
            // Bounded recent history: absent evidence suspends submissions, never
            // treats a missing acknowledgement as a rejected submission.
            for _ in 0..8 {
                let v = self.call(
                    "thread/items/list",
                    json!({"threadId":thread,"sortDirection":"desc","limit":100,"cursor":cursor}),
                )?;
                let found = goblins_codex::has_client_id(&v, &id);
                cursor = v["nextCursor"].clone();
                history.push(v);
                if found || cursor.is_null() {
                    break;
                }
            }
        }
        d.reconcile(
            &queue,
            &json!(history),
            activity(&self.status) == AgentState::Idle,
        );
        if let Some(p) = &d.pending
            && let Some(queue_id) = &p.queue_id
            && queue.iter().any(|q| q["id"] == *queue_id)
            && activity(&self.status) == AgentState::Idle
        {
            match self.call(
                "thread/queue/start",
                json!({"threadId":thread,"queuedSubmissionId":queue_id}),
            ) {
                Ok(_) => self.set_status(json!({"type":"active","activeFlags":[]})),
                // Races with native user submissions and automatic queue start
                // are reconciled next poll without resubmitting the reminder.
                Err(e) if e.downcast_ref::<Rejected>().is_some() => (),
                Err(e) => return Err(e),
            }
        }
        Ok(())
    }
}
/// A thread the person works in: not ephemeral, not a subagent's, not a
/// background task's.
fn user_thread(thread: &Value) -> bool {
    !(thread["ephemeral"] == true
        || !thread["parentThreadId"].is_null()
        || thread["source"]
            .as_object()
            .is_some_and(|s| s.keys().any(|k| k.eq_ignore_ascii_case("subagent")))
        || thread["threadSource"]
            .as_str()
            .is_some_and(|source| source != "user"))
}

/// Which conversation the attached terminal works in: the thread of the
/// newest user message that started during the current attachment and is
/// not one of the adapter's own reminders.
#[derive(Default)]
struct Follow {
    /// When the current attachment's client started.
    since: Option<u64>,
    /// The newest qualifying message: when it started, and its thread.
    winner: Option<(u64, String)>,
    /// Item IDs already considered, per thread, newest last.
    seen: BTreeMap<String, VecDeque<String>>,
}
impl Follow {
    /// A new attachment starts afresh: true when it changed. Its threads are
    /// caught up again, since messages seen before it was known still count.
    fn attach(&mut self, since: Option<u64>) -> bool {
        if since == self.since {
            return false;
        }
        self.since = since;
        self.winner = None;
        self.seen.clear();
        true
    }
    /// A user message item in `thread`, live or caught up, considered once.
    fn message(
        &mut self,
        thread: &str,
        item: &Value,
        started: Option<u64>,
        reminded: impl Fn(&str) -> bool,
    ) {
        let Some(id) = item["id"].as_str() else {
            return;
        };
        // Judged only against a known attachment; the catch-up once it is
        // known sees it again.
        let Some(since) = self.since else {
            return;
        };
        let seen = self.seen.entry(thread.to_owned()).or_default();
        if seen.iter().any(|s| s == id) {
            return;
        }
        seen.push_back(id.to_owned());
        if seen.len() > 256 {
            seen.pop_front();
        }
        let Some(started) = started else {
            return;
        };
        if started < since || item["clientId"].as_str().is_some_and(reminded) {
            return;
        }
        if self.winner.as_ref().is_none_or(|(at, _)| started > *at) {
            self.winner = Some((started, thread.to_owned()));
        }
    }
}

/// The goblin's approval policy for a thread in `cwd`: "never" when it
/// bypasses approvals, else the one configured there, else Codex's own
/// default, on-request. The sandbox is always full access: Goblins is the
/// sandbox, and Codex's own cannot run inside a goblin.
fn approval(server: &mut Server, bypass: bool, cwd: Option<&Value>) -> Result<Value> {
    if bypass {
        return Ok(json!("never"));
    }
    let params = match cwd {
        Some(cwd) if cwd.is_string() => json!({"cwd":cwd}),
        _ => json!({}),
    };
    let read = server.call("config/read", params)?;
    let policy = &read.get("config").ok_or("config/read returned no config")?["approval_policy"];
    Ok(if policy.is_null() {
        json!("on-request")
    } else {
        policy.clone()
    })
}
fn permitted(result: &Value, approval: &Value) -> bool {
    result["approvalPolicy"] == *approval && result["sandbox"]["type"] == "dangerFullAccess"
}
/// Apply the goblin's policy to a loaded thread, then read it back.
fn enforce(server: &mut Server, thread: &str, approval: &Value) -> Result<Value> {
    server.call(
        "thread/settings/update",
        json!({"threadId":thread,"approvalPolicy":approval,"sandboxPolicy":{"type":"dangerFullAccess"}}),
    )?;
    let verified = server.call(
        "thread/resume",
        json!({"threadId":thread,"excludeTurns":true}),
    )?;
    if !permitted(&verified, approval) {
        return Err("Codex did not apply this goblin's approval and sandbox policy".into());
    }
    Ok(verified)
}
/// Create the goblin's conversation, or load the recorded one, with its
/// policy. A new one is named at once, which saves it, so the terminal can
/// resume it before its first turn. Returns the thread.
fn open(server: &mut Server, thread: Option<&str>, bypass: bool, name: &str) -> Result<Value> {
    let (mut result, approval) = match thread {
        Some(thread) => {
            let cwd = server.call(
                "thread/read",
                json!({"threadId":thread,"includeTurns":false}),
            )?["thread"]["cwd"]
                .clone();
            let approval = approval(server, bypass, Some(&cwd))?;
            let result = server.call(
                "thread/resume",
                json!({"threadId":thread,"excludeTurns":true,"approvalPolicy":approval,"sandbox":"danger-full-access"}),
            )?;
            (result, approval)
        }
        None => {
            let approval = approval(server, bypass, None)?;
            let mut result = server.call(
                "thread/start",
                json!({"approvalPolicy":approval,"sandbox":"danger-full-access"}),
            )?;
            let id = result["thread"]["id"]
                .as_str()
                .ok_or("thread response has no id")?
                .to_owned();
            server.call("thread/name/set", json!({"threadId":id,"name":name}))?;
            result["thread"]["name"] = json!(name);
            (result, approval)
        }
    };
    let id = result["thread"]["id"]
        .as_str()
        .ok_or("thread response has no id")?
        .to_owned();
    // A loaded thread can keep its own policy despite the request's.
    if !permitted(&result, &approval) {
        let verified = enforce(server, &id, &approval)?;
        result["thread"]["status"] = verified["thread"]["status"].clone();
    }
    Ok(result["thread"].clone())
}
/// Join a thread without changing it. None when it is not saved yet: a new
/// thread has no rollout until its first turn.
fn join(server: &mut Server, thread: &str) -> Result<Option<Value>> {
    match server.call(
        "thread/resume",
        json!({"threadId":thread,"excludeTurns":true}),
    ) {
        Ok(v) => Ok(Some(v["thread"].clone())),
        Err(e)
            if e.downcast_ref::<Rejected>()
                .is_some_and(|e| e.0.contains("no rollout found")) =>
        {
            Ok(None)
        }
        Err(e) => Err(e),
    }
}
/// Every thread the server has loaded.
fn loaded(server: &mut Server) -> Result<Vec<String>> {
    let mut threads = vec![];
    let mut cursor = Value::Null;
    for _ in 0..32 {
        let v = server.call("thread/loaded/list", json!({"cursor":cursor}))?;
        threads.extend(
            v["data"]
                .as_array()
                .ok_or("invalid loaded thread list")?
                .iter()
                .filter_map(|t| t.as_str().map(str::to_owned)),
        );
        cursor = v["nextCursor"].clone();
        if cursor.is_null() {
            return Ok(threads);
        }
    }
    Err("loaded thread pagination limit exceeded".into())
}
/// Consider a joined thread's user messages from the current attachment
/// that arrived before it could be joined, or while disconnected.
fn catch_up(server: &mut Server, follow: &mut Follow, ledger: &Ledger, thread: &str) -> Result<()> {
    let Some(since) = follow.since else {
        return Ok(());
    };
    let reminded = |id: &str| ledger.threads.get(thread).is_some_and(|d| d.reminded(id));
    let mut cursor = Value::Null;
    for _ in 0..8 {
        let v = server.call(
            "thread/items/list",
            json!({"threadId":thread,"sortDirection":"desc","limit":100,"cursor":cursor}),
        )?;
        for entry in v["data"].as_array().ok_or("invalid item list")? {
            let started = entry["startedAtMs"].as_u64();
            if started.is_some_and(|at| at < since) {
                return Ok(());
            }
            if entry["item"]["type"] == "userMessage" {
                follow.message(thread, &entry["item"], started, reminded);
            }
        }
        cursor = v["nextCursor"].clone();
        if cursor.is_null() {
            break;
        }
    }
    Ok(())
}

fn run(dir: &Path, socket: &Path, thread: Option<&str>, bypass: bool) -> Result<()> {
    let lock = OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .mode(0o600)
        .open(dir.join("run.lock"))?;
    if unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
        return Err("adapter already running".into());
    }
    let mut ledger: Ledger = match read(&dir.join("ledger.json")) {
        Ok(v) => v,
        Err(e)
            if e.downcast_ref::<io::Error>()
                .is_some_and(|e| e.kind() == io::ErrorKind::NotFound) =>
        {
            Ledger::default()
        }
        Err(e) => return Err(e),
    };
    let reporter = Reporter::start();
    let conversation = conversation_reporter();
    let name = daemon("sessions.status", json!({}))
        .ok()
        .and_then(|s| s["agent_name"].as_str().map(str::to_owned))
        .unwrap_or_else(|| "goblin".into());
    // A conversation that cannot be created or loaded stops the goblin.
    let mut s = Server::connect(socket, reporter.transitions.clone())?;
    let opened = open(&mut s, thread, bypass, &name)?;
    let mut bound = opened["id"].as_str().ok_or("thread has no id")?.to_owned();
    let _ = conversation.send((
        bound.clone(),
        opened["name"].as_str().unwrap_or_default().to_owned(),
    ));
    let mut server = Some(s);
    // Joined threads and their directories; threads that are not the person's.
    let mut joined: BTreeMap<String, Value> = BTreeMap::new();
    let mut ignored: BTreeSet<String> = BTreeSet::new();
    let mut follow = Follow::default();
    let mut report = AgentState::Unknown;
    let mut report_error = None;
    loop {
        let tick = Instant::now();
        let mut diagnostic = None;
        let mut paused = false;
        let result = (|| -> Result<()> {
            let control: Controls = match read(&dir.join("controls.json")) {
                Ok(v) => v,
                Err(e)
                    if e.downcast_ref::<io::Error>()
                        .is_some_and(|e| e.kind() == io::ErrorKind::NotFound) =>
                {
                    Controls::default()
                }
                Err(e) => return Err(e),
            };
            paused = control.paused;
            if control.resume != ledger.resume {
                for d in ledger.threads.values_mut() {
                    d.resume();
                }
                ledger.resume = control.resume;
            }
            if server.is_none() {
                server = Some(Server::connect(socket, reporter.transitions.clone())?);
                // Rejoin and catch up on everything after a reconnect.
                joined.clear();
            }
            let s = server.as_mut().unwrap();
            // Follow the terminal only while a client is attached.
            // An unanswered status keeps the attachment last known.
            if let Ok(status) = daemon("sessions.status", json!({}))
                && follow.attach(
                    status["attached_since"]
                        .as_u64()
                        .filter(|_| status["attached"] == true),
                )
            {
                for thread in joined.keys().cloned().collect::<Vec<_>>() {
                    catch_up(s, &mut follow, &ledger, &thread)?;
                }
            }
            let mut found: BTreeSet<String> = loaded(s)?.into_iter().collect();
            for v in std::mem::take(&mut s.events) {
                let params = &v["params"];
                match v["method"].as_str() {
                    Some("thread/started") if user_thread(&params["thread"]) => {
                        if let Some(id) = params["thread"]["id"].as_str() {
                            found.insert(id.to_owned());
                        }
                    }
                    Some("thread/status/changed") => {
                        if let Some(id) = params["threadId"].as_str() {
                            found.insert(id.to_owned());
                        }
                    }
                    // Only the person's joined threads; one not joined yet is
                    // caught up once it is, and others never count.
                    Some("item/started")
                        if params["item"]["type"] == "userMessage"
                            && params["threadId"]
                                .as_str()
                                .is_some_and(|t| joined.contains_key(t)) =>
                    {
                        let thread = params["threadId"].as_str().unwrap_or_default();
                        let reminded =
                            |id: &str| ledger.threads.get(thread).is_some_and(|d| d.reminded(id));
                        follow.message(
                            thread,
                            &params["item"],
                            params["startedAtMs"].as_u64(),
                            reminded,
                        );
                    }
                    Some("thread/name/updated") if params["threadId"] == bound.as_str() => {
                        let name = params["threadName"].as_str().unwrap_or_default();
                        let _ = conversation.send((bound.clone(), name.to_owned()));
                    }
                    _ => (),
                }
            }
            // The adapter's own binding request.
            let requested = match read::<String>(&dir.join("bind.json")) {
                Ok(thread) => Some(thread),
                Err(e)
                    if e.downcast_ref::<io::Error>()
                        .is_some_and(|e| e.kind() == io::ErrorKind::NotFound) =>
                {
                    None
                }
                Err(e) => return Err(e),
            };
            if let Some(thread) = &requested {
                found.insert(thread.clone());
            }
            for thread in found {
                if joined.contains_key(&thread) || ignored.contains(&thread) {
                    continue;
                }
                match join(s, &thread)? {
                    Some(view) if user_thread(&view) => {
                        joined.insert(thread.clone(), view["cwd"].clone());
                        catch_up(s, &mut follow, &ledger, &thread)?;
                    }
                    Some(_) => {
                        let _ = s.call("thread/unsubscribe", json!({"threadId":thread}));
                        ignored.insert(thread);
                    }
                    None => (),
                }
            }
            if let Some(thread) = requested {
                fs::remove_file(dir.join("bind.json"))?;
                if !joined.contains_key(&thread) {
                    return Err(
                        format!("cannot bind {thread}: it is not a loaded conversation").into(),
                    );
                }
                follow.winner = Some((now_ms(), thread));
            }
            if let Some((_, thread)) = follow.winner.clone()
                && thread != bound
                && joined.contains_key(&thread)
            {
                let approval = approval(s, bypass, joined.get(&thread))?;
                let verified = enforce(s, &thread, &approval)?;
                bound = thread;
                let name = verified["thread"]["name"].as_str().unwrap_or_default();
                let _ = conversation.send((bound.clone(), name.to_owned()));
            }
            s.bind(&bound)?;
            report = activity(&s.status);
            let inbox = daemon("inbox.status", json!({}))?;
            if inbox["audit_failed"] != false {
                return Err("inbox audit unavailable; delivery suspended".into());
            }
            let d = ledger.threads.entry(bound.clone()).or_default();
            d.progress(&inbox["head"]);
            s.reconcile(&bound, d)?;
            if d.eligible(now_ms(), paused, &inbox) {
                let id = goblins_protocol::messages::operation_key(None)?;
                d.begin(id.clone(), now_ms());
                atomic(&dir.join("ledger.json"), &ledger)?; // write ahead of send
                let d = ledger.threads.get_mut(&bound).unwrap();
                match s.call("thread/queue/add",json!({"threadId":bound,"clientUserMessageId":id,"input":[{"type":"text","text":goblins_protocol::INTEGRATION_PROMPT}]})) {
                    Ok(v) => {
                        d.pending.as_mut().unwrap().queue_id=v["queuedSubmission"]["id"].as_str().map(str::to_owned);
                        if d.pending.as_ref().unwrap().queue_id.is_none() { d.uncertain=true; }
                    }
                    Err(e) if e.downcast_ref::<Rejected>().is_some() => { d.pending=None;return Err(e); }
                    Err(e) => { d.uncertain=true;return Err(e); }
                }
            }
            let d = ledger.threads.get(&bound).unwrap();
            if d.uncertain {
                diagnostic = Some(
                    "submission ownership uncertain; automatic submissions suspended".to_string(),
                );
            } else if d.attempts >= 3 && d.pending.is_none() {
                diagnostic=Some("no inbox progress after three reminders; resume locally to reset the retry limit".to_string());
            }
            // Drain a bounded set of observer events between polling cycles.
            for _ in 0..64 {
                if let Some(v) = s.receive()? {
                    s.event(&v);
                } else {
                    break;
                }
            }
            report = activity(&s.status);
            Ok(())
        })();
        if let Err(e) = result {
            diagnostic = Some(e.to_string());
            report = AgentState::Unknown;
            server = None;
        }
        atomic(&dir.join("ledger.json"), &ledger)?;
        for error in reporter.errors.try_iter() {
            report_error = error;
        }
        if report_error.is_some() {
            diagnostic = report_error.clone();
        }
        let _ = reporter.transitions.send(report);
        atomic(
            &dir.join("status.json"),
            &json!({"thread_id":bound,"agent_state":report,"paused":paused,"diagnostic":diagnostic,"updated_at_ms":now_ms(),"threads":ledger.threads}),
        )?;
        std::thread::sleep(Duration::from_millis(500).saturating_sub(tick.elapsed()));
    }
}
fn main() {
    let cli = Cli::parse();
    if let Some(socket) = &cli.request_socket {
        let _ = REQUEST_SOCKET.set(socket.clone());
    }
    let dir = cli
        .runtime_dir
        .or_else(|| std::env::var_os("GOBLINS_CODEX_RUNTIME").map(PathBuf::from))
        .unwrap_or_else(|| PathBuf::from("/tmp/goblins-codex"));
    let result = (|| -> Result<()> {
        if let Command::ProfileArgs { name } = &cli.command {
            if name.is_empty()
                || !name
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b"_-".contains(&b))
            {
                return Err("invalid native profile name".into());
            }
            let home = fs::canonicalize(PathBuf::from(
                std::env::var_os("CODEX_HOME").ok_or("CODEX_HOME missing")?,
            ))?;
            let path = home.join(format!("{name}.config.toml"));
            let profile = fs::read_to_string(&path)?;
            let base = match fs::read_to_string(home.join("config.toml")) {
                Ok(input) => toml::from_str(&input)?,
                Err(e) if e.kind() == io::ErrorKind::NotFound => toml::Table::new(),
                Err(e) => return Err(e.into()),
            };
            for argument in profile_arguments(&profile, &path, &base)? {
                io::stdout().write_all(argument.as_bytes())?;
                io::stdout().write_all(b"\0")?;
            }
            return Ok(());
        }
        let meta = fs::symlink_metadata(&dir)?;
        if !meta.is_dir()
            || meta.permissions().mode() & 0o777 != 0o700
            || std::os::unix::fs::MetadataExt::uid(&meta) != unsafe { libc::geteuid() }
        {
            return Err("runtime must be a private directory owned by this user".into());
        }
        match cli.command {
            Command::Run {
                socket,
                thread,
                bypass_permissions,
            } => run(&dir, &socket, thread.as_deref(), bypass_permissions),
            Command::Bind { thread } => {
                if thread.is_empty()
                    || thread.len() > 128
                    || !thread
                        .bytes()
                        .all(|b| b.is_ascii_alphanumeric() || b == b'-')
                {
                    return Err("invalid thread ID".into());
                }
                atomic(&dir.join("bind.json"), &thread)?;
                println!("{}", json!({"bind":thread}));
                Ok(())
            }
            Command::Status => {
                println!("{}", read::<Value>(&dir.join("status.json"))?);
                Ok(())
            }
            Command::Pause | Command::Resume => {
                let paused = matches!(cli.command, Command::Pause);
                let control = Controls {
                    paused,
                    resume: if paused {
                        read::<Controls>(&dir.join("controls.json"))
                            .unwrap_or_default()
                            .resume
                    } else {
                        goblins_protocol::messages::operation_key(None)?
                    },
                };
                atomic(&dir.join("controls.json"), &control)?;
                println!("{}", json!({"paused":paused}));
                Ok(())
            }
            Command::ProfileArgs { .. } => unreachable!(),
        }
    })();
    if let Err(e) = result {
        eprintln!("goblins-codex: {e}");
        std::process::exit(1);
    }
}

fn profile_arguments(input: &str, path: &Path, base: &toml::Table) -> Result<Vec<String>> {
    let mut profile: toml::Table = toml::from_str(input)?;
    // Codex keys hook preferences by declaration file. Moving a
    // profile into runtime overrides changes that file to this native synthetic
    // path. Preserve existing enablement/trusted hashes for the same handlers;
    // Codex still verifies the hash, so edited commands never inherit trust.
    let prefix = format!("{}:", path.display());
    let mut aliases = toml::Table::new();
    for config in [base, &profile] {
        if let Some(states) = config
            .get("hooks")
            .and_then(|h| h.get("state"))
            .and_then(toml::Value::as_table)
        {
            for (key, value) in states {
                if let Some(suffix) = key.strip_prefix(&prefix) {
                    let key = format!("/<session-flags>/config.toml:{suffix}");
                    if let Some(existing) =
                        aliases.get_mut(&key).and_then(toml::Value::as_table_mut)
                        && let Some(fields) = value.as_table()
                    {
                        existing.extend(fields.clone());
                    } else {
                        aliases.insert(key, value.clone());
                    }
                }
            }
        }
    }
    if !aliases.is_empty() {
        let hooks = profile
            .entry("hooks")
            .or_insert_with(|| toml::Value::Table(toml::Table::new()))
            .as_table_mut()
            .ok_or("invalid profile hooks")?;
        hooks
            .entry("state")
            .or_insert_with(|| toml::Value::Table(toml::Table::new()))
            .as_table_mut()
            .ok_or("invalid profile hook state")?
            .extend(aliases);
    }
    Ok(profile
        .into_iter()
        .map(|(key, value)| format!("{key}={value}"))
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::net::UnixListener;

    #[test]
    fn the_newest_message_of_this_attachment_wins_unless_it_is_a_reminder() {
        let item = |id: &str, client: Option<&str>| json!({"type":"userMessage","id":id,"clientId":client});
        let none = |_: &str| false;
        let mut f = Follow::default();
        // Nothing qualifies while detached, and nothing is judged before the
        // attachment is known: a message seen then counts once it is.
        f.message("a", &item("1", None), Some(300), none);
        assert!(f.winner.is_none());
        assert!(f.attach(Some(200)));
        assert!(!f.attach(Some(200)));
        f.message("a", &item("1", None), Some(300), none);
        assert_eq!(f.winner, Some((300, "a".into())));
        // Items are considered once per attachment.
        assert!(f.attach(Some(250)));
        f.message("a", &item("2", None), Some(260), none);
        assert_eq!(f.winner, Some((260, "a".into())));
        f.message("a", &item("2", None), Some(500), none);
        assert_eq!(f.winner, Some((260, "a".into())));
        // Old history and messages from before the attachment never count.
        assert!(f.attach(Some(1000)));
        f.message("a", &item("3", None), Some(150), none);
        f.message("a", &item("4", None), None, none);
        assert!(f.winner.is_none());
        f.message("b", &item("5", Some("native")), Some(1010), none);
        assert_eq!(f.winner, Some((1010, "b".into())));
        // A reminder never moves it; a late catch-up of an older message
        // cannot override a newer one.
        f.message("c", &item("6", Some("reminder")), Some(1400), |id| {
            id == "reminder"
        });
        f.message("a", &item("7", None), Some(1005), none);
        assert_eq!(f.winner, Some((1010, "b".into())));
        f.message("a", &item("8", None), Some(1020), none);
        assert_eq!(f.winner, Some((1020, "a".into())));
        // Detaching forgets it.
        assert!(f.attach(None));
        assert!(f.winner.is_none());
    }
    #[test]
    fn only_the_persons_threads_are_followed() {
        assert!(user_thread(
            &json!({"id":"t","ephemeral":false,"parentThreadId":null,"source":"vscode","threadSource":null})
        ));
        assert!(user_thread(&json!({"id":"t","threadSource":"user"})));
        assert!(!user_thread(&json!({"id":"t","ephemeral":true})));
        assert!(!user_thread(&json!({"id":"t","parentThreadId":"p"})));
        assert!(!user_thread(
            &json!({"id":"t","source":{"subAgent":{"other":"x"}}})
        ));
        assert!(!user_thread(
            &json!({"id":"t","threadSource":"thread_title"})
        ));
    }
    #[test]
    fn profile_arguments_preserve_tables_arrays_and_literal_shell_text() {
        let input = "model='$(false) literal'\n[model_providers.local]\nbase_url='http://localhost'\n[hooks]\nSessionStart=[{hooks=[{type='command',command='echo `literal`'}]}]\n";
        let original: toml::Table = toml::from_str(input).unwrap();
        let merged: toml::Table = toml::from_str(
            &profile_arguments(
                input,
                Path::new("/home/fixture.config.toml"),
                &toml::Table::new(),
            )
            .unwrap()
            .join("\n"),
        )
        .unwrap();
        assert_eq!(original, merged);
    }
    #[test]
    fn profile_hook_preferences_keep_native_hash_checks_and_field_precedence() {
        let base: toml::Table = toml::from_str("[hooks.state.\"/home/work.config.toml:SessionStart:0:0\"]\nenabled=false\ntrusted_hash='sha256:original'\n[hooks.state.\"/home/other.config.toml:SessionStart:0:0\"]\ntrusted_hash='sha256:unrelated'\n").unwrap();
        let profile = "[hooks.state.\"/home/work.config.toml:SessionStart:0:0\"]\nenabled=true\n";
        let converted: toml::Table = toml::from_str(
            &profile_arguments(profile, Path::new("/home/work.config.toml"), &base)
                .unwrap()
                .join("\n"),
        )
        .unwrap();
        let states = converted["hooks"]["state"].as_table().unwrap();
        let state = &states["/<session-flags>/config.toml:SessionStart:0:0"];
        assert_eq!(state["enabled"].as_bool(), Some(true));
        assert_eq!(state["trusted_hash"].as_str(), Some("sha256:original"));
        assert_eq!(states.len(), 2); // original preference plus its server alias
    }
    fn socket() -> PathBuf {
        std::env::temp_dir().join(format!(
            "gc-{}.sock",
            goblins_protocol::messages::operation_key(None).unwrap()
        ))
    }
    #[test]
    fn server_request_id_collision_never_answers_native_approvals() {
        let path = socket();
        let listener = UnixListener::bind(&path).unwrap();
        let worker = std::thread::spawn(move || {
            let (peer, _) = listener.accept().unwrap();
            peer.set_read_timeout(Some(Duration::from_millis(150)))
                .unwrap();
            let mut ws = tungstenite::accept(peer).unwrap();
            let init: Value = serde_json::from_str(ws.read().unwrap().to_text().unwrap()).unwrap();
            assert_eq!(init["params"]["capabilities"]["experimentalApi"], true);
            ws.send(Message::text(
                json!({"id":init["id"],"result":{}}).to_string(),
            ))
            .unwrap();
            ws.read().unwrap(); // initialized
            let call: Value = serde_json::from_str(ws.read().unwrap().to_text().unwrap()).unwrap();
            ws.send(Message::text(json!({"id":call["id"],"method":"item/tool/requestUserInput","params":{"threadId":"bound"}}).to_string())).unwrap();
            ws.send(Message::text(json!({"method":"thread/status/changed","params":{"threadId":"another-thread","status":{"type":"active","activeFlags":[]}}}).to_string())).unwrap();
            ws.send(Message::text(
                json!({"id":call["id"],"result":{"ok":true}}).to_string(),
            ))
            .unwrap();
            assert!(matches!(ws.read(), Err(tungstenite::Error::Io(_))));
        });
        let (tx, rx) = mpsc::channel();
        let mut server = Server::connect(&path, tx).unwrap();
        server.thread = Some("bound".into());
        assert_eq!(
            server
                .call("thread/queue/list", json!({"threadId":"bound"}))
                .unwrap(),
            json!({"ok":true})
        );
        assert!(rx.try_recv().is_err());
        worker.join().unwrap();
        fs::remove_file(path).unwrap();
    }
    #[test]
    fn observer_accepts_large_server_frames() {
        let root = std::env::temp_dir().join(format!(
            "go-{}",
            goblins_protocol::messages::operation_key(None).unwrap()
        ));
        fs::create_dir(&root).unwrap();
        let endpoint = root.join("server.sock");
        let listener = UnixListener::bind(&endpoint).unwrap();
        let remote = std::thread::spawn(move || {
            let (peer, _) = listener.accept().unwrap();
            let mut ws = tungstenite::accept(peer).unwrap();
            let request: Value =
                serde_json::from_str(ws.read().unwrap().to_text().unwrap()).unwrap();
            ws.send(Message::text(
                json!({"id":request["id"],"result":{"padding":"x".repeat((64 << 20) + 1)}})
                    .to_string(),
            ))
            .unwrap();
            let initialized: Value =
                serde_json::from_str(ws.read().unwrap().to_text().unwrap()).unwrap();
            assert_eq!(initialized["method"], "initialized");
        });
        let (transitions, _states) = mpsc::channel();
        let _server = Server::connect(&endpoint, transitions).unwrap();
        remote.join().unwrap();
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn failed_reports_retry_with_only_the_latest_values() {
        let root = std::env::temp_dir().join(format!(
            "gr-{}",
            goblins_protocol::messages::operation_key(None).unwrap()
        ));
        fs::create_dir(&root).unwrap();
        let socket = root.join("request.sock");
        let listener = UnixListener::bind(&socket).unwrap();
        REQUEST_SOCKET.set(socket).unwrap();
        let (calls, received) = mpsc::channel();
        std::thread::spawn(move || {
            for (attempt, peer) in listener.incoming().enumerate() {
                let mut peer = peer.unwrap();
                let init = rpc::read(&mut peer).unwrap();
                peer.write_all(
                    &rpc::encode(&rpc::result(init["id"].clone(), json!({"api":1}))).unwrap(),
                )
                .unwrap();
                let call = rpc::read(&mut peer).unwrap();
                let _ = calls.send(call["params"]["variables"].clone());
                // The first report fails, as an unreachable daemon would.
                let reply = if attempt == 0 {
                    rpc::error(call["id"].clone(), -32009, "not yet")
                } else {
                    rpc::result(call["id"].clone(), json!({"accepted":true,"recorded":true}))
                };
                peer.write_all(&rpc::encode(&reply).unwrap()).unwrap();
            }
        });
        let reporter = conversation_reporter();
        reporter.send(("old".into(), "Old".into())).unwrap();
        let first = received.recv_timeout(Duration::from_secs(5)).unwrap();
        assert_eq!(first, json!({"session_id":"old","session_name":"Old"}));
        reporter.send(("new".into(), "New".into())).unwrap();
        let retried = received.recv_timeout(Duration::from_secs(5)).unwrap();
        assert_eq!(retried, json!({"session_id":"new","session_name":"New"}));
        // Acknowledged values are not sent again.
        reporter.send(("new".into(), "New".into())).unwrap();
        assert!(received.recv_timeout(Duration::from_secs(2)).is_err());
        fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn disconnect_after_submission_is_not_an_explicit_rejection() {
        let path = socket();
        let listener = UnixListener::bind(&path).unwrap();
        let worker = std::thread::spawn(move || {
            let (peer, _) = listener.accept().unwrap();
            let mut ws = tungstenite::accept(peer).unwrap();
            let v: Value = serde_json::from_str(ws.read().unwrap().to_text().unwrap()).unwrap();
            ws.send(Message::text(json!({"id":v["id"],"result":{}}).to_string()))
                .unwrap();
            ws.read().unwrap();
            ws.read().unwrap(); // submission committed, reply lost
        });
        let (tx, _) = mpsc::channel();
        let mut server = Server::connect(&path, tx).unwrap();
        let error = server
            .call("thread/queue/add", json!({"clientUserMessageId":"owned"}))
            .unwrap_err();
        assert!(error.downcast_ref::<Rejected>().is_none());
        worker.join().unwrap();
        fs::remove_file(path).unwrap();
    }
}
