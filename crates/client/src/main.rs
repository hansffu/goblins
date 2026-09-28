//! Untrusted request client. No controller or helper dependency.
mod cli;
mod preview;
use clap::Parser;
use goblins_protocol::{REQUEST_SOCKET, rpc};
use serde_json::json;
use std::{
    io::{Read, Write},
    os::unix::net::UnixStream,
};

fn parse_args(legacy: bool, args: &[String]) -> Result<(String, String), String> {
    let mut args = args.iter();
    let kind = if legacy { "package" } else { "request-package" };
    if args.next().map(String::as_str) != Some(kind) {
        return Err("usage: goblins request-package PACKAGE [--reason TEXT]".into());
    }
    let mut package = None;
    let mut reason = "Requested from the sandbox shell".to_string();
    let mut positional = false;
    while let Some(arg) = args.next() {
        if !positional && arg == "--reason" {
            reason = args.next().ok_or("--reason requires text")?.clone();
        } else if !positional && arg == "--" {
            positional = true;
        } else if !positional && arg.starts_with('-') {
            return Err(format!("unknown option: {arg}"));
        } else if package.replace(arg.clone()).is_some() {
            return Err("expected exactly one package attribute".into());
        }
    }
    Ok((package.ok_or("missing package attribute")?, reason))
}
fn run() -> Result<i32, String> {
    let mut args = std::env::args();
    let legacy = args.next().is_some_and(|a| {
        std::path::Path::new(&a)
            .file_name()
            .is_some_and(|n| n == "goblins-request")
    });
    let args: Vec<_> = args.collect();
    if !legacy {
        let command =
            cli::Cli::try_parse_from(std::iter::once("goblins".to_string()).chain(args.clone()))
                .unwrap_or_else(|e| e.exit())
                .command;
        match command {
            cli::Command::Integration { command } => return integration(command),
            cli::Command::Send {
                recipient,
                source,
                key,
            } => {
                let body =
                    goblins_protocol::messages::load_body(source.message, source.file.as_deref())
                        .map_err(|e| e.to_string())?;
                let key =
                    goblins_protocol::messages::operation_key(key).map_err(|e| e.to_string())?;
                return mailbox_command(
                    "messages.send",
                    json!({"key":key,"to":recipient,"body":body}),
                );
            }
            cli::Command::Reply {
                message_id,
                claim_generation,
                source,
                key,
            } => {
                let body =
                    goblins_protocol::messages::load_body(source.message, source.file.as_deref())
                        .map_err(|e| e.to_string())?;
                let key =
                    goblins_protocol::messages::operation_key(key).map_err(|e| e.to_string())?;
                return mailbox_command(
                    "messages.reply",
                    json!({"key":key,"message":message_id,"claim_generation":claim_generation,"body":body}),
                );
            }
            cli::Command::Inbox { command } => {
                let (method, params) = command.request().map_err(|e| e.to_string())?;
                return mailbox_command(method, params);
            }
            cli::Command::RequestPackage { .. } => (),
            cli::Command::EnableDocker { reason } => {
                return request_permission(
                    json!({"kind":"docker", "reason":reason.unwrap_or_else(|| "Enable Docker in this sandbox".into())}),
                );
            }
            cli::Command::Devshell { command } => return devshell(command),
            cli::Command::Flake {
                command: cli::FlakeCommand::Run { installable, args },
            } => return flake_run(installable.as_deref(), args),
            cli::Command::Completions { shell } => {
                cli::completions(shell);
                return Ok(0);
            }
            cli::Command::Complete { words } => return cli::complete(words),
            cli::Command::Status => {
                let record = call("sessions.status", json!({}))?;
                println!(
                    "Sandbox: {}\nConfiguration: {}\nDescription: {}\nState: {}\nScope: {}\nDocker: {}\nDev shell: {}\nID: {}",
                    record["agent_name"].as_str().unwrap_or("unknown"),
                    record["name"].as_str().unwrap_or("unknown"),
                    record["description"].as_str().unwrap_or("unknown"),
                    record["state"].as_str().unwrap_or("unknown"),
                    record["scope"].as_str().unwrap_or("none"),
                    if record["docker_enabled"] == true {
                        "enabled"
                    } else {
                        "disabled"
                    },
                    record["dev_shell"].as_str().unwrap_or("none"),
                    record["id"].as_str().unwrap_or("unknown")
                );
                return Ok(0);
            }
            cli::Command::Attach { session } => {
                require_terminal()?;
                let record = call("sessions.get", json!({"session":session}))?;
                return attach(record["id"].as_str().ok_or("missing session ID")?);
            }
            cli::Command::Run {
                config,
                name,
                parent,
                detatched,
            } => {
                if !detatched {
                    require_terminal()?;
                }
                let launch = call(
                    "sessions.start",
                    json!({"key":random_key()?,
                    "name":config,"agent_name":name,"parent":parent,"detached":detatched}),
                )?;
                if detatched {
                    println!("{launch}");
                    return Ok(0);
                }
                return attach(launch["session"].as_str().ok_or("missing session ID")?);
            }
            command => {
                let (method, params) = match command {
                    cli::Command::List => ("sessions.list", json!({})),
                    cli::Command::Kill {
                        session,
                        kill_children,
                    }
                    | cli::Command::Stop {
                        session,
                        kill_children,
                    } => (
                        "sessions.stop",
                        json!({"session":session,"kill_children":kill_children}),
                    ),
                    cli::Command::Detach => ("sessions.detach", json!({})),
                    _ => unreachable!(),
                };
                let (mut socket, _) = connect()?;
                let reply = rpc::exchange(&mut socket, json!(1), method, params)
                    .map_err(|e| e.to_string())?;
                println!("{reply}");
                return Ok(0);
            }
        }
    } else if args.iter().any(|a| a == "--help" || a == "-h") {
        println!("usage: goblins-request package PACKAGE [--reason TEXT]");
        return Ok(0);
    }
    let (package, reason) = parse_args(legacy, &args)?;
    request_permission(json!({"kind":"package","package":package,"reason":reason}))
}
fn request_permission(params: serde_json::Value) -> Result<i32, String> {
    Ok(request(params)?.0)
}
/// Write the sandbox's trusted lock into the workspace unless it is already
/// there. Returns whether the file changed.
fn write_trusted_lock() -> Result<(bool, u64), String> {
    let trusted = call("devshell.lock", json!({}))?;
    let path = std::path::PathBuf::from(trusted["path"].as_str().ok_or("missing lock path")?);
    let generation = trusted["generation"].as_u64().unwrap_or(0);
    let current = std::fs::read(&path)
        .ok()
        .and_then(|b| serde_json::from_slice::<serde_json::Value>(&b).ok());
    if current.as_ref() == Some(&trusted["lock"]) {
        return Ok((false, generation));
    }
    let mut text = serde_json::to_string_pretty(&trusted["lock"]).map_err(|e| e.to_string())?;
    text.push('\n');
    let staged = path.with_file_name(".flake.lock.goblins");
    std::fs::write(&staged, text).map_err(|e| format!("cannot write {}: {e}", staged.display()))?;
    std::fs::rename(&staged, &path)
        .map_err(|e| format!("cannot replace {}: {e}", path.display()))?;
    Ok((true, generation))
}
fn devshell(command: cli::DevshellCommand) -> Result<i32, String> {
    match command {
        cli::DevshellCommand::RestoreLock => {
            let (changed, generation) = write_trusted_lock()?;
            println!(
                "flake.lock {} the trusted lock of dev shell generation {generation}",
                if changed { "restored to" } else { "already is" }
            );
            Ok(0)
        }
        cli::DevshellCommand::Diff { update, lock } => {
            let mut params = json!({});
            if let Some(update) = update {
                params["update"] = json!(update);
            }
            if lock {
                params["lock"] = json!(true);
            }
            match waiting_call("devshell-diff", "devshell.diff", params)? {
                Ok(preview) if preview.is_object() => {
                    print!("{}", preview::render(&preview));
                    Ok(0)
                }
                Ok(_) => Err("daemon returned an invalid preview".into()),
                Err(message) => {
                    eprintln!("goblins: {message}");
                    Ok(1)
                }
            }
        }
        cli::DevshellCommand::Refresh {
            update,
            lock,
            reason,
        } => {
            let mut params = json!({"kind":"devshell","reason":reason
                .unwrap_or_else(|| "Refresh the dev shell from the workspace flake".into())});
            if let Some(update) = update {
                params["update"] = json!(update);
            }
            if lock {
                params["lock"] = json!(true);
            }
            let (code, reply) = request(params)?;
            if code != 0 {
                if let Some(message) = reply["message"].as_str() {
                    eprintln!("{message}");
                }
                return Ok(code);
            }
            let (changed, generation) = write_trusted_lock()?;
            if changed {
                println!("flake.lock updated to the approved lock");
            }
            // Enter the new generation once, like nix develop: its shellHook
            // runs here, inside the sandbox.
            let status = std::process::Command::new("/bin/sh")
                .args(["-c", ". /run/goblins/devshell/env.sh"])
                .status()
                .map_err(|e| e.to_string())?;
            if !status.success() {
                eprintln!("warning: the dev shell's shellHook exited with {status}");
            }
            println!(
                "Dev shell generation {generation} is active. Running processes keep their \
                 old environment: run `source /run/goblins/devshell/env.sh` in Bash (or start \
                 a new shell) to use it. New child sandboxes start in it."
            );
            Ok(0)
        }
    }
}
/// The app named by `[FLAKE]#APP`. Only the dev shell's flake is served, so
/// a named flake must resolve to it; the daemon never evaluates another.
fn app_name(installable: Option<&str>) -> Result<String, String> {
    let installable = installable.unwrap_or(".");
    let (flake, app) = installable.split_once('#').unwrap_or((installable, ""));
    if !flake.is_empty() {
        let trusted = call("devshell.lock", json!({}))?;
        let lock = std::path::Path::new(trusted["path"].as_str().ok_or("missing lock path")?);
        let resolve = |p: &std::path::Path| std::fs::canonicalize(p).ok();
        if flake.contains(':')
            || resolve(std::path::Path::new(flake)).is_none()
            || resolve(std::path::Path::new(flake)) != lock.parent().and_then(resolve)
        {
            return Err(format!(
                "flake run serves only this sandbox's dev shell flake ({}); use #APP",
                lock.parent().unwrap_or(lock).display()
            ));
        }
    }
    Ok(if app.is_empty() { "default" } else { app }.to_string())
}
/// A program path the daemon returned: an existing file in the store, with
/// no `..` or other indirection in the path itself.
fn store_program(value: &serde_json::Value) -> Result<std::path::PathBuf, String> {
    let program = std::path::PathBuf::from(value.as_str().ok_or("missing program")?);
    let valid = program.starts_with("/nix/store/")
        && program
            .components()
            .skip(1)
            .all(|c| matches!(c, std::path::Component::Normal(_)))
        && program.components().count() > 4
        && program.is_file();
    if !valid {
        return Err(format!(
            "daemon returned an invalid program {}",
            program.display()
        ));
    }
    Ok(program)
}
/// One sandbox call the daemon answers from its worker, which can take long;
/// the daemon bounds evaluation. The outer error is a connection or protocol
/// failure; the inner one is the daemon's refusal or failure.
fn waiting_call(
    feature: &str,
    method: &str,
    params: serde_json::Value,
) -> Result<Result<serde_json::Value, String>, String> {
    let (mut socket, init) = connect()?;
    if !init["features"]
        .as_array()
        .is_some_and(|v| v.iter().any(|f| f == feature))
    {
        return Err(format!("daemon does not support {method}"));
    }
    socket
        .set_write_timeout(Some(rpc::TIMEOUT))
        .map_err(|e| e.to_string())?;
    socket
        .write_all(
            &rpc::encode(&rpc::request(json!(1), method, params)).map_err(|e| e.to_string())?,
        )
        .map_err(|e| e.to_string())?;
    let reply = rpc::read(&mut socket).map_err(|e| e.to_string())?;
    rpc::validate_response(&reply, &json!(1)).map_err(|e| e.to_string())?;
    if let Some(error) = reply.get("error") {
        return Ok(Err(error["message"]
            .as_str()
            .unwrap_or("request failed")
            .to_string()));
    }
    Ok(Ok(reply["result"].clone()))
}
fn flake_run(installable: Option<&str>, args: Vec<String>) -> Result<i32, String> {
    let app = app_name(installable)?;
    let result = match waiting_call("flake-run", "flake.run", json!({"app":app}))? {
        Ok(result) => result,
        Err(message) => {
            eprintln!("goblins: {message}");
            return Ok(1);
        }
    };
    let program = store_program(&result["program"])?;
    use std::os::unix::process::CommandExt;
    let error = std::process::Command::new(&program).args(args).exec();
    Err(format!("cannot execute {}: {error}", program.display()))
}
fn request(params: serde_json::Value) -> Result<(i32, serde_json::Value), String> {
    let id = random_key()?;
    let exchange = || -> Result<serde_json::Value, Box<dyn std::error::Error>> {
        let mut socket = UnixStream::connect(REQUEST_SOCKET)?;
        let init = rpc::exchange(&mut socket, json!(0), "initialize", json!({"api":1}))?;
        if init["api"] != 1 || init["role"] != "sandbox" {
            return Err("incompatible daemon".into());
        }
        if params["kind"] == "docker"
            && !init["features"]
                .as_array()
                .is_some_and(|features| features.iter().any(|feature| feature == "docker-enable"))
        {
            return Err("daemon does not support on-demand Docker; start a new sandbox with an updated daemon".into());
        }
        socket.set_write_timeout(Some(rpc::TIMEOUT))?;
        use std::io::Write;
        socket.write_all(&rpc::encode(&rpc::request(
            json!(id),
            "permissions.request",
            params.clone(),
        ))?)?;
        let reply = rpc::read(&mut socket)?;
        rpc::validate_response(&reply, &json!(id))?;
        if let Some(error) = reply.get("error") {
            return Ok(json!({"status":"error","message":error}));
        }
        let result: rpc::PermissionResult = serde_json::from_value(reply["result"].clone())?;
        if !["ready", "denied", "error"].contains(&result.status.as_str())
            || !goblins_protocol::identifier(&result.request)
        {
            return Err("invalid terminal result".into());
        }
        Ok(serde_json::to_value(result)?)
    };
    match exchange() {
        Ok(reply) => {
            println!("{}", serde_json::to_string(&reply).unwrap());
            Ok((if reply["status"] == "ready" { 0 } else { 1 }, reply))
        }
        Err(e) => {
            eprintln!("goblins-request: outcome unknown; do not retry automatically: {e}");
            Ok((2, json!(null)))
        }
    }
}
fn call(method: &str, params: serde_json::Value) -> Result<serde_json::Value, String> {
    let (mut socket, _) = connect()?;
    rpc::exchange(&mut socket, json!(1), method, params).map_err(|e| e.to_string())
}
fn mailbox_command(method: &str, params: serde_json::Value) -> Result<i32, String> {
    if method != "inbox.status" {
        goblins_protocol::messages::Mutation::parse(method, params.clone())?;
    }
    let (mut socket, init) = connect()?;
    if !init["features"]
        .as_array()
        .is_some_and(|v| v.iter().any(|f| f == goblins_protocol::messages::FEATURE))
    {
        return Err("daemon does not support agent inboxes".into());
    }
    let key = params["key"].as_str().map(String::from);
    if let Some(key) = &key {
        eprintln!("{}", json!({"key":key}));
    }
    Ok(goblins_protocol::messages::print_outcome(
        goblins_protocol::messages::exchange(&mut socket, json!(1), method, params),
        key.as_deref(),
    ))
}
fn require_terminal() -> Result<(), String> {
    if unsafe { libc::isatty(0) } != 1 {
        return Err(
            "attachment requires terminal input; use run --detatched for background launches"
                .into(),
        );
    }
    Ok(())
}
static STOP: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
static RESIZE: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(true);
extern "C" fn interrupted(_: i32) {
    STOP.store(true, std::sync::atomic::Ordering::Relaxed);
}
extern "C" fn resized(_: i32) {
    RESIZE.store(true, std::sync::atomic::Ordering::Relaxed);
}
fn attach(session: &str) -> Result<i32, String> {
    unsafe {
        libc::signal(libc::SIGINT, interrupted as *const () as _);
        libc::signal(libc::SIGTERM, interrupted as *const () as _);
        libc::signal(libc::SIGHUP, interrupted as *const () as _);
        libc::signal(libc::SIGWINCH, resized as *const () as _);
        libc::signal(libc::SIGPIPE, libc::SIG_IGN);
    }
    let (mut socket, _) = connect()?;
    let reply = rpc::exchange(
        &mut socket,
        json!(1),
        "sessions.attach",
        json!({"session":session}),
    )
    .map_err(|e| e.to_string())?;
    if reply["attached"] != true {
        return Err("invalid terminal handshake".into());
    }
    goblins_protocol::terminal::relay(
        socket,
        session,
        |method, params| call(method, params).map_err(Into::into),
        &STOP,
        &RESIZE,
    )
    .map_err(|e| e.to_string())
}
fn random_key() -> Result<String, String> {
    let mut random = [0; 16];
    std::fs::File::open("/dev/urandom")
        .and_then(|mut f| f.read_exact(&mut random))
        .map_err(|e| e.to_string())?;
    Ok(random.iter().map(|b| format!("{b:02x}")).collect())
}
fn connect() -> Result<(UnixStream, serde_json::Value), String> {
    let mut socket = UnixStream::connect(REQUEST_SOCKET).map_err(|e| e.to_string())?;
    let init = rpc::exchange(&mut socket, json!(0), "initialize", json!({"api":1}))
        .map_err(|e| e.to_string())?;
    if init["api"] != 1 || init["role"] != "sandbox" {
        return Err("incompatible daemon".into());
    }
    Ok((socket, init))
}
fn main() {
    std::process::exit(match run() {
        Ok(code) => code,
        Err(e) => {
            eprintln!("goblins: {e}");
            if std::env::args()
                .nth(1)
                .is_some_and(|s| matches!(s.as_str(), "send" | "reply" | "inbox"))
            {
                1
            } else {
                2
            }
        }
    });
}

