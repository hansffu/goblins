//! Codex App Server 0.162.1 adapter. Runs entirely inside the reporting sandbox.
use clap::{Parser, Subcommand};
use goblins_codex::{Delivery, activity};
use goblins_protocol::{AgentState, rpc};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    fs::{self, File, OpenOptions},
    io::{self, Read, Write},
    os::{
        fd::AsRawFd,
        unix::{
            fs::{FileTypeExt, MetadataExt, OpenOptionsExt, PermissionsExt},
            net::{UnixListener, UnixStream},
        },
    },
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex,
        mpsc::{self, Receiver, Sender},
    },
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};
use tungstenite::{Message, WebSocket, protocol::WebSocketConfig};

/// Match Codex's remote client receive limits. The handshake's advertised
/// 16 MiB cap limits client uploads; server model/configuration responses can
/// be larger. Tungstenite's default 16 MiB frame / 64 MiB message limits would
/// disconnect the terminal before it receives those responses.
fn websocket_config() -> WebSocketConfig {
    WebSocketConfig::default()
        .max_frame_size(Some(128 << 20))
        .max_message_size(Some(128 << 20))
        .max_write_buffer_size((128 << 20) + (128 << 10))
}

mod transport;

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
        /// Pass through the native connection to capture exact start/resume IDs
        #[arg(long)]
        native_socket: Option<PathBuf>,
        /// The goblin bypasses Codex's approvals (bypassPermissions)
        #[arg(long)]
        bypass_permissions: bool,
    },
    Hook,
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
        let deadline = Instant::now() + Duration::from_secs(3);
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
        if self.thread.as_deref() != Some(thread) {
            if let Some(old) = self.thread.clone() {
                let _ = self.call("thread/unsubscribe", json!({"threadId":old}));
            }
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
#[derive(Default)]
struct NativeForeground {
    next_request: u64,
    selected_request: u64,
    owner: Option<(u64, String)>,
    diagnostic: Option<String>,
}
impl NativeForeground {
    fn allows(&self, thread: Option<&str>) -> bool {
        // Hook-only launchers have no proxied selections. Once the native
        // protocol has selected (or suspended), it owns delivery authority.
        self.next_request == 0
            || self
                .owner
                .as_ref()
                .is_some_and(|(_, selected)| Some(selected.as_str()) == thread)
    }
}

/// Connections used only for pickers/catalogs never own the foreground.
/// Serialize selection, renames and disconnect cleanup across connections so
/// an old connection cannot erase or rename a newer connection's selection.
struct NativeConnection {
    id: u64,
    dir: PathBuf,
    conversation: Sender<(String, String)>,
    foreground: Arc<Mutex<NativeForeground>>,
}
impl NativeConnection {
    fn failed(&self, reason: &str) {
        self.foreground.lock().unwrap().diagnostic = Some(variable_text(&format!(
            "native connection {}: {reason}",
            self.id
        )));
    }
    fn suspend(&self, reason: &str) -> Result<()> {
        let mut state = self.foreground.lock().unwrap();
        state.next_request += 1;
        state.selected_request = state.next_request;
        state.owner = None;
        state.diagnostic = Some(variable_text(reason));
        let _ = fs::remove_file(self.dir.join("thread.json"));
        atomic(&self.dir.join("native-thread.json"), &"")
    }
    fn requested(&self) -> u64 {
        let mut state = self.foreground.lock().unwrap();
        state.next_request += 1;
        state.next_request
    }
    fn selected(&self, request: u64, thread: &str, name: &str) -> Result<()> {
        let mut state = self.foreground.lock().unwrap();
        if request <= state.selected_request {
            return Ok(());
        }
        atomic(&self.dir.join("native-thread.json"), &thread)?;
        atomic(&self.dir.join("thread.json"), &thread)?;
        state.selected_request = request;
        state.owner = Some((self.id, thread.to_owned()));
        state.diagnostic = None;
        let _ = self.conversation.send((thread.to_owned(), name.to_owned()));
        Ok(())
    }
    fn renamed(&self, thread: &str, name: &str) {
        let state = self.foreground.lock().unwrap();
        if state
            .owner
            .as_ref()
            .is_some_and(|(id, selected)| *id == self.id && selected == thread)
        {
            let _ = self.conversation.send((thread.to_owned(), name.to_owned()));
        }
    }
}
impl Drop for NativeConnection {
    fn drop(&mut self) {
        let mut state = self.foreground.lock().unwrap();
        if state.owner.as_ref().is_none_or(|(id, _)| *id == self.id) {
            // A picker closing must not suspend delivery to the main terminal.
            // Clear only our binding, or unowned startup hooks left before a
            // successful foreground response. Tombstone delayed hooks too.
            let _ = fs::remove_file(self.dir.join("thread.json"));
            let _ = atomic(&self.dir.join("native-thread.json"), &"");
            state.owner = None;
        }
    }
}

/// Forward native connections, including temporary pickers. Only successful
/// foreground start/resume/fork responses select the terminal's thread.
/// Codex defers SessionStart until the first turn, so this also binds an empty
/// terminal and guards against late compaction hooks after a switch. Resume
/// and fork requests get this goblin's permissions (`thread_permissions`).
#[allow(clippy::result_large_err)] // Tungstenite callback requires an unboxed HTTP error response.
fn native_proxy(
    native_stream: UnixStream,
    upstream: &Path,
    connection: &NativeConnection,
    bypass: bool,
) -> Result<()> {
    native_stream.set_read_timeout(Some(Duration::from_secs(3)))?;
    native_stream.set_write_timeout(Some(Duration::from_secs(3)))?;
    let stream = UnixStream::connect(upstream)?;
    stream.set_read_timeout(Some(Duration::from_secs(3)))?;
    stream.set_write_timeout(Some(Duration::from_secs(3)))?;
    let (server, handshake) = tungstenite::client::client_with_config(
        "ws://localhost/",
        stream,
        Some(websocket_config()),
    )?;
    let cap = handshake
        .headers()
        .get("x-codex-websocket-max-unfragmented-message-bytes")
        .cloned();
    let native = tungstenite::accept_hdr_with_config(
        native_stream,
        move |_: &tungstenite::handshake::server::Request,
              mut response: tungstenite::handshake::server::Response| {
            if let Some(cap) = cap {
                response
                    .headers_mut()
                    .insert("x-codex-websocket-max-unfragmented-message-bytes", cap);
            }
            Ok(response)
        },
        Some(websocket_config()),
    )?;
    let mut pending: BTreeMap<String, PendingThread> = BTreeMap::new();
    transport::relay(native, server, |direction, message| {
        let Message::Text(text) = &message else {
            return Ok(transport::Action::forward(message));
        };
        let Ok(mut v) = serde_json::from_str::<Value>(text) else {
            return Ok(transport::Action::forward(message));
        };
        if direction == transport::Direction::Native {
            if matches!(
                v["method"].as_str(),
                Some("thread/start" | "thread/resume" | "thread/fork")
            ) && v["params"].is_object()
                && v.get("id").is_some()
            {
                if pending.len() >= 64 {
                    return Err("too many pending native thread requests".into());
                }
                let selection = foreground_thread_request(&v).then(|| connection.requested());
                let ambiguous = selection.is_none() && !background_thread_request(&v);
                if v["method"] == "thread/start" {
                    v["params"]["sandbox"] = json!("danger-full-access");
                }
                if let Err(error) = thread_permissions(&mut v["params"], bypass, |params| {
                    configured_approval(upstream, params)
                }) {
                    let diagnostic =
                        format!("Goblins could not apply this goblin's approval policy: {error}");
                    connection.failed(&diagnostic);
                    return Ok(transport::Action::reply(rpc_failure(&v["id"], &diagnostic)));
                }
                let request = PendingThread {
                    selection,
                    approval: v["params"]["approvalPolicy"].clone(),
                    sandbox: v["params"]["sandbox"].clone(),
                };
                pending.insert(v["id"].to_string(), request);
                let reply = if ambiguous {
                    let reason = "Goblins cannot identify the foreground request from this Codex version; automatic inbox delivery is suspended until a recognized conversation selection.";
                    connection.suspend(reason)?;
                    Some(Message::text(json!({"method":"configWarning","params":{"summary":reason,"details":null,"path":null,"range":null}}).to_string()))
                } else {
                    None
                };
                return Ok(transport::Action {
                    forward: Some(Message::text(v.to_string())),
                    reply,
                });
            }
        } else if v.get("method").is_none() {
            if let Some(request) = v.get("id").and_then(|id| pending.remove(&id.to_string()))
                && v.get("result").is_some()
            {
                if let Err(error) = verify_thread_permissions(upstream, &request, &mut v["result"])
                {
                    let diagnostic =
                        format!("Goblins could not verify this thread's permissions: {error}");
                    connection.failed(&diagnostic);
                    return Ok(transport::Action::forward(rpc_failure(
                        &v["id"],
                        &diagnostic,
                    )));
                }
                if let Some(selection) = request.selection {
                    let thread = v["result"]["thread"]["id"]
                        .as_str()
                        .ok_or("thread response has no id")?;
                    let name = v["result"]["thread"]["name"].as_str().unwrap_or_default();
                    connection.selected(selection, thread, name)?;
                }
                return Ok(transport::Action::forward(Message::text(v.to_string())));
            }
        } else if v["method"] == "thread/name/updated"
            && let Some(thread) = v["params"]["threadId"].as_str()
        {
            connection.renamed(
                thread,
                v["params"]["threadName"].as_str().unwrap_or_default(),
            );
        }
        Ok(transport::Action::forward(message))
    })
}

struct PendingThread {
    selection: Option<u64>,
    approval: Value,
    sandbox: Value,
}
fn rpc_failure(id: &Value, diagnostic: &str) -> Message {
    Message::text(json!({"id":id,"error":{"code":-32603,"message":diagnostic}}).to_string())
}
fn sandbox_type(mode: &Value) -> Result<&'static str> {
    match mode.as_str() {
        Some("danger-full-access") => Ok("dangerFullAccess"),
        Some("read-only") => Ok("readOnly"),
        Some("workspace-write") => Ok("workspaceWrite"),
        _ => Err("unsupported sandbox mode".into()),
    }
}
fn verify_thread_permissions(
    upstream: &Path,
    request: &PendingThread,
    result: &mut Value,
) -> Result<()> {
    let expected = sandbox_type(&request.sandbox)?;
    if result["approvalPolicy"] == request.approval && result["sandbox"]["type"] == expected {
        return Ok(());
    }
    let thread = result["thread"]["id"]
        .as_str()
        .ok_or("thread response has no id")?;
    let policy = match expected {
        "dangerFullAccess" => json!({"type":"dangerFullAccess"}),
        "readOnly" => json!({"type":"readOnly"}),
        _ => return Err("cannot safely repair a workspace sandbox without its full policy".into()),
    };
    let (transitions, _) = mpsc::channel();
    let mut server = Server::connect(upstream, transitions)?;
    let repaired = (|| -> Result<Value> {
        server.call(
            "thread/settings/update",
            json!({"threadId":thread,"approvalPolicy":request.approval,"sandboxPolicy":policy}),
        )?;
        let verified = server.call(
            "thread/resume",
            json!({"threadId":thread,"excludeTurns":true}),
        )?;
        if verified["approvalPolicy"] != request.approval || verified["sandbox"]["type"] != expected
        {
            return Err("Codex did not apply the requested approval and sandbox policy".into());
        }
        Ok(verified)
    })();
    let _ = server.ws.close(None);
    let verified = repaired?;
    for key in ["approvalPolicy", "sandbox", "activePermissionProfile"] {
        if let Some(value) = verified.get(key) {
            result[key] = value.clone();
        }
    }
    Ok(())
}
fn background_thread_request(v: &Value) -> bool {
    v["params"]["threadSource"]
        .as_str()
        .is_some_and(|source| source != "user")
        || v["id"].as_str().is_some_and(|id| {
            id.starts_with("tui-dynamic-") || id.starts_with("temporary-structured-")
        })
}

