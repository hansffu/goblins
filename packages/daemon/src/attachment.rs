//! Host terminal relay; the daemon retains the PTY and owns sandbox lifetime.
use crate::RESIZE;
use crate::STOP;
use goblins_controller::Result;
use goblins_controller::host::Client;
use goblins_jsonrpc as rpc;
use goblins_protocol::contract;
use goblins_protocol::terminal::size;
use goblins_unix as unix;
use std::{
    io::Write, os::fd::AsRawFd, os::unix::net::UnixStream, path::PathBuf, sync::atomic::Ordering,
};

/// Host launch choices; the wire request also adds nonce, cwd and terminal size.
pub struct Start {
    pub name: String,
    pub configuration: String,
    pub agent_name: Option<String>,
    pub scope: Option<String>,
    pub dev_shell: Option<String>,
    /// Omitted from the request when unset.
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
        text(Some(&record.agent_name)),
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

/// Attach to the goblin whose launch or record is `record`: a hello with this
/// terminal's size on its `attach.sock`.
fn relay(mut control: Client, session: &str, record: &serde_json::Value) -> Result<i32> {
    let terminal = handshake(session, record, None)?;
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
    session: &str,
    record: &serde_json::Value,
    terminal: Option<&str>,
) -> Result<UnixStream> {
    let path = record["attach"].as_str().ok_or("missing attach path")?;
    let stream = UnixStream::connect(path)?;
    let d = size(0)?;
    greet(
        stream,
        session,
        (d.ws_row.max(1), d.ws_col.max(1)),
        terminal,
        || eprintln!("Starting…"),
    )
}
fn greet(
    mut stream: UnixStream,
    session: &str,
    (rows, cols): (u16, u16),
    terminal: Option<&str>,
    starting: impl FnMut(),
) -> Result<UnixStream> {
    let params = contract::AttachHelloParams {
        version: contract::ATTACH_VERSION,
        rows,
        cols,
        terminal: terminal.map(str::to_owned),
    };
    stream.write_all(&rpc::encode(&contract::notification::<
        contract::attach_notifications::AttachHello,
    >(params)?)?)?;
    let hello = completion(&mut stream, session, starting)?;
    contract::decode_completion::<contract::attach_exchanges::Reply>(hello)?;
    Ok(stream)
}

/// The attachment's completion, read past the waiting notifications before
/// it; `starting` runs at the first that says the goblin is still starting.
fn completion(
    stream: &mut UnixStream,
    session: &str,
    mut starting: impl FnMut(),
) -> Result<serde_json::Value> {
    let mut started = false;
    Ok(loop {
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
            if unix::readable(stream.as_raw_fd(), left.as_millis() as i32)? {
                break;
            }
            if left.is_zero() {
                return Err("terminal attachment timed out".into());
            }
        }
        let frame = rpc::read(stream)?;
        let Some(waiting) = contract::attach_waiting(&frame)? else {
            break frame;
        };
        // Shown once the goblin itself runs and its client waits.
        if !started && waiting.has_missing {
            starting();
            started = true;
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fake_daemon::{Daemon, fixtures};
    use serde_json::{Value, json};
    use std::io::Read;

    #[test]
    fn hello_waits_past_notices_and_hands_over_the_raw_stream() {
        let waiting = [
            json!({"jsonrpc":"2.0","method":"attach.waiting","params":{"missing":[]}}),
            json!({"jsonrpc":"2.0","method":"attach.waiting","params":{"missing":["session_id"]}}),
            json!({"jsonrpc":"2.0","method":"attach.waiting","params":{"missing":["thread"]}}),
        ];
        let malformed = [
            json!({"jsonrpc":"old","id":null,"method":"attach.waiting","params":{"missing":[]}}),
            json!({"jsonrpc":"2.0","method":"attach.waiting","params":{"missing":[7]}}),
            json!({"jsonrpc":"2.0","method":"attach.waiting","params":{"missing":"bad"}}),
        ];
        let success = json!({"jsonrpc":"2.0","id":0,"result":{"attached":true,"future":1}});
        let cases = [
            (&waiting[..], success.clone(), None, 1),
            (&waiting[..1], success.clone(), None, 0),
            (&malformed[..1], success.clone(), Some("invalid waiting notification"), 0),
            (&malformed[1..2], success.clone(), Some("invalid waiting notification"), 0),
            (&malformed[2..], success.clone(), Some("invalid waiting notification"), 0),
            (&[], success, None, 0),
            (
                &[],
                json!({"jsonrpc":"2.0","id":1,"result":{"attached":true}}),
                Some("invalid correlated response"),
                0,
            ),
            (
                &[],
                json!({"jsonrpc":"2.0","id":0,"result":{"attached":false}}),
                Some("invalid terminal handshake"),
                0,
            ),
            (
                &[],
                json!({"jsonrpc":"2.0","id":0,"error":{"code":-32602,"message":"old refusal"}}),
                Some("old refusal"),
                0,
            ),
        ];
        let payload = b"RAW_C06\nContent-Length: 2\r\n\r\n{}\n";
        let _no_forks = unix::no_forks();
        for (notices, completion, error, shown) in cases {
            let (stream, mut goblin) = UnixStream::pair().unwrap();
            goblin.set_read_timeout(Some(rpc::TIMEOUT)).unwrap();
            for frame in notices.iter().chain([&completion]) {
                goblin.write_all(&rpc::encode(frame).unwrap()).unwrap();
            }
            goblin.write_all(payload).unwrap();
            goblin.shutdown(std::net::Shutdown::Write).unwrap();
            let mut starting = 0;
            let accepted = greet(stream, "a", (24, 100), None, || starting += 1);
            assert_eq!(
                rpc::read(&mut goblin).unwrap(),
                json!({"jsonrpc":"2.0","method":"attach.hello","params":{"version":1,"rows":24,"cols":100}})
            );
            assert_eq!(starting, shown, "{completion}");
            match (accepted, error) {
                (Ok(mut stream), None) => {
                    let mut raw = vec![];
                    stream.read_to_end(&mut raw).unwrap();
                    assert_eq!(raw, payload);
                }
                (Err(e), Some(error)) => assert!(e.to_string().contains(error), "{e}"),
                (accepted, _) => panic!("{completion}: {:?}", accepted.map(drop)),
            }
            // Nothing follows the hello. A refused frontend leaves the raw
            // bytes unread, so its close may reset the connection.
            let mut sent = vec![];
            match goblin.read_to_end(&mut sent) {
                Err(e) if error.is_some() && e.kind() == std::io::ErrorKind::ConnectionReset => {}
                read => assert_eq!(read.unwrap(), 0, "{completion}"),
            }
        }
    }

    #[test]
    fn hello_names_the_terminal_it_attaches() {
        let (stream, mut goblin) = UnixStream::pair().unwrap();
        goblin.set_read_timeout(Some(rpc::TIMEOUT)).unwrap();
        goblin
            .write_all(
                &rpc::encode(&json!({"jsonrpc":"2.0","id":0,"result":{"attached":true}})).unwrap(),
            )
            .unwrap();
        greet(stream, "a", (30, 120), Some("t1"), || {
            panic!("not starting")
        })
        .unwrap();
        assert_eq!(
            rpc::read(&mut goblin).unwrap()["params"],
            json!({"version":1,"rows":30,"cols":120,"terminal":"t1"})
        );
    }

    /// The `sessions.start` request a detached launch sends.
    fn launch(command: impl FnOnce(PathBuf) -> Result<i32>) -> Value {
        let _no_forks = unix::no_forks();
        let daemon = Daemon::start(
            &[],
            fixtures()["forward_compatibility"]["host_launch"].clone(),
        );
        assert_eq!(command(daemon.state.clone()).unwrap(), 0);
        let call = daemon.call();
        assert_eq!(call["method"], "sessions.start");
        let key = call["params"]["key"].as_str().unwrap();
        assert!(
            key.len() == 32 && key.bytes().all(|b| b.is_ascii_hexdigit()),
            "{key}"
        );
        call["params"].clone()
    }

    #[test]
    fn run_sends_its_choices_with_unset_ones_null_and_no_options() {
        let params = launch(|state| {
            run(
                state,
                Start {
                    name: "shell".into(),
                    configuration: "/manifest.json".into(),
                    agent_name: None,
                    scope: None,
                    dev_shell: None,
                    raid: None,
                },
                true,
            )
        });
        let mut expected = json!({
            "configuration":"/manifest.json","rows":24,"cols":80,"detached":true,
            "name":"shell","agent_name":null,"scope":null,"dev_shell":null,
            "cwd":std::env::current_dir().unwrap(),
        });
        expected["key"] = params["key"].clone();
        assert_eq!(params, expected);
    }

    #[test]
    fn resume_sends_the_record_to_recreate() {
        let params =
            launch(|state| resume(state, "/manifest.json".into(), "record-id".into(), true));
        let mut expected = json!({
            "configuration":"/manifest.json","rows":24,"cols":80,"detached":true,
            "recreate":"record-id",
        });
        expected["key"] = params["key"].clone();
        assert_eq!(params, expected);
    }
}
