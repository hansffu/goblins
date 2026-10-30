//! Host terminal relay; the daemon retains the PTY and owns sandbox lifetime.
use crate::{RESIZE, STOP};
use goblins_controller::host::Client;
use goblins_controller::{Result, unix};
use goblins_protocol::terminal::size;
use serde::Serialize;
use serde_json::json;
use std::{os::fd::AsRawFd, path::PathBuf, sync::atomic::Ordering};

/// What a host launch asks the daemon to start (`sessions.start`).
#[derive(Serialize)]
pub struct Start {
    pub name: String,
    pub configuration: String,
    pub agent_name: Option<String>,
    pub scope: Option<String>,
    pub dev_shell: Option<String>,
    /// Omitted when unset, so an older daemon still accepts the launch.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub raid: Option<String>,
}

pub fn run(state: PathBuf, start: Start, detatched: bool) -> Result<i32> {
    use std::io::Read;
    if !detatched && unsafe { libc::isatty(0) != 1 } {
        return Err("goblins run requires terminal input".into());
    }
    let mut control = Client::connect(&state)?;
    let dimensions = if detatched {
        libc::winsize {
            ws_row: 24,
            ws_col: 80,
            ws_xpixel: 0,
            ws_ypixel: 0,
        }
    } else {
        size(0)?
    };
    let mut random = [0; 16];
    std::fs::File::open("/dev/urandom")?.read_exact(&mut random)?;
    let key: String = random.iter().map(|b| format!("{b:02x}")).collect();
    let mut params = serde_json::to_value(start)?;
    params.as_object_mut().unwrap().extend([
        ("key".into(), json!(key)),
        ("detached".into(), json!(detatched)),
        ("cwd".into(), json!(std::env::current_dir()?)),
        ("rows".into(), json!(dimensions.ws_row.max(1))),
        ("cols".into(), json!(dimensions.ws_col.max(1))),
    ]);
    let launch = control.call("sessions.start", params)?;
    if detatched {
        println!("{launch}");
        return Ok(0);
    }
    let session = launch["session"].as_str().ok_or("missing session ID")?;
    relay(control, session, &launch)
}