/// In native Codex 0.162.1, foreground requests use integer IDs (or these
/// startup/fallback IDs). Hidden title/recap requests and background task tools
/// share the same connection but use their own string IDs. Neither selects the
/// terminal's conversation. Do not guess ownership for unfamiliar string IDs.
fn foreground_thread_request(v: &Value) -> bool {
    matches!(
        v["method"].as_str(),
        Some("thread/start" | "thread/resume" | "thread/fork")
    ) && (v["id"].is_i64()
        || v["id"].is_u64()
        || v["id"].as_str().is_some_and(|id| {
            id.starts_with("startup-thread-start-") || id.starts_with("legacy-thread-start-")
        }))
        && v["params"]["threadSource"]
            .as_str()
            .is_none_or(|source| source == "user")
}

/// A resumed or forked thread keeps the approval policy it was saved with, not
/// the server's, so set it on the request: "never" when the goblin bypasses
/// approvals, else the request's own (also as a config override) or the one
/// `configured` reads for the request. Codex leaves it unset to mean
/// on-request. The sandbox is always full access: Goblins is the sandbox, and
/// Codex's own cannot run inside a goblin.
fn thread_permissions(
    params: &mut Value,
    bypass: bool,
    configured: impl FnOnce(&Value) -> Result<Value>,
) -> Result<()> {
    let given = |value: &Value| (!value.is_null()).then(|| value.clone());
    if given(&params["sandbox"]).is_none() {
        params["sandbox"] = json!("danger-full-access");
    }
    let policy = if bypass {
        json!("never")
    } else if let Some(policy) =
        given(&params["approvalPolicy"]).or_else(|| given(&params["config"]["approval_policy"]))
    {
        policy
    } else {
        given(&configured(params)?).unwrap_or_else(|| json!("on-request"))
    };
    params["approvalPolicy"] = policy;
    Ok(())
}

