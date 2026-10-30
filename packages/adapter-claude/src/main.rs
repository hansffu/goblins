//! Claude's inbox integration, inside the goblin: the launcher starts a run,
//! every managed hook updates the ledger, and the plugin monitor's `watch`
//! prints a line for each wakeup. The daemon sees only generic calls.
use clap::{Parser, Subcommand};
use goblins_claude::{CONTINUE, Hook, Ledger, wake_line};
use goblins_protocol::{AgentState, rpc};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    fs::{self, DirBuilder, File, OpenOptions},
    io::{self, Write},
    os::{
        fd::AsRawFd,
        unix::{
            fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt},
            net::UnixStream,
        },
    },
    path::{Path, PathBuf},
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

type Result<T> = std::result::Result<T, Box<dyn std::error::Error>>;
#[derive(Parser)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}
#[derive(Subcommand)]
enum Command {
    /// Start a fresh ledger for this launch of Claude
    Start,
    /// Run a managed hook: Claude's hook input on stdin, its output on stdout
    Hook {
        /// The plugin monitor runs: a turn never continues from its Stop
        #[arg(long)]
        monitor: bool,
    },
    /// The plugin monitor: one line per inbox wakeup
    Watch,
    Status,
    Pause,
    Resume,
}
const RUNTIME: &str = "/tmp/goblins-claude";
/// Hooks must answer within their own timeout, whatever the daemon does.
const HOOK_LOCK: Duration = Duration::from_secs(1);
const HOOK_CALL: Duration = Duration::from_secs(2);

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
    rpc::exchange(&mut s, json!(0), "initialize", json!({"api":1}))?;
    Ok(rpc::exchange(&mut s, json!(1), method, value)?)
}
/// A call bounded as a whole: the protocol's timeouts apply to each
/// exchange. One still running when the time is up is abandoned, so only
/// the short-lived hook process uses it, for a read.
fn within<T: Send + 'static>(
    timeout: Duration,
    call: impl FnOnce() -> std::result::Result<T, String> + Send + 'static,
) -> Result<T> {
    let (send, receive) = std::sync::mpsc::channel();
    std::thread::spawn(move || send.send(call()));
    match receive.recv_timeout(timeout) {
        Ok(result) => Ok(result?),
        Err(_) => Err(format!("no answer within {timeout:?}").into()),
    }
}
fn bounded(timeout: Duration, method: &'static str, value: Value) -> Result<Value> {
    within(timeout, move || {
        daemon(method, value).map_err(|e| e.to_string())
    })
}
/// Hooks and the monitor are separate processes: they change the ledger
/// only under this lock, waiting for it at most `wait`.
fn lock(dir: &Path, wait: Duration) -> Result<File> {
    let lock = OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .mode(0o600)
        .open(dir.join("ledger.lock"))?;
    let deadline = Instant::now() + wait;
    while unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
        let e = io::Error::last_os_error();
        if e.kind() != io::ErrorKind::WouldBlock && e.kind() != io::ErrorKind::Interrupted {
            return Err(e.into());
        }
        if Instant::now() >= deadline {
            return Err("ledger is busy".into());
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    Ok(lock)
}
fn locked<T>(dir: &Path, wait: Duration, change: impl FnOnce(&mut Ledger) -> T) -> Result<T> {
    let _lock = lock(dir, wait)?;
    let path = dir.join("ledger.json");
    let mut ledger: Ledger = read(&path)?;
    let result = change(&mut ledger);
    atomic(&path, &ledger)?;
    Ok(result)
}
fn private(dir: &Path) -> Result<()> {
    let meta = fs::symlink_metadata(dir)?;
    if !meta.is_dir()
        || meta.permissions().mode() & 0o777 != 0o700
        || std::os::unix::fs::MetadataExt::uid(&meta) != unsafe { libc::geteuid() }
    {
        return Err("runtime must be a private directory owned by this user".into());
    }
    Ok(())
}
fn start(dir: &Path) -> Result<String> {
    match DirBuilder::new().mode(0o700).create(dir) {
        Err(e) if e.kind() != io::ErrorKind::AlreadyExists => return Err(e.into()),
        _ => private(dir)?,
    }
    let _lock = lock(dir, Duration::from_secs(5))?;
    let run = goblins_protocol::messages::operation_key(None)?;
    atomic(
        &dir.join("ledger.json"),
        &Ledger::new(run.clone(), now_ms()),
    )?;
    Ok(run)
}
/// The hook's output for Claude; an error answers as a failed daemon call.
fn hook(dir: &Path, input: &Hook, monitor: bool) -> Result<Value> {
    let event = input.hook_event_name.as_str();
    let inbox = match event {
        "Stop" => Some(bounded(HOOK_CALL, "inbox.status", json!({}))?),
        _ => None,
    };
    let continuation = locked(dir, HOOK_LOCK, |ledger| {
        let continuation = match &inbox {
            Some(inbox) => ledger.stop(input.stop_hook_active || monitor, inbox),
            None => {
                ledger.event(
                    event,
                    input.tool_name.as_deref(),
                    input.notification_type.as_deref(),
                )?;
                false
            }
        };
        Ok::<_, String>(continuation)
    })??;
    Ok(if continuation {
        json!({"decision":"block","reason":CONTINUE})
    } else {
        json!({})
    })
}
enum Poll {
    /// A newer launch of Claude started its own run.
    Replaced,
    Wake(String),
    Quiet,
}
/// One monitor poll, with the inbox status it read outside the lock, and
/// the activity to report: one the daemon has not accepted yet, or any for
/// a heartbeat.
fn poll(
    dir: &Path,
    run: &str,
    inbox: Result<Value>,
    heartbeat: bool,
) -> Result<(Poll, Option<AgentState>)> {
    locked(dir, Duration::from_secs(5), |ledger| {
        if ledger.run != run {
            return (Poll::Replaced, None);
        }
        let now = now_ms();
        let poll = match inbox {
            Ok(inbox) => {
                ledger.diagnostic = None;
                if ledger.tick(&inbox, now) {
                    Poll::Wake(wake_line(&inbox))
                } else {
                    Poll::Quiet
                }
            }
            Err(e) => {
                ledger.last_seen_ms = now;
                ledger.diagnostic = Some(format!("inbox status failed: {e}"));
                Poll::Quiet
            }
        };
        let report =
            (heartbeat || ledger.reported != Some(ledger.activity)).then_some(ledger.activity);
        (poll, report)
    })
}
/// The monitor is the only activity reporter, so reports reach the daemon
/// in order; it records what the daemon accepted, in its own run only.
fn report(dir: &Path, run: &str, state: AgentState) -> Result<()> {
    accepted(
        dir,
        run,
        state,
        daemon("sessions.set-state", json!({"state":state})),
    )
}
fn accepted(dir: &Path, run: &str, state: AgentState, result: Result<Value>) -> Result<()> {
    locked(dir, Duration::from_secs(5), |ledger| match result {
        _ if ledger.run != run => (),
        Ok(_) => ledger.reported = Some(state),
        Err(e) => {
            ledger.reported = None;
            ledger.diagnostic = Some(format!("activity report failed: {e}"));
        }
    })
}
/// Polls every 500 ms and repeats the activity every ten seconds, so it
/// stays fresh within the daemon's expiry for as long as the monitor runs.
fn watch(dir: &Path) -> Result<()> {
    let run = read::<Ledger>(&dir.join("ledger.json"))?.run;
    let mut reported = Instant::now() - Duration::from_secs(10);
    loop {
        let tick = Instant::now();
        let heartbeat = reported.elapsed() >= Duration::from_secs(10);
        if heartbeat {
            reported = tick;
        }
        let inbox = daemon("inbox.status", json!({}));
        let (poll, activity) = poll(dir, &run, inbox, heartbeat)?;
        if let Some(state) = activity {
            report(dir, &run, state)?;
        }
        match poll {
            Poll::Replaced => return Ok(()),
            Poll::Wake(line) => {
                let mut out = io::stdout().lock();
                writeln!(out, "{line}")?;
                out.flush()?;
            }
            Poll::Quiet => (),
        }
        std::thread::sleep(Duration::from_millis(500).saturating_sub(tick.elapsed()));
    }
}
fn main() {
    let cli = Cli::parse();
    let dir = Path::new(RUNTIME);
    let result = (|| -> Result<Value> {
        if let Command::Start = cli.command {
            return Ok(json!({"run":start(dir)?}));
        }
        private(dir)?;
        match cli.command {
            Command::Start => unreachable!(),
            Command::Hook { monitor } => hook(dir, &serde_json::from_reader(io::stdin())?, monitor),
            Command::Watch => watch(dir).map(|()| Value::Null),
            Command::Status => Ok(read::<Ledger>(&dir.join("ledger.json"))?.status(now_ms())),
            Command::Pause => {
                locked(dir, Duration::from_secs(5), |l| l.paused = true)?;
                Ok(json!({"paused":true}))
            }
            Command::Resume => {
                locked(dir, Duration::from_secs(5), Ledger::resume)?;
                Ok(json!({"paused":false}))
            }
        }
    })();
    match result {
        Ok(Value::Null) => (),
        Ok(output) => println!("{output}"),
        Err(e) => {
            eprintln!("goblins-claude: {e}");
            std::process::exit(1);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn runtime() -> PathBuf {
        // Never this goblin's own daemon, but one that never answers:
        // connections wait on a listener nobody accepts.
        static UNRESPONSIVE: std::sync::OnceLock<std::os::unix::net::UnixListener> =
            std::sync::OnceLock::new();
        UNRESPONSIVE.get_or_init(|| {
            let path = std::env::temp_dir().join(format!(
                "goblins-claude-test-{}.sock",
                goblins_protocol::messages::operation_key(None).unwrap()
            ));
            let listener = std::os::unix::net::UnixListener::bind(&path).unwrap();
            REQUEST_SOCKET.set(path).unwrap();
            listener
        });
        let dir = std::env::temp_dir().join(format!(
            "goblins-claude-test-{}",
            goblins_protocol::messages::operation_key(None).unwrap()
        ));
        DirBuilder::new().mode(0o700).create(&dir).unwrap();
        dir
    }
    fn pending() -> Result<Value> {
        Ok(json!({"queued":1,"claim":null,"head":["m1",0],"audit_failed":false}))
    }
    #[test]
    fn a_stale_monitor_exits_when_a_new_run_starts() {
        let dir = runtime();
        let first = start(&dir).unwrap();
        assert!(matches!(
            poll(&dir, &first, pending(), false).unwrap().0,
            Poll::Wake(_)
        ));
        let second = start(&dir).unwrap();
        assert!(matches!(
            poll(&dir, &first, pending(), false).unwrap().0,
            Poll::Replaced
        ));
        // The new run's monitor starts over with a fresh ledger.
        assert!(matches!(
            poll(&dir, &second, pending(), false).unwrap().0,
            Poll::Wake(_)
        ));
        fs::remove_dir_all(dir).unwrap();
    }
    #[test]
    fn a_pause_survives_a_restart_of_watch() {
        let dir = runtime();
        let run = start(&dir).unwrap();
        locked(&dir, HOOK_LOCK, |l| l.paused = true).unwrap();
        assert!(matches!(
            poll(&dir, &run, pending(), false).unwrap().0,
            Poll::Quiet
        ));
        // A restarted monitor reads the same run and ledger.
        let again = read::<Ledger>(&dir.join("ledger.json")).unwrap();
        assert_eq!(again.run, run);
        assert!(matches!(
            poll(&dir, &again.run, pending(), false).unwrap().0,
            Poll::Quiet
        ));
        assert_eq!(again.status(now_ms())["health"], "paused");
        locked(&dir, HOOK_LOCK, Ledger::resume).unwrap();
        assert!(matches!(
            poll(&dir, &run, pending(), false).unwrap().0,
            Poll::Wake(_)
        ));
        fs::remove_dir_all(dir).unwrap();
    }
    #[test]
    fn stop_answers_within_its_bound_when_the_daemon_does_not() {
        let dir = runtime();
        start(&dir).unwrap();
        let stop = serde_json::from_value(json!({"hook_event_name":"Stop"})).unwrap();
        let started = Instant::now();
        assert!(hook(&dir, &stop, false).is_err());
        assert!(started.elapsed() < HOOK_CALL + Duration::from_millis(500));
        // The ledger stays free for the next hook.
        let prompt = serde_json::from_value(json!({"hook_event_name":"UserPromptSubmit"})).unwrap();
        assert_eq!(hook(&dir, &prompt, false).unwrap(), json!({}));
        fs::remove_dir_all(dir).unwrap();
    }
    #[test]
    fn the_monitor_reports_the_newest_activity_in_order() {
        let dir = runtime();
        let run = start(&dir).unwrap();
        let empty = || Ok(json!({"queued":0,"claim":null,"head":null}));
        let hook = |event| locked(&dir, HOOK_LOCK, |l| l.event(event, None, None)).unwrap();
        hook("SessionStart").unwrap();
        let (_, idle) = poll(&dir, &run, empty(), false).unwrap();
        assert_eq!(idle, Some(AgentState::Idle));
        // A hook's newer activity while that report is under way.
        hook("UserPromptSubmit").unwrap();
        accepted(&dir, &run, AgentState::Idle, Ok(json!({}))).unwrap();
        let (_, working) = poll(&dir, &run, empty(), false).unwrap();
        assert_eq!(working, Some(AgentState::Working));
        accepted(&dir, &run, AgentState::Working, Ok(json!({}))).unwrap();
        // A report of an earlier run is never recorded in this one.
        accepted(&dir, "earlier", AgentState::Idle, Ok(json!({}))).unwrap();
        // Accepted activity is repeated only as the heartbeat.
        assert_eq!(poll(&dir, &run, empty(), false).unwrap().1, None);
        assert_eq!(
            poll(&dir, &run, empty(), true).unwrap().1,
            Some(AgentState::Working)
        );
        fs::remove_dir_all(dir).unwrap();
    }
    #[test]
    fn failed_daemon_calls_are_diagnostics() {
        let dir = runtime();
        let run = start(&dir).unwrap();
        poll(&dir, &run, Err("unreachable".into()), true).unwrap();
        report(&dir, &run, AgentState::Idle).unwrap();
        let ledger = read::<Ledger>(&dir.join("ledger.json")).unwrap();
        assert!(
            ledger.status(now_ms())["diagnostic"]
                .as_str()
                .unwrap()
                .contains("failed")
        );
        assert_eq!(ledger.reported, None);
        fs::remove_dir_all(dir).unwrap();
    }
}