/// `goblins resume`: recreate a recorded goblin from the current manifest,
/// wait until it runs, report what it could not restore, then attach.
pub fn resume(state: PathBuf, configuration: String, id: String, detached: bool) -> Result<i32> {
    use std::io::Read;
    if !detached && unsafe { libc::isatty(0) != 1 } {
        return Err("goblins resume requires terminal input; use --detached".into());
    }
    let mut control = Client::connect(&state)?;
    let dimensions = if detached {
        libc::winsize {
            ws_row: 24,
            ws_col: 80,
            ws_xpixel: 0,
            ws_ypixel: 0,
        }
    } else {
        size(0)?
    };
    let mut random = [0; 16];
    std::fs::File::open("/dev/urandom")?.read_exact(&mut random)?;
    let key: String = random.iter().map(|b| format!("{b:02x}")).collect();
    let launch = control.call(
        "sessions.start",
        json!({"key":key,"configuration":configuration,"recreate":id,"detached":detached,
            "rows":dimensions.ws_row.max(1),"cols":dimensions.ws_col.max(1)}),
    )?;
    if detached {
        println!("{launch}");
        return Ok(0);
    }
    let session = launch["session"].as_str().ok_or("missing session ID")?;
    let record = loop {
        let record = control.call("sessions.get", json!({"session":session}))?;
        if record["state"] != "starting" {
            break record;
        }
        if STOP.load(Ordering::Relaxed) {
            return Err(format!("interrupted; {session} is still starting").into());
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    };
    let text = |v: &serde_json::Value| goblins_protocol::plain_text(v.as_str().unwrap_or("-"));
    if record["state"] == "failed" {
        return Err(format!(
            "resuming {} failed: {}",
            text(&record["recreated_from"]),
            text(&record["detail"])
        )
        .into());
    }
    eprintln!(
        "Resumed {} ({}) as {session}",
        text(&record["agent_name"]),
        text(&record["recreated_from"])
    );
    for skipped in record["skipped"].as_array().into_iter().flatten() {
        eprintln!(
            "Skipped: {} — {}",
            text(&skipped["name"]),
            text(&skipped["reason"])
        );
    }
    if let Some(detail) = record["detail"].as_str() {
        eprintln!("{}", goblins_protocol::plain_text(detail));
    }
    relay(control, session, &record)
}

pub fn attach(state: PathBuf, session: String, instance: Option<String>) -> Result<i32> {
    if unsafe { libc::isatty(0) != 1 } {
        return Err("goblins attach requires terminal input".into());
    }
    let mut control = Client::connect(&state)?;
    if instance
        .as_ref()
        .is_some_and(|expected| control.instance != *expected)
    {
        return Err("daemon instance changed; cannot attach".into());
    }
    let record = control.call("sessions.get", json!({"session":session}))?;
    // Do not resolve reusable agent names for an editor's retained identity.
    if instance.is_some() && record["id"] != session {
        return Err("session identity mismatch".into());
    }
    let session = record["id"].as_str().ok_or("missing session ID")?;
    // Apply the Ghostel window's actual dimensions once the PTY is ready.
    RESIZE.store(true, Ordering::Relaxed);
    relay(control, session, &record)
}

/// Attach to the goblin whose launch or record is `record`. A daemon with
/// attach handlers takes a hello with this terminal's size on `attach.sock`;
/// an older one attaches `terminal.sock` at once.
fn relay(mut control: Client, session: &str, record: &serde_json::Value) -> Result<i32> {
    use goblins_jsonrpc as rpc;
    use std::{io::Write, os::unix::net::UnixStream};
    let attach = record["attach"]
        .as_str()
        .filter(|_| control.features.iter().any(|f| f == "attach-handlers"));
    let mut terminal = match attach {
        Some(path) => {
            let mut terminal = UnixStream::connect(path)?;
            let d = size(0)?;
            terminal.write_all(&rpc::encode(
                &json!({"jsonrpc":"2.0","method":"attach.hello",
                "params":{"version":1,"rows":d.ws_row.max(1),"cols":d.ws_col.max(1)}}),
            )?)?;
            terminal
        }
        None => UnixStream::connect(record["terminal"].as_str().ok_or("missing terminal path")?)?,
    };
    let mut starting = false;
    let hello = loop {
        // rpc::read allows an unlimited idle wait for permission replies.
        // Bound each handshake frame's first byte separately; a waiting
        // notification extends the deadline to the next one. A resize's
        // SIGWINCH interrupts the wait without ending it; Ctrl-C ends it.
        let deadline = std::time::Instant::now() + rpc::TIMEOUT;
        loop {
            if STOP.load(Ordering::Relaxed) {
                return Err(format!("interrupted before {session} was attached").into());
            }
            let left = deadline.saturating_duration_since(std::time::Instant::now());
            if unix::readable(terminal.as_raw_fd(), left.as_millis() as i32)? {
                break;
            }
            if left.is_zero() {
                return Err("terminal attachment timed out".into());
            }
        }
        let frame = rpc::read(&mut terminal)?;
        if frame["method"] != "attach.waiting" {
            break frame;
        }
        // Shown once the goblin itself runs and its client waits.
        if !starting
            && frame["params"]["missing"]
                .as_array()
                .is_some_and(|m| !m.is_empty())
        {
            eprintln!("Starting…");
            starting = true;
        }
    };
    rpc::validate_response(&hello, &json!(0))?;
    if let Some(error) = hello.get("error") {
        return Err(error["message"]
            .as_str()
            .unwrap_or("attachment rejected")
            .into());
    }
    if hello["result"]["attached"] != true {
        return Err("invalid terminal handshake".into());
    }
    goblins_protocol::terminal::relay(
        terminal,
        session,
        |method, params| control.call(method, params),
        &STOP,
        &RESIZE,
    )
}
