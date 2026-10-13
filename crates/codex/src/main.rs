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
    sync::mpsc::{self, Receiver, Sender},
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};
use tungstenite::{Message, WebSocket};

type Result<T> = std::result::Result<T, Box<dyn std::error::Error>>;
#[derive(Parser)]
struct Cli {
    #[arg(long, global = true)]
    runtime_dir: Option<PathBuf>,
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
fn daemon(method: &str, value: Value) -> Result<Value> {
    let mut s = UnixStream::connect(goblins_protocol::REQUEST_SOCKET)?;
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
        let (ws, _) = tungstenite::client("ws://localhost/", stream)?;
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
/// The terminal alone owns this connection. Forward every payload unchanged;
/// only successful foreground start/resume/fork responses select a thread.
/// Codex defers SessionStart until the first turn, so this also binds an empty
/// terminal and immediately guards against late compaction hooks after a switch.
#[allow(clippy::result_large_err)] // Tungstenite callback requires an unboxed HTTP error response.
fn native_proxy(listener: &UnixListener, upstream: &Path, dir: &Path) -> Result<()> {
    let (native_stream, _) = listener.accept()?;
    native_stream.set_read_timeout(Some(Duration::from_secs(3)))?;
    native_stream.set_write_timeout(Some(Duration::from_secs(3)))?;
    let stream = UnixStream::connect(upstream)?;
    stream.set_read_timeout(Some(Duration::from_secs(3)))?;
    stream.set_write_timeout(Some(Duration::from_secs(3)))?;
    let (mut server, handshake) = tungstenite::client("ws://localhost/", stream)?;
    let cap = handshake
        .headers()
        .get("x-codex-websocket-max-unfragmented-message-bytes")
        .cloned();
    let mut native = tungstenite::accept_hdr(
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
    )?;
    native
        .get_ref()
        .set_read_timeout(Some(Duration::from_millis(10)))?;
    server
        .get_ref()
        .set_read_timeout(Some(Duration::from_millis(10)))?;
    let mut pending = std::collections::BTreeSet::new();
    fn receive(ws: &mut WebSocket<UnixStream>) -> Result<Option<Message>> {
        match ws.read() {
            Ok(message) => Ok(Some(message)),
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
    loop {
        if let Some(message) = receive(&mut native)? {
            if let Message::Text(text) = &message
                && let Ok(v) = serde_json::from_str::<Value>(text)
                && foreground_thread_request(&v)
                && let Some(id) = v.get("id")
            {
                if pending.len() >= 64 {
                    return Err("too many pending native thread requests".into());
                }
                pending.insert(id.to_string());
            }
            let closing = matches!(message, Message::Close(_));
            if matches!(
                message,
                Message::Text(_) | Message::Binary(_) | Message::Close(_)
            ) {
                server.send(message)?;
            }
            if closing {
                return Ok(());
            }
        }
        if let Some(message) = receive(&mut server)? {
            if let Message::Text(text) = &message
                && let Ok(v) = serde_json::from_str::<Value>(text)
                && v.get("method").is_none()
                && let Some(id) = v.get("id")
                && pending.remove(&id.to_string())
                && let Some(thread) = v["result"]["thread"]["id"].as_str()
            {
                atomic(&dir.join("native-thread.json"), &thread)?;
                atomic(&dir.join("thread.json"), &thread)?;
            }
            let closing = matches!(message, Message::Close(_));
            if matches!(
                message,
                Message::Text(_) | Message::Binary(_) | Message::Close(_)
            ) {
                native.send(message)?;
            }
            if closing {
                return Ok(());
            }
        }
    }
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

fn run(dir: &Path, socket: &Path, native_socket: Option<&Path>) -> Result<()> {
    let lock = OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .mode(0o600)
        .open(dir.join("run.lock"))?;
    if unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
        return Err("adapter already running".into());
    }
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
        std::thread::spawn(move || {
            loop {
                if let Err(e) = native_proxy(&listener, &upstream, &runtime) {
                    eprintln!("native proxy: {e}");
                }
                // The terminal has disconnected; do not deliver to its old thread.
                let _ = fs::remove_file(runtime.join("thread.json"));
                // A tombstone also rejects delayed hooks from the disconnected
                // thread while the terminal reconnects and selects its next one.
                let _ = atomic(&runtime.join("native-thread.json"), &"");
                // Keep accepting the terminal's reconnects. It still selects the
                // thread explicitly; the adapter never retries a human task.
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
        let _ = reporter.transitions.send(report);
        for error in reporter.errors.try_iter() {
            report_error = error;
        }
        if report_error.is_some() {
            diagnostic = report_error.clone();
        }
        atomic(
            &dir.join("status.json"),
            &json!({"thread_id":bound,"agent_state":report,"paused":paused,"diagnostic":diagnostic,"updated_at_ms":now_ms(),"threads":ledger.threads}),
        )?;
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
            } => run(&dir, &socket, native_socket.as_deref()),
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
                json!({"id":v["id"],"result":{"thread":{"id":"exact"}}}).to_string(),
            ))
            .unwrap();
            let reply: Value = serde_json::from_str(ws.read().unwrap().to_text().unwrap()).unwrap();
            assert_eq!(reply, json!({"id":7,"result":{"answers":{}}}));
            let resume: Value =
                serde_json::from_str(ws.read().unwrap().to_text().unwrap()).unwrap();
            assert_eq!(resume["method"], "thread/resume");
            ws.send(Message::text(
                json!({"id":resume["id"],"result":{"thread":{"id":"switched"}}}).to_string(),
            ))
            .unwrap();
            let _ = ws.read();
        });
        let runtime = root.clone();
        let proxy = std::thread::spawn(move || {
            native_proxy(&native_listener, &upstream, &runtime).unwrap()
        });
        let stream = UnixStream::connect(&endpoint).unwrap();
        let (mut native, handshake) = tungstenite::client("ws://localhost/", stream).unwrap();
        assert_eq!(
            handshake.headers()["x-codex-websocket-max-unfragmented-message-bytes"],
            "16777216"
        );
        native.send(Message::text(json!({"id":7,"method":"thread/start","params":{"input":[{"type":"localImage","path":"input.png"}]}}).to_string())).unwrap();
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