/// The configured approval policy for a request's cwd, the resumed thread's
/// saved directory, or the server's default directory for a fresh thread.
/// Use a separate connection, without selecting the terminal's foreground.
fn configured_approval(upstream: &Path, params: &Value) -> Result<Value> {
    let (transitions, _) = mpsc::channel();
    let mut server = Server::connect(upstream, transitions)?;
    let result = (|| -> Result<Value> {
        let resuming = params["threadId"].is_string();
        let cwd = match &params["cwd"] {
            Value::Null if resuming => server.call(
                "thread/read",
                json!({"threadId":params["threadId"],"includeTurns":false}),
            )?["thread"]["cwd"]
                .clone(),
            cwd => cwd.clone(),
        };
        if !cwd.is_string() && (resuming || !cwd.is_null()) {
            return Err("no working directory for the thread".into());
        }
        let params = if cwd.is_null() {
            json!({})
        } else {
            json!({"cwd":cwd})
        };
        let read = server.call("config/read", params)?;
        match read.get("config") {
            Some(config) => Ok(config["approval_policy"].clone()),
            None => Err("config/read returned no config".into()),
        }
    })();
    let _ = server.ws.close(None);
    result
}

fn run(dir: &Path, socket: &Path, native_socket: Option<&Path>, bypass: bool) -> Result<()> {
    let lock = OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .mode(0o600)
        .open(dir.join("run.lock"))?;
    if unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
        return Err("adapter already running".into());
    }
    let proxy_state = Arc::new(Mutex::new(NativeForeground::default()));
    if let Some(path) = native_socket {
        if path.parent() != Some(dir) {
            return Err("native socket must be inside the private runtime directory".into());
        }
        match fs::symlink_metadata(path) {
            Ok(meta)
                if meta.file_type().is_socket() && meta.uid() == unsafe { libc::geteuid() } =>
            {
                fs::remove_file(path)?
            }
            Ok(_) => return Err("native socket path is occupied by a non-socket".into()),
            Err(e) if e.kind() == io::ErrorKind::NotFound => (),
            Err(e) => return Err(e.into()),
        }
        // The exclusive runtime lock proves an old adapter no longer owns it.
        let listener = UnixListener::bind(path)?;
        fs::set_permissions(path, fs::Permissions::from_mode(0o600))?;
        let upstream = socket.to_owned();
        let runtime = dir.to_owned();
        let conversation = conversation_reporter();
        let foreground = proxy_state.clone();
        std::thread::spawn(move || {
            let mut workers: Vec<std::thread::JoinHandle<()>> = Vec::new();
            for (id, stream) in listener.incoming().enumerate() {
                let stream = match stream {
                    Ok(stream) => stream,
                    Err(e) => {
                        eprintln!("native proxy accept: {e}");
                        foreground.lock().unwrap().diagnostic =
                            Some(format!("native proxy accept: {e}"));
                        continue;
                    }
                };
                workers.retain(|worker| !worker.is_finished());
                // Native pickers open additional connections while the main
                // terminal stays connected. Bound helpers and handshake stalls.
                if workers.len() >= 16 {
                    eprintln!("native proxy: too many connections");
                    foreground.lock().unwrap().diagnostic =
                        Some("native proxy: too many connections".into());
                    continue;
                }
                let connection = NativeConnection {
                    id: id as u64,
                    dir: runtime.clone(),
                    conversation: conversation.clone(),
                    foreground: foreground.clone(),
                };
                let upstream = upstream.clone();
                workers.push(std::thread::spawn(move || {
                    if let Err(e) = native_proxy(stream, &upstream, &connection, bypass) {
                        eprintln!("native proxy: {e}");
                        connection.failed(&e.to_string());
                    }
                }));
            }
        });
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
    let mut server: Option<Server> = None;
    let mut report = AgentState::Unknown;
    let reporter = Reporter::start();
    let mut report_error = None;
    loop {
        let tick = Instant::now();
        let mut diagnostic = None;
        let mut bound = None;
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
            bound = match read::<String>(&dir.join("thread.json")) {
                Ok(v) => Some(v),
                Err(e)
                    if e.downcast_ref::<io::Error>()
                        .is_some_and(|e| e.kind() == io::ErrorKind::NotFound) =>
                {
                    None
                }
                Err(e) => return Err(e),
            };
            let Some(thread) = bound.as_deref() else {
                report = AgentState::Unknown;
                return Ok(());
            };
            if server.is_none() {
                server = Some(Server::connect(socket, reporter.transitions.clone())?);
            }
            let s = server.as_mut().unwrap();
            s.bind(thread)?;
            report = activity(&s.status);
            let inbox = daemon("inbox.status", json!({}))?;
            if inbox["audit_failed"] != false {
                return Err("inbox audit unavailable; delivery suspended".into());
            }
            let d = ledger.threads.entry(thread.to_owned()).or_default();
            d.progress(&inbox["head"]);
            s.reconcile(thread, d)?;
            let foreground = proxy_state.lock().unwrap();
            if native_socket.is_some() && !foreground.allows(Some(thread)) {
                report = AgentState::Unknown;
                return Ok(());
            }
            if d.eligible(now_ms(), paused, &inbox) {
                let id = goblins_protocol::messages::operation_key(None)?;
                d.begin(id.clone(), now_ms());
                atomic(&dir.join("ledger.json"), &ledger)?; // write ahead of send
                let d = ledger.threads.get_mut(thread).unwrap();
                match s.call("thread/queue/add",json!({"threadId":thread,"clientUserMessageId":id,"input":[{"type":"text","text":goblins_protocol::INTEGRATION_PROMPT}]})) {
                    Ok(v) => {
                        d.pending.as_mut().unwrap().queue_id=v["queuedSubmission"]["id"].as_str().map(str::to_owned);
                        if d.pending.as_ref().unwrap().queue_id.is_none() { d.uncertain=true; }
                    }
                    Err(e) if e.downcast_ref::<Rejected>().is_some() => { d.pending=None;return Err(e); }
                    Err(e) => { d.uncertain=true;return Err(e); }
                }
            }
            drop(foreground);
            let d = ledger.threads.get(thread).unwrap();
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
        let foreground = proxy_state.lock().unwrap();
        if native_socket.is_some() && !foreground.allows(bound.as_deref()) {
            bound = None;
            report = AgentState::Unknown;
        }
        if let Some(proxy_error) = &foreground.diagnostic {
            diagnostic = Some(match diagnostic {
                Some(other) => format!("{proxy_error}; {other}"),
                None => proxy_error.clone(),
            });
        }
        let _ = reporter.transitions.send(report);
        atomic(
            &dir.join("status.json"),
            &json!({"thread_id":bound,"agent_state":report,"paused":paused,"diagnostic":diagnostic,"updated_at_ms":now_ms(),"threads":ledger.threads}),
        )?;
        drop(foreground);
        std::thread::sleep(Duration::from_millis(500).saturating_sub(tick.elapsed()));
    }
}
fn bind_hook(dir: &Path, v: &Value) -> Result<()> {
    if v["hook_event_name"] != "SessionStart" {
        return Err("expected SessionStart hook".into());
    }
    let id = v["session_id"].as_str().ok_or("hook has no session_id")?;
    if id.len() != 36
        || !id.bytes().enumerate().all(|(i, b)| {
            if [8, 13, 18, 23].contains(&i) {
                b == b'-'
            } else {
                b.is_ascii_hexdigit()
            }
        })
    {
        return Err("invalid hook thread ID".into());
    }
    match read::<String>(&dir.join("native-thread.json")) {
        Ok(native) if native != id => return Ok(()), // stale compaction hook
        Ok(_) => (),
        Err(e)
            if e.downcast_ref::<io::Error>()
                .is_some_and(|e| e.kind() == io::ErrorKind::NotFound) => {}
        Err(e) => return Err(e),
    }
    atomic(&dir.join("thread.json"), &id)
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
    let hook = matches!(cli.command, Command::Hook);
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
                native_socket,
                bypass_permissions,
            } => run(&dir, &socket, native_socket.as_deref(), bypass_permissions),
            Command::Hook => {
                let mut input = String::new();
                io::stdin().take(65537).read_to_string(&mut input)?;
                if input.len() > 65536 {
                    return Err("hook input too large".into());
                }
                let v: Value = serde_json::from_str(&input)?;
                bind_hook(&dir, &v)
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
    if hook {
        if let Err(e) = result {
            eprintln!("goblins-codex hook: {e}");
        }
        println!("{{}}");
    } else if let Err(e) = result {
        eprintln!("goblins-codex: {e}");
        std::process::exit(1);
    }
}

