//! Untrusted request client. No controller or helper dependency.
mod cli;
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
            cli::Command::EnableDocker {
                reason,
                scope,
                anonymous,
            } => {
                let mut params = json!({"kind":"docker", "reason":reason.unwrap_or_else(|| "Enable Docker in this sandbox".into())});
                if let Some(scope) = scope {
                    params["scope"] = json!(scope);
                }
                if anonymous {
                    params["anonymous"] = json!(true);
                }
                return request_permission(params);
            }
            cli::Command::Completions { shell } => {
                cli::completions(shell);
                return Ok(0);
            }
            cli::Command::Complete { words } => return cli::complete(words),
            cli::Command::Status => {
                let record = call("sessions.status", json!({}))?;
                println!(
                    "Sandbox: {}\nConfiguration: {}\nDescription: {}\nState: {}\nDocker: {}\nDocker scope: {}\nID: {}",
                    record["agent_name"].as_str().unwrap_or("unknown"),
                    record["name"].as_str().unwrap_or("unknown"),
                    record["description"].as_str().unwrap_or("unknown"),
                    record["state"].as_str().unwrap_or("unknown"),
                    if record["docker_enabled"] == true {
                        "enabled"
                    } else {
                        "disabled"
                    },
                    record["docker_scope"].as_str().unwrap_or("anonymous"),
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
        if (params["scope"].is_string() || params["anonymous"] == true)
            && !init["features"]
                .as_array()
                .is_some_and(|features| features.iter().any(|feature| feature == "docker-scopes"))
        {
            return Err(
                "daemon does not support Docker scopes; start a new sandbox with an updated daemon"
                    .into(),
            );
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
            Ok(if reply["status"] == "ready" { 0 } else { 1 })
        }
        Err(e) => {
            eprintln!("goblins-request: outcome unknown; do not retry automatically: {e}");
            Ok(2)
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
