//! Host terminal relay; the daemon retains the PTY and owns sandbox lifetime.
use crate::RESIZE;
use crate::STOP;
use goblins_controller::Result;
use goblins_controller::host::Client;
use goblins_protocol::contract;
use goblins_protocol::terminal::size;
use goblins_unix as unix;
use std::{os::fd::AsRawFd, path::PathBuf, sync::atomic::Ordering};

/// Host launch choices; the wire request also adds nonce, cwd and terminal size.
pub struct Start {
    pub name: String,
    pub configuration: String,
    pub agent_name: Option<String>,
    pub scope: Option<String>,
    pub dev_shell: Option<String>,
    /// Omitted when unset, so an older daemon still accepts the launch.
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
    let params = contract::HostLaunchParams {
        key,
        name: start.name,
        configuration: start.configuration,
        agent_name: start.agent_name,
        scope: start.scope,
        dev_shell: start.dev_shell,
        raid: start.raid,
        cwd: Some(std::env::current_dir()?),
        rows: dimensions.ws_row.max(1),
        cols: dimensions.ws_col.max(1),
        detached: detatched,
        recreate: None,
        options: Default::default(),
    };
    let (launch, wire) = control.call_typed_with_wire::<contract::host::SessionsStart>(params)?;
    if detatched {
        println!("{wire}");
        return Ok(0);
    }
    relay(control, &launch.session, &wire)
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
    let (launch, wire) = control.call_typed_with_wire::<contract::host::SessionsStart>(
        contract::HostLaunchParams {
            key,
            configuration,
            name: String::new(),
            agent_name: None,
            cwd: None,
            scope: None,
            dev_shell: None,
            rows: dimensions.ws_row.max(1),
            cols: dimensions.ws_col.max(1),
            detached,
            raid: None,
            recreate: Some(id),
            options: Default::default(),
        },
    )?;
    if detached {
        println!("{wire}");
        return Ok(0);
    }
    let session = launch.session.as_str();
    let record = loop {
        let record =
            control.call_typed::<contract::host::SessionsGet>(contract::SessionSelectorParams {
                session: session.to_string(),
            })?;
        if record.state != "starting" {
            break record;
        }
        if STOP.load(Ordering::Relaxed) {
            return Err(format!("interrupted; {session} is still starting").into());
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    };
    let text = |v: Option<&str>| goblins_protocol::plain_text(v.unwrap_or("-"));
    if record.state == "failed" {
        return Err(format!(
            "resuming {} failed: {}",
            text(record.recreated_from.as_deref()),
            text(record.detail.as_deref())
        )
        .into());
    }
    eprintln!(
        "Resumed {} ({}) as {session}",
        text(record.agent_name.as_deref()),
        text(record.recreated_from.as_deref())
    );
    for skipped in &record.skipped {
        eprintln!(
            "Skipped: {} — {}",
            text(Some(&skipped.name)),
            text(Some(&skipped.reason))
        );
    }
    if let Some(detail) = record.detail.as_deref() {
        eprintln!("{}", goblins_protocol::plain_text(detail));
    }
    relay(control, session, &serde_json::to_value(record.clone())?)
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
    let record =
        control.call_typed::<contract::host::SessionsGet>(contract::SessionSelectorParams {
            session: session.to_string(),
        })?;
    // Do not resolve reusable agent names for an editor's retained identity.
    if instance.is_some() && record.id != session {
        return Err("session identity mismatch".into());
    }
    let session = record.id.as_str();
    // Apply the Ghostel window's actual dimensions once the PTY is ready.
    RESIZE.store(true, Ordering::Relaxed);
    relay(control, session, &serde_json::to_value(record.clone())?)
}

/// Attach to the goblin whose launch or record is `record`. A daemon with
/// attach handlers takes a hello with this terminal's size on `attach.sock`;
/// an older one attaches `terminal.sock` at once.
fn relay(mut control: Client, session: &str, record: &serde_json::Value) -> Result<i32> {
    let terminal = handshake(&control, session, record, None)?;
    goblins_protocol::terminal::relay(
        terminal,
        session,
        |method, params| control.call(method, params),
        &STOP,
        &RESIZE,
    )
}

/// Connect to a goblin's attach socket and be accepted: for the goblin's own
/// terminal, or with `terminal` for one of its terminals.
pub fn handshake(
    control: &Client,
    session: &str,
    record: &serde_json::Value,
    terminal: Option<&str>,
) -> Result<std::os::unix::net::UnixStream> {
    use goblins_jsonrpc as rpc;
    use std::{io::Write, os::unix::net::UnixStream};
    let attach = record["attach"]
        .as_str()
        .filter(|_| terminal.is_some() || control.features.iter().any(|f| f == "attach-handlers"));
    let modern = attach.is_some();
    let mut terminal_stream = match attach {
        Some(path) => {
            let mut stream = UnixStream::connect(path)?;
            let d = size(0)?;
            let params = contract::AttachHelloParams {
                version: contract::ATTACH_VERSION,
                rows: d.ws_row.max(1),
                cols: d.ws_col.max(1),
                terminal: terminal.map(str::to_owned),
            };
            stream.write_all(&rpc::encode(&contract::notification::<
                contract::attach_notifications::AttachHello,
            >(params)?)?)?;
            stream
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
            if unix::readable(terminal_stream.as_raw_fd(), left.as_millis() as i32)? {
                break;
            }
            if left.is_zero() {
                return Err("terminal attachment timed out".into());
            }
        }
        let frame = rpc::read(&mut terminal_stream)?;
        let Some(waiting) = contract::attach_waiting(&frame) else {
            break frame;
        };
        // Shown once the goblin itself runs and its client waits.
        if !starting && waiting.has_missing {
            eprintln!("Starting…");
            starting = true;
        }
    };
    let _ = if modern {
        contract::decode_completion::<contract::attach_exchanges::Reply>(hello)?
    } else {
        contract::decode_completion::<contract::attach_exchanges::LegacyAttach>(hello)?
    };
    Ok(terminal_stream)
}
