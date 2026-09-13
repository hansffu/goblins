//! Local server management. Stop uses the authenticated host connection, never a PID file.
use crate::STOP;
use goblins_controller::{Result, controller::Controller, host::Client, unix};
use serde_json::json;
use std::{
    fs::{File, OpenOptions},
    io,
    os::{
        fd::AsRawFd,
        unix::{
            fs::{MetadataExt, OpenOptionsExt},
            process::CommandExt,
        },
    },
    path::{Path, PathBuf},
    process::{Command, Stdio},
    sync::atomic::Ordering,
    thread,
    time::{Duration, Instant},
};

pub fn foreground(state: &Path, workspace: Option<PathBuf>) -> Result<()> {
    let mut daemon = Controller::new(state, workspace)?;
    println!(
        "Goblins daemon ready at {}",
        state.join("host.sock").display()
    );
    while !STOP.load(Ordering::Relaxed) && !daemon.shutdown_ready() {
        daemon.tick()?;
        thread::sleep(Duration::from_millis(5));
    }
    println!("Goblins server stopping; cleaning up agents");
    drop(daemon);
    println!("Goblins server stopped");
    Ok(())
}

fn lock_held(state: &Path) -> Result<bool> {
    let file = match OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(state.join("daemon.lock"))
    {
        Ok(file) => file,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(false),
        Err(e) => return Err(e.into()),
    };
    if !file.metadata()?.is_file() {
        return Err("daemon lock must be a regular file".into());
    }
    if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == 0 {
        return Ok(false);
    }
    let error = io::Error::last_os_error();
    if error.kind() == io::ErrorKind::WouldBlock {
        Ok(true)
    } else {
        Err(error.into())
    }
}

fn log_file(state: &Path, write: bool) -> Result<File> {
    let file = OpenOptions::new()
        .read(!write)
        .append(write)
        .create(write)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(state.join("server.log"))?;
    let meta = file.metadata()?;
    if !meta.is_file() || meta.uid() != unsafe { libc::getuid() } || meta.mode() & 0o077 != 0 {
        return Err("server log must be a private regular file owned by you".into());
    }
    Ok(file)
}

pub fn start(state: &Path, workspace: Option<&Path>) -> Result<()> {
    unix::private_directory(state)?;
    let _management = unix::lock(&state.join("server-control.lock"))?;
    if let Some(client) = Client::connect_optional(state)? {
        println!("Server already running ({})", client.instance);
        return Ok(());
    }
    if lock_held(state)? {
        return Err("server is starting or stopping; try 'goblins server status' shortly".into());
    }
    let log = log_file(state, true)?;
    let mut command = Command::new(std::env::current_exe()?);
    command
        .arg("--state-dir")
        .arg(state)
        .args(["server", "start", "--foreground"])
        .stdin(Stdio::null())
        .stdout(log.try_clone()?)
        .stderr(log);
    if let Some(path) = workspace {
        command.arg("--workspace").arg(path);
    }
    // Start a new POSIX session so closing the launching terminal leaves the
    // server alive. Only async-signal-safe setsid runs between fork and exec.
    unsafe {
        command.pre_exec(|| {
            unix::cvt(libc::setsid())?;
            Ok(())
        });
    }
    let mut child = command.spawn()?;
    let deadline = Instant::now() + Duration::from_secs(10);
    let result = (|| -> Result<()> {
        loop {
            if let Some(status) = child.try_wait()? {
                return Err(format!(
                    "server exited during startup ({status}); see 'goblins server logs'"
                )
                .into());
            }
            if let Some(client) = Client::connect_optional(state)? {
                println!("Server started ({})", client.instance);
                return Ok(());
            }
            if Instant::now() >= deadline || STOP.load(Ordering::Relaxed) {
                return Err("server startup did not complete; see 'goblins server logs'".into());
            }
            thread::sleep(Duration::from_millis(20));
        }
    })();
    if result.is_err() {
        // Kill only the child we just spawned, never a recorded/reused PID.
        let _ = child.kill();
        let _ = child.wait();
    }
    result
}

pub fn stop(state: &Path) -> Result<()> {
    if !state.exists() {
        println!("Server already stopped");
        return Ok(());
    }
    unix::private_directory(state)?;
    let _management = unix::lock(&state.join("server-control.lock"))?;
    let Some(mut client) = Client::connect_optional(state)? else {
        if lock_held(state)? {
            return Err(
                "server is starting or stopping; try 'goblins server status' shortly".into(),
            );
        }
        println!("Server already stopped");
        return Ok(());
    };
    client.call("server.stop", json!({}))?;
    let deadline = Instant::now() + Duration::from_secs(15);
    while lock_held(state)? {
        if Instant::now() >= deadline || STOP.load(Ordering::Relaxed) {
            return Err("stop accepted; server cleanup has not finished yet".into());
        }
        thread::sleep(Duration::from_millis(20));
    }
    println!("Server stopped");
    Ok(())
}

pub fn status(state: &Path) -> Result<i32> {
    if let Some(mut client) = Client::connect_optional(state)? {
        let status = client.call("server.status", json!({}))?;
        println!(
            "Server {} ({}) · {} live agents",
            status["state"].as_str().unwrap_or("unknown"),
            client.instance,
            status["sessions"]
        );
        Ok(0)
    } else if lock_held(state)? {
        println!("Server starting or stopping (not accepting connections)");
        Ok(1)
    } else {
        println!("Server stopped");
        Ok(1)
    }
}

pub fn logs(state: &Path) -> Result<()> {
    match log_file(state, false) {
        Ok(mut log) => {
            io::copy(&mut log, &mut io::stdout().lock())?;
        }
        Err(e)
            if e.downcast_ref::<io::Error>()
                .is_some_and(|e| e.kind() == io::ErrorKind::NotFound) =>
        {
            println!("No server output recorded yet")
        }
        Err(e) => return Err(e),
    }
    Ok(())
}