fn integration(command: cli::IntegrationCommand) -> Result<i32, String> {
    use cli::IntegrationCommand::*;
    let key = || goblins_protocol::messages::operation_key(None).map_err(|e| e.to_string());
    let result = match command {
        Register { driver } => call(
            "integration.register",
            json!({"driver":driver,"key":key()?}),
        )?,
        Hook { event, active } => {
            let epoch = std::env::var("GOBLINS_INTEGRATION_EPOCH")
                .map_err(|_| "missing integration epoch")?
                .parse::<u64>()
                .map_err(|_| "invalid integration epoch")?;
            call(
                "integration.check",
                json!({"epoch":epoch,"event":event,"active":active,"key":key()?}),
            )?
        }
        Status => call("integration.status", json!({}))?,
        Watch { epoch, delivery } => {
            let mut failures = 0;
            loop {
                match call(
                    "integration.tick",
                    json!({"epoch":epoch,"delivery":delivery}),
                ) {
                    Ok(result) => {
                        failures = 0;
                        if result["notify"] == true && result["delivery"] == "notification" {
                            println!("{}", goblins_protocol::INTEGRATION_PROMPT);
                            std::io::stdout().flush().map_err(|e| e.to_string())?;
                        }
                    }
                    Err(e) => {
                        failures += 1;
                        if failures == 1 {
                            eprintln!("goblins: inbox notifier unavailable: {e}");
                        }
                        if failures >= 20 {
                            return Err(
                                "inbox notifier stopped; manual inbox commands remain available"
                                    .into(),
                            );
                        }
                    }
                }
                std::thread::sleep(std::time::Duration::from_millis(500));
            }
        }
    };
    println!("{result}");
    Ok(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn request_options_can_precede_or_follow_the_package() {
        for args in [
            ["request-package", "hello", "--reason", "test"],
            ["request-package", "--reason", "test", "hello"],
        ] {
            assert_eq!(
                parse_args(false, &args.map(String::from)).unwrap(),
                ("hello".into(), "test".into())
            );
        }
        assert!(parse_args(false, &["tui".into()]).is_err());
        assert!(parse_args(true, &["package".into(), "hello".into()]).is_ok());
    }
}