fn profile_arguments(input: &str, path: &Path, base: &toml::Table) -> Result<Vec<String>> {
    let mut profile: toml::Table = toml::from_str(input)?;
    // Codex 0.162.1 keys hook preferences by declaration file. Moving a
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

    fn proxy_once(
        listener: &UnixListener,
        upstream: &Path,
        dir: &Path,
        conversation: &Sender<(String, String)>,
        bypass: bool,
    ) -> Result<()> {
        let connection = NativeConnection {
            id: 0,
            dir: dir.to_owned(),
            conversation: conversation.clone(),
            foreground: Arc::default(),
        };
        native_proxy(listener.accept()?.0, upstream, &connection, bypass)
    }

    #[test]
    fn picker_and_old_connections_cannot_erase_or_rename_the_foreground() {
        let root = std::env::temp_dir().join(format!(
            "gf-{}",
            goblins_protocol::messages::operation_key(None).unwrap()
        ));
        fs::create_dir(&root).unwrap();
        let (conversation, reports) = mpsc::channel();
        let foreground = Arc::default();
        let connection = |id| NativeConnection {
            id,
            dir: root.clone(),
            conversation: conversation.clone(),
            foreground: Arc::clone(&foreground),
        };
        atomic(&root.join("thread.json"), &"unowned startup hook").unwrap();
        drop(connection(99));
        assert!(!root.join("thread.json").exists());
        assert_eq!(
            read::<String>(&root.join("native-thread.json")).unwrap(),
            ""
        );
        let original = connection(0);
        original
            .selected(original.requested(), "original", "Original")
            .unwrap();
        assert_eq!(
            reports.recv().unwrap(),
            ("original".into(), "Original".into())
        );
        let picker = connection(1);
        picker.renamed("original", "Not selected");
        drop(picker);
        assert_eq!(
            read::<String>(&root.join("thread.json")).unwrap(),
            "original"
        );
        assert!(reports.try_recv().is_err());

        let late = original.requested();
        let replacement = connection(2);
        replacement
            .selected(replacement.requested(), "replacement", "New")
            .unwrap();
        assert_eq!(
            reports.recv().unwrap(),
            ("replacement".into(), "New".into())
        );
        // A response to an older pending request must not undo a newer switch.
        original.selected(late, "late", "Late").unwrap();
        original.renamed("replacement", "Obsolete connection");
        drop(original);
        for file in ["thread.json", "native-thread.json"] {
            assert_eq!(read::<String>(&root.join(file)).unwrap(), "replacement");
        }
        assert!(reports.try_recv().is_err());
        replacement.renamed("replacement", "Renamed");
        assert_eq!(
            reports.recv().unwrap(),
            ("replacement".into(), "Renamed".into())
        );
        drop(replacement);
        assert!(!root.join("thread.json").exists());
        assert_eq!(
            read::<String>(&root.join("native-thread.json")).unwrap(),
            ""
        );
        fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn resumed_threads_get_the_goblins_permissions_not_their_saved_ones() {
        let apply = |params: Value, bypass: bool, configured: Value| {
            let mut params = params;
            let mut asked = None;
            thread_permissions(&mut params, bypass, |params| {
                asked = Some(params["cwd"].as_str().map(str::to_owned));
                Ok(configured)
            })
            .unwrap();
            (
                params["sandbox"].clone(),
                params["approvalPolicy"].clone(),
                asked,
            )
        };
        let granular =
            json!({"granular":{"sandbox_approval":true,"rules":false,"mcp_elicitations":true}});
        // Unset (missing or null) reads the configuration for the request's
        // directory; Codex's own default when that is unset too.
        assert_eq!(
            apply(
                json!({"threadId":"t","approvalPolicy":null,"sandbox":null}),
                false,
                Value::Null
            ),
            (json!("danger-full-access"), json!("on-request"), Some(None))
        );
        assert_eq!(
            apply(
                json!({"threadId":"t","cwd":"/work"}),
                false,
                granular.clone()
            ),
            (
                json!("danger-full-access"),
                granular.clone(),
                Some(Some("/work".into()))
            )
        );
        // The request's own policy wins, also given as a config override.
        assert_eq!(
            apply(
                json!({"threadId":"t","approvalPolicy":"never"}),
                false,
                json!("on-request")
            )
            .1,
            json!("never")
        );
        assert_eq!(
            apply(
                json!({"threadId":"t","config":{"approval_policy":granular}}),
                false,
                json!("never")
            ),
            (json!("danger-full-access"), granular, None)
        );
        // Bypassing always resumes with "never", without reading anything.
        assert_eq!(
            apply(
                json!({"threadId":"t","approvalPolicy":"on-request","sandbox":"read-only"}),
                true,
                Value::Null
            ),
            (json!("read-only"), json!("never"), None)
        );
        let mut params = json!({"threadId":"t"});
        assert!(thread_permissions(&mut params, false, |_| Err("unreachable".into())).is_err());
    }
    #[test]
    fn native_background_requests_do_not_select_the_conversation() {
        for method in ["thread/start", "thread/resume", "thread/fork"] {
            assert!(foreground_thread_request(
                &json!({"id":8,"method":method,"params":{}})
            ));
            assert!(!foreground_thread_request(
                &json!({"id":"tui-dynamic-task","method":method,"params":{}})
            ));
            assert!(!foreground_thread_request(
                &json!({"id":"temporary-structured-title","method":method,"params":{"ephemeral":true,"threadSource":"thread_title"}})
            ));
            assert!(!foreground_thread_request(
                &json!({"id":"unfamiliar","method":method,"params":{}})
            ));
        }
        for id in [
            "startup-thread-start-terminal",
            "legacy-thread-start-terminal",
        ] {
            assert!(foreground_thread_request(
                &json!({"id":id,"method":"thread/start","params":{}})
            ));
        }
        // A visible ephemeral conversation is still the terminal's thread;
        // feature ownership and request identity, rather than persistence, matter.
        assert!(foreground_thread_request(
            &json!({"id":8,"method":"thread/start","params":{"ephemeral":true}})
        ));
        assert!(!foreground_thread_request(
            &json!({"id":8,"method":"thread/start","params":{"threadSource":"thread_title"}})
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
    fn repeated_hooks_preserve_binding_and_stale_hooks_cannot_switch_it() {
        let root = std::env::temp_dir().join(format!(
            "gh-{}",
            goblins_protocol::messages::operation_key(None).unwrap()
        ));
        fs::create_dir(&root).unwrap();
        let current = "00000000-0000-4000-8000-000000000001";
        let previous = "00000000-0000-4000-8000-000000000002";
        atomic(&root.join("native-thread.json"), &current).unwrap();
        let v = json!({"hook_event_name":"SessionStart","session_id":current,"source":"compact"});
        bind_hook(&root, &v).unwrap();
        bind_hook(&root, &v).unwrap();
        bind_hook(
            &root,
            &json!({"hook_event_name":"SessionStart","session_id":previous}),
        )
        .unwrap();
        assert_eq!(read::<String>(&root.join("thread.json")).unwrap(), current);
        assert!(
            bind_hook(
                &root,
                &json!({"hook_event_name":"SubagentStart","session_id":previous})
            )
            .is_err()
        );
        assert!(bind_hook(&root,&json!({"hook_event_name":"SessionStart","session_id":"------------------------------------"})).is_err());
        fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn native_proxy_forwards_large_frames_in_both_directions() {
        let root = std::env::temp_dir().join(format!(
            "gl-{}",
            goblins_protocol::messages::operation_key(None).unwrap()
        ));
        fs::create_dir(&root).unwrap();
        let upstream = root.join("upstream.sock");
        let endpoint = root.join("native.sock");
        let listener = UnixListener::bind(&upstream).unwrap();
        let native_listener = UnixListener::bind(&endpoint).unwrap();
        // Cross both Tungstenite defaults: 16 MiB per frame, 64 MiB per
        // message. Real Codex model/configuration responses exceed 16 MiB.
        let payload = Message::text(
            json!({"method":"large","params":{"text":"x".repeat((64 << 20) + 1)}}).to_string(),
        );
        let expected = payload.clone();
        let remote = std::thread::spawn(move || {
            let (peer, _) = listener.accept().unwrap();
            peer.set_read_timeout(Some(Duration::from_secs(10)))
                .unwrap();
            let mut ws = tungstenite::accept_with_config(
                peer,
                Some(
                    tungstenite::protocol::WebSocketConfig::default()
                        .max_frame_size(Some(128 << 20))
                        .max_message_size(Some(128 << 20)),
                ),
            )
            .unwrap();
            let message = ws.read().unwrap();
            assert!(
                message == expected,
                "upstream received the complete message"
            );
            ws.send(message).unwrap();
            let _ = ws.read();
        });
        let runtime = root.clone();
        let (conversation, _reported) = mpsc::channel();
        let proxy = std::thread::spawn(move || {
            proxy_once(&native_listener, &upstream, &runtime, &conversation, false).unwrap()
        });
        let stream = UnixStream::connect(&endpoint).unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(10)))
            .unwrap();
        let (mut native, _) = tungstenite::client::client_with_config(
            "ws://localhost/",
            stream,
            Some(
                tungstenite::protocol::WebSocketConfig::default()
                    .max_frame_size(Some(128 << 20))
                    .max_message_size(Some(128 << 20)),
            ),
        )
        .unwrap();
        native.send(payload.clone()).unwrap();
        assert!(
            native.read().unwrap() == payload,
            "native received the complete message"
        );
        native.close(None).unwrap();
        proxy.join().unwrap();
        remote.join().unwrap();
        fs::remove_dir_all(root).unwrap();
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
    #[allow(clippy::result_large_err)] // Required by Tungstenite's handshake callback.
    fn native_proxy_binds_only_owned_responses_and_preserves_requests() {
        let root = std::env::temp_dir().join(format!(
            "gp-{}",
            goblins_protocol::messages::operation_key(None).unwrap()
        ));
        fs::create_dir(&root).unwrap();
        let upstream = root.join("upstream.sock");
        let endpoint = root.join("native.sock");
        let listener = UnixListener::bind(&upstream).unwrap();
        let native_listener = UnixListener::bind(&endpoint).unwrap();
        let (notice_tx, notice_rx) = mpsc::channel();
        let remote = std::thread::spawn(move || {
            let (peer, _) = listener.accept().unwrap();
            // The resume's approval policy comes from config/read on a
            // connection of its own, for the thread's saved directory.
            let config = std::thread::spawn(move || {
                let (peer, _) = listener.accept().unwrap();
                let mut ws = tungstenite::accept(peer).unwrap();
                let mut read = None;
                while let Ok(Message::Text(text)) = ws.read() {
                    let v: Value = serde_json::from_str(&text).unwrap();
                    if v["method"] == "thread/read" {
                        assert_eq!(
                            v["params"],
                            json!({"threadId":"switched","includeTurns":false})
                        );
                        ws.send(Message::text(json!({"id":v["id"],"result":{"thread":{"id":"switched","cwd":"/saved"}}}).to_string())).unwrap();
                    } else if v["method"] == "config/read" {
                        read = Some(v["params"].clone());
                        ws.send(Message::text(json!({"id":v["id"],"result":{"config":{"approval_policy":"on-request"}}}).to_string())).unwrap();
                    } else if v.get("id").is_some() {
                        ws.send(Message::text(json!({"id":v["id"],"result":{}}).to_string()))
                            .unwrap();
                    }
                }
                read
            });
            let mut ws = tungstenite::accept_hdr(
                peer,
                |_: &tungstenite::handshake::server::Request,
                 mut response: tungstenite::handshake::server::Response| {
                    response.headers_mut().insert(
                        "x-codex-websocket-max-unfragmented-message-bytes",
                        "16777216".parse().unwrap(),
                    );
                    Ok(response)
                },
            )
            .unwrap();
            let request = ws.read().unwrap();
            let v: Value = serde_json::from_str(request.to_text().unwrap()).unwrap();
            assert_eq!(v["params"]["input"][0]["type"], "localImage");
            ws.send(Message::text(
                json!({"method":"thread/started","params":{"thread":{"id":"foreign"}}}).to_string(),
            ))
            .unwrap();
            notice_rx.recv().unwrap();
            ws.send(Message::text(json!({"id":v["id"],"method":"item/tool/requestUserInput","params":{"threadId":"exact"}}).to_string())).unwrap();
            ws.send(Message::text(
                json!({"id":v["id"],"result":{"thread":{"id":"exact"},"approvalPolicy":"on-request","sandbox":{"type":"dangerFullAccess"}}}).to_string(),
            ))
            .unwrap();
            let reply: Value = serde_json::from_str(ws.read().unwrap().to_text().unwrap()).unwrap();
            assert_eq!(reply, json!({"id":7,"result":{"answers":{}}}));
            let resume: Value =
                serde_json::from_str(ws.read().unwrap().to_text().unwrap()).unwrap();
            assert_eq!(resume["method"], "thread/resume");
            assert_eq!(
                resume["params"],
                json!({"threadId":"switched","approvalPolicy":"on-request","sandbox":"danger-full-access"})
            );
            // Without a cwd, the directory the thread was saved with.
            assert_eq!(config.join().unwrap(), Some(json!({"cwd":"/saved"})));
            ws.send(Message::text(
                json!({"id":resume["id"],"result":{"thread":{"id":"switched"},"approvalPolicy":"on-request","sandbox":{"type":"dangerFullAccess"}}}).to_string(),
            ))
            .unwrap();
            let _ = ws.read();
        });
        let runtime = root.clone();
        let (conversation, _reported) = mpsc::channel();
        let proxy = std::thread::spawn(move || {
            proxy_once(&native_listener, &upstream, &runtime, &conversation, false).unwrap()
        });
        let stream = UnixStream::connect(&endpoint).unwrap();
        let (mut native, handshake) = tungstenite::client("ws://localhost/", stream).unwrap();
        assert_eq!(
            handshake.headers()["x-codex-websocket-max-unfragmented-message-bytes"],
            "16777216"
        );
        native.send(Message::text(json!({"id":7,"method":"thread/start","params":{"approvalPolicy":"on-request","input":[{"type":"localImage","path":"input.png"}]}}).to_string())).unwrap();
        native.read().unwrap(); // Foreign notifications do not bind.
        assert!(!root.join("thread.json").exists());
        notice_tx.send(()).unwrap();
        let approval: Value =
            serde_json::from_str(native.read().unwrap().to_text().unwrap()).unwrap();
        assert_eq!(approval["id"], 7);
        assert_eq!(approval["method"], "item/tool/requestUserInput");
        native.read().unwrap();
        assert_eq!(read::<String>(&root.join("thread.json")).unwrap(), "exact");
        native
            .send(Message::text(
                json!({"id":7,"result":{"answers":{}}}).to_string(),
            ))
            .unwrap();
        native
            .send(Message::text(
                json!({"id":8,"method":"thread/resume","params":{"threadId":"switched"}})
                    .to_string(),
            ))
            .unwrap();
        native.read().unwrap();
        assert_eq!(
            read::<String>(&root.join("native-thread.json")).unwrap(),
            "switched"
        );
        native.close(None).unwrap();
        proxy.join().unwrap();
        remote.join().unwrap();
        fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn the_foreground_thread_and_its_name_are_reported_together() {
        let root = std::env::temp_dir().join(format!(
            "gn-{}",
            goblins_protocol::messages::operation_key(None).unwrap()
        ));
        fs::create_dir(&root).unwrap();
        let upstream = root.join("upstream.sock");
        let endpoint = root.join("native.sock");
        let listener = UnixListener::bind(&upstream).unwrap();
        let native_listener = UnixListener::bind(&endpoint).unwrap();
        let remote = std::thread::spawn(move || {
            let (peer, _) = listener.accept().unwrap();
            let mut ws = tungstenite::accept(peer).unwrap();
            let answer = |ws: &mut WebSocket<UnixStream>, thread: Value| {
                let v: Value = serde_json::from_str(ws.read().unwrap().to_text().unwrap()).unwrap();
                assert_eq!(v["params"]["sandbox"], "danger-full-access");
                assert_eq!(v["params"]["approvalPolicy"], "never");
                ws.send(Message::text(
                    json!({"id":v["id"],"result":{"thread":thread,"approvalPolicy":"never","sandbox":{"type":"dangerFullAccess"}}}).to_string(),
                ))
                .unwrap();
            };
            let renamed = |thread: &str, name: &str| {
                Message::text(
                    json!({"method":"thread/name/updated","params":{"threadId":thread,"threadName":name}})
                        .to_string(),
                )
            };
            answer(&mut ws, json!({"id":"a","name":null}));
            ws.send(renamed("other", "Not ours")).unwrap();
            ws.send(renamed("a", "Named\tlater")).unwrap();
            answer(&mut ws, json!({"id":"c","name":"Old c"}));
            ws.send(renamed("a", "No longer foreground")).unwrap();
            let _ = ws.read();
        });
        let runtime = root.clone();
        let (conversation, reported) = mpsc::channel();
        let proxy = std::thread::spawn(move || {
            // Bypassing: the resume below needs no config/read connection.
            proxy_once(&native_listener, &upstream, &runtime, &conversation, true).unwrap()
        });
        let stream = UnixStream::connect(&endpoint).unwrap();
        let (mut native, _) = tungstenite::client("ws://localhost/", stream).unwrap();
        native
            .send(Message::text(
                json!({"id":1,"method":"thread/start","params":{}}).to_string(),
            ))
            .unwrap();
        for _ in 0..3 {
            native.read().unwrap();
        }
        native
            .send(Message::text(
                json!({"id":2,"method":"thread/resume","params":{"threadId":"c"}}).to_string(),
            ))
            .unwrap();
        for _ in 0..2 {
            native.read().unwrap();
        }
        native.close(None).unwrap();
        proxy.join().unwrap();
        remote.join().unwrap();
        let values: Vec<_> = reported.try_iter().collect();
        let pair = |a: &str, b: &str| (a.to_string(), b.to_string());
        assert_eq!(
            values,
            [pair("a", ""), pair("a", "Named\tlater"), pair("c", "Old c")]
        );
        assert_eq!(variable_text("Named\tlater"), "Named later");
        assert!(variable_text(&"é".repeat(200)).len() <= 256);
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
