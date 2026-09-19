//! Host terminal relay; the daemon retains the PTY and owns sandbox lifetime.
use crate::{RESIZE, STOP};
use goblins_controller::host::Client;
use goblins_controller::{Result, unix};
use goblins_protocol::terminal::size;
use serde_json::json;
use std::{os::fd::AsRawFd, path::PathBuf, sync::atomic::Ordering};

pub fn run(
    state: PathBuf,
    name: String,
    configuration: String,
    agent_name: Option<String>,
    parent: Option<String>,
    detatched: bool,
) -> Result<i32> {
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
    let launch=control.call("sessions.start",json!({"key":key,"configuration":configuration,"name":name,"agent_name":agent_name,"parent":parent,"detached":detatched,"cwd":std::env::current_dir()?,"rows":dimensions.ws_row.max(1),"cols":dimensions.ws_col.max(1)}))?;
    if detatched {
        println!("{launch}");
        return Ok(0);
    }
    let session = launch["session"].as_str().ok_or("missing session ID")?;
    relay(
        control,
        session,
        launch["terminal"].as_str().ok_or("missing terminal path")?,
    )
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
    relay(
        control,
        session,
        record["terminal"].as_str().ok_or("missing terminal path")?,
    )
}

fn relay(mut control: Client, session: &str, path: &str) -> Result<i32> {
    use std::os::unix::net::UnixStream;
    let mut terminal = UnixStream::connect(path)?;
    // rpc::read allows an unlimited idle wait for permission replies. Bound
    // this handshake's first byte separately; subsequent bytes are timed.
    if !unix::readable(
        terminal.as_raw_fd(),
        goblins_protocol::rpc::TIMEOUT.as_millis() as i32,
    )? {
        return Err("terminal attachment timed out".into());
    }
    let hello = goblins_protocol::rpc::read(&mut terminal)?;
    goblins_protocol::rpc::validate_response(&hello, &json!(0))?;
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
