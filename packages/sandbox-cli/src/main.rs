//! Untrusted request client. No controller or helper dependency.
use goblins_protocol::contract::{self, Method};
mod cli;
mod host;
mod init;
mod preview;
mod terminal;
mod worktree;
use clap::Parser;
use goblins_jsonrpc as rpc;
use goblins_protocol::REQUEST_SOCKET;
use serde_json::json;
use std::{
    io::{Read, Write},
    os::unix::net::UnixStream,
};

fn parse_args(legacy: bool, args: &[String]) -> Result<(String, String), String> {
    let mut args = args.iter();
    let kind = if legacy { "package" } else { "request-package" };
    if args.next().map(String::as_str) != Some(kind) {
        return Err("usage: goblin request-package PACKAGE [--reason TEXT]".into());
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
    let name = args.next().unwrap_or_default();
    let name = std::path::Path::new(&name)
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or_default()
        .to_string();
    let legacy = name == "goblins-request";
    let args: Vec<_> = args.collect();
    match name.as_str() {
        "goblins-init" => return init::run(&args),
        "goblins-editor" => return editor_open(&args),
        _ => (),
    }
    if !legacy {
        let command =
            cli::Cli::try_parse_from(std::iter::once("goblin".to_string()).chain(args.clone()))
                .unwrap_or_else(|e| e.exit())
                .command;
        match command {
            cli::Command::SetState { state } => {
                println!(
                    "{}",
                    json!(call_typed::<contract::sandbox::SessionsSetState>(
                        contract::SetStateParams {
                            state: state
                                .parse::<contract::AgentState>()
                                .map_err(|e| e.to_string())?
                        }
                    )?)
                );
                return Ok(0);
            }
            cli::Command::Variable {
                command: cli::VariableCommand::Set { assignments },
            } => {
                let set = variables(&assignments)?;
                println!(
                    "{}",
                    json!(call_typed::<contract::sandbox::SessionsSetVariables>(set)?)
                );
                return Ok(0);
            }
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
                return request_permission(permission_params(
                    "docker",
                    reason.unwrap_or_else(|| "Enable Docker in this sandbox".into()),
                ));
            }
            cli::Command::Devshell { command } => return devshell(command),
            cli::Command::Host { command } => return host::command(command),
            cli::Command::Terminal { command } => return terminal::run(command),
            cli::Command::Editor {
                command: cli::EditorCommand::Open { args },
            } => return editor_open(&args),
            cli::Command::Flake {
                command: cli::FlakeCommand::Run { installable, args },
            } => return flake_run(installable.as_deref(), args),
            cli::Command::Completions { shell } => {
                cli::completions(shell);
                return Ok(0);
            }
            cli::Command::Complete { words } => return cli::complete(words),
            cli::Command::Raid { command } => return raid(command),
            cli::Command::Worktree { name, base, main } => {
                return worktree::run(name, base, main);
            }
            cli::Command::Targets => {
                print!(
                    "{}",
                    targets(&call_typed::<contract::sandbox::TargetsList>(
                        contract::EmptyParams {}
                    )?)
                );
                return Ok(0);
            }
            cli::Command::List => {
                let (mut socket, _) = connect()?;
                let (_, wire) = contract::exchange_with_wire::<contract::sandbox::SessionsList>(
                    &mut socket,
                    json!(1),
                    contract::EmptyParams {},
                )
                .map_err(|e| e.to_string())?;
                println!("{wire}");
                return Ok(0);
            }
            cli::Command::Status => {
                let record =
                    call_typed::<contract::sandbox::SessionsStatus>(contract::EmptyParams {})?;
                let raid = record.raid.as_ref();
                println!(
                    "Sandbox: {}\nConfiguration: {}\nDescription: {}\nRun targets: {}\nLaunch options: {}\nState: {}\nActivity: {}\nAttached: {}\nScope: {}\nDocker: {}\nNix: {}\nHost commands: {}{}\nDev shell: {}\nRaid: {}\nID: {}",
                    record.agent_name.as_deref().unwrap_or("unknown"),
                    record.name.as_deref().unwrap_or("unknown"),
                    record.description.as_deref().unwrap_or("unknown"),
                    match record.allowed_run_targets.as_ref() {
                        Some(names) if !names.is_empty() => names
                            .iter()
                            .map(String::as_str)
                            .collect::<Vec<_>>()
                            .join(", "),
                        Some(_) => "none".into(),
                        None => "unknown".into(),
                    },
                    record
                        .options
                        .as_ref()
                        .filter(|o| !o.is_empty())
                        .map(|o| o
                            .iter()
                            .map(|(k, v)| goblins_protocol::plain_text(&format!("{k}={v}")))
                            .collect::<Vec<_>>()
                            .join(", "))
                        .unwrap_or("none".into()),
                    record
                        .state
                        .map(|v| v.to_string())
                        .unwrap_or("unknown".into()),
                    record
                        .agent_state
                        .map(|v| v.to_string())
                        .unwrap_or("unknown".into()),
                    match (record.attached, record.attached_since) {
                        (Some(true), Some(since)) => {
                            let now = std::time::SystemTime::now()
                                .duration_since(std::time::UNIX_EPOCH)
                                .map_or(0, |d| d.as_millis() as u64);
                            format!("yes, for {}s", now.saturating_sub(since) / 1000)
                        }
                        (Some(true), None) => "yes".into(),
                        (Some(false), _) => "no".into(),
                        (None, _) => "unknown".into(),
                    },
                    record.scope.as_deref().unwrap_or("none"),
                    if record.docker_enabled == Some(true) {
                        "enabled"
                    } else {
                        "disabled"
                    },
                    // Ordinary nix commands against the host daemon.
                    if record.nix == Some(true) {
                        "available"
                    } else {
                        "none"
                    },
                    if record.host_commands == Some(true) {
                        "available after approval"
                    } else {
                        "none"
                    },
                    // What the configuration asks, not what the agent's own
                    // settings make of it; no line for a goblin without an agent.
                    match record.permission_bypass {
                        Some(true) => "\nAgent permission bypass: requested",
                        Some(false) => "\nAgent permission bypass: not requested",
                        None => "",
                    },
                    record.dev_shell.as_deref().unwrap_or("none"),
                    match raid.and_then(|r| r.name.as_deref()) {
                        Some(name) if raid.and_then(|r| r.paused) == Some(true) => format!(
                            "{name} (role: {}, paused until the host selects a new owner)",
                            raid.and_then(|r| r.role.as_deref()).unwrap_or("none"),
                        ),
                        Some(name) => format!(
                            "{name} (role: {}, owner: {})",
                            raid.and_then(|r| r.role.as_deref()).unwrap_or("none"),
                            raid.and_then(|r| r.owner_name.as_deref().or(r.owner.as_deref()))
                                .unwrap_or("unknown")
                        ),
                        None => "none".into(),
                    },
                    record.id.as_deref().unwrap_or("unknown")
                );
                return Ok(0);
            }
            // Goblins do not attach to each other's terminals: a member always
            // starts detached, and the host can attach to it.
            cli::Command::Run {
                config,
                name,
                worktree,
                options,
                ..
            } => {
                let params = contract::MemberLaunchParams {
                    key: random_key()?,
                    name: config,
                    agent_name: name,
                    detached: true,
                    worktree,
                    options: launch_options(&options)?,
                };
                println!(
                    "{}",
                    call_typed_with_wire::<contract::sandbox::SessionsStart>(params)?.1
                );
                return Ok(0);
            }
            cli::Command::Kill { session } | cli::Command::Stop { session } => {
                println!(
                    "{}",
                    call_typed_with_wire::<contract::sandbox::SessionsStop>(
                        contract::SessionSelectorParams { session }
                    )?
                    .1
                );
                return Ok(0);
            }
            cli::Command::Detach => {
                println!(
                    "{}",
                    call_typed_with_wire::<contract::sandbox::SessionsDetach>(
                        contract::EmptyParams {}
                    )?
                    .1
                );
                return Ok(0);
            }
        }
    } else if args.iter().any(|a| a == "--help" || a == "-h") {
        println!("usage: goblins-request package PACKAGE [--reason TEXT]");
        return Ok(0);
    }
    let (package, reason) = parse_args(legacy, &args)?;
    request_permission(contract::PermissionParams {
        package,
        ..permission_params("package", reason)
    })
}
/// `KEY=VALUE` arguments, split at the first `=`.
fn variables(assignments: &[String]) -> Result<goblins_protocol::SetVariables, String> {
    let mut set = goblins_protocol::SetVariables {
        variables: Default::default(),
    };
    for assignment in assignments {
        let (key, value) = assignment
            .split_once('=')
            .ok_or_else(|| format!("expected KEY=VALUE, got '{assignment}'"))?;
        if set.variables.insert(key.into(), value.into()).is_some() {
            return Err(format!("variable '{key}' is given twice"));
        }
    }
    set.validate()?;
    Ok(set)
}
fn raid(command: cli::RaidCommand) -> Result<i32, String> {
    use cli::RaidCommand::*;
    let wire = match command {
        Create { raid } => {
            call_typed_with_wire::<contract::sandbox::RaidsCreate>(contract::RaidSelectorParams {
                raid,
            })?
            .1
        }
        Leave => call_typed_with_wire::<contract::sandbox::RaidsLeave>(contract::EmptyParams {})?.1,
        SetRole { role } => {
            call_typed_with_wire::<contract::sandbox::RaidsSetRole>(
                contract::SandboxRaidRoleParams { role },
            )?
            .1
        }
        Members { json } => {
            let (reply, wire) =
                call_typed_with_wire::<contract::sandbox::RaidsMembers>(contract::EmptyParams {})?;
            if json {
                println!("{wire}");
            } else {
                print!("{}", goblins_protocol::raid_members_typed(&reply));
            }
            return Ok(0);
        }
    };
    println!("{wire}");
    Ok(0)
}
fn request_permission(params: contract::PermissionParams) -> Result<i32, String> {
    Ok(request(params)?.0)
}
/// Write the sandbox's trusted lock into the workspace unless it is already
/// there. Returns whether the file changed.
fn write_trusted_lock() -> Result<(bool, u64), String> {
    let trusted = call_typed::<contract::sandbox::DevShellLock>(contract::EmptyParams {})?;
    let path = std::path::PathBuf::from(trusted.path.as_deref().ok_or("missing lock path")?);
    let generation = trusted.generation;
    let current = std::fs::read(&path)
        .ok()
        .and_then(|b| serde_json::from_slice::<serde_json::Value>(&b).ok());
    if current.as_ref() == Some(&trusted.lock) {
        return Ok((false, generation));
    }
    let mut text = serde_json::to_string_pretty(&trusted.lock).map_err(|e| e.to_string())?;
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
            match waiting_call_typed::<contract::sandbox::DevShellDiff>(
                "devshell-diff",
                contract::DevShellDiffParams { update, lock },
            )? {
                Ok(preview) => {
                    print!("{}", preview::render(&preview));
                    Ok(0)
                }
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
            let params = contract::PermissionParams {
                update,
                lock,
                ..permission_params(
                    "devshell",
                    reason
                        .unwrap_or_else(|| "Refresh the dev shell from the workspace flake".into()),
                )
            };
            let (code, reply) = request(params)?;
            if code != 0 {
                if let Some(message) = reply.message.as_deref() {
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
                 a new shell) to use it. Goblins this one runs start in it."
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
        let trusted = call_typed::<contract::sandbox::DevShellLock>(contract::EmptyParams {})?;
        let lock = std::path::Path::new(trusted.path.as_deref().ok_or("missing lock path")?);
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
fn store_program(value: Option<&str>) -> Result<std::path::PathBuf, String> {
    let program = std::path::PathBuf::from(value.ok_or("missing program")?);
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
    if !init.features.iter().any(|candidate| candidate == feature) {
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
/// Patient exchange bound to the generated method and normalized reply DTO.
fn waiting_call_typed<M: contract::Method>(
    feature: &str,
    params: M::Params,
) -> Result<Result<M::Result, String>, String> {
    let result = waiting_call(
        feature,
        M::NAME,
        contract::params_value::<M>(params).map_err(|e| e.to_string())?,
    )?;
    match result {
        Ok(value) => Ok(Ok(
            <M::Result as contract::Response>::decode(value).map_err(|e| e.to_string())?
        )),
        Err(message) => Ok(Err(message)),
    }
}
/// `goblin editor open`: ask the host editor to open sandbox files, by
/// default waiting until it is done with all of them.
fn editor_open(args: &[String]) -> Result<i32, String> {
    use goblins_protocol::editor;
    let (mut files, wait) = editor::parse(args)?;
    let cwd = std::env::current_dir().map_err(|e| e.to_string())?;
    for file in &mut files {
        file.path = editor::absolute(&cwd, std::path::Path::new(&file.path))
            .to_str()
            .ok_or("file names must be UTF-8")?
            .to_string();
    }
    let open = editor::Open { files, wait };
    open.validate()?;
    match waiting_call_typed::<contract::sandbox::EditorOpen>(editor::FEATURE, open)? {
        Ok(result) => {
            if let Some(message) = result.message.as_deref() {
                eprintln!("goblins: {}", goblins_protocol::plain_text(message));
            }
            Ok(result.code.map_or(1, |c| c as i32))
        }
        Err(message) => {
            eprintln!("goblins: {message}");
            Ok(1)
        }
    }
}
/// `goblin run -o NAME=CHOICE`, each option at most once. The daemon checks
/// them against the configuration.
fn launch_options(
    options: &[String],
) -> Result<std::collections::BTreeMap<String, String>, String> {
    let mut chosen = std::collections::BTreeMap::new();
    for option in options {
        let Some((name, choice)) = option
            .split_once('=')
            .filter(|(n, c)| !n.is_empty() && !c.is_empty())
        else {
            return Err(format!(
                "-o {option}: expected NAME=CHOICE (see goblin targets)"
            ));
        };
        if chosen.insert(name.into(), choice.into()).is_some() {
            return Err(format!("-o {name} is given more than once"));
        }
    }
    Ok(chosen)
}
/// `goblin targets`: each configuration, then its options and choices.
fn targets(list: &contract::RunTargetListReply) -> String {
    let text = goblins_protocol::plain_text;
    let mut out = String::new();
    if list.0.is_empty() {
        out.push_str("No run targets\n");
    }
    let width = list
        .0
        .iter()
        .filter_map(|t| t.options.as_ref())
        .flat_map(|o| o.keys().map(|k| k.chars().count()))
        .max()
        .unwrap_or(0);
    for target in &list.0 {
        out.push_str(
            format!(
                "{}  {}",
                text(&target.name),
                text(target.description.as_deref().unwrap_or(""))
            )
            .trim_end(),
        );
        out.push('\n');
        for (option, declared) in target.options.iter().flatten() {
            let choices: Vec<_> = declared
                .choices
                .iter()
                .flatten()
                .map(|(c, _)| text(c))
                .collect();
            let line = format!(
                "  {:width$}  {}  {}",
                text(option),
                choices.join(" | "),
                text(declared.description.as_deref().unwrap_or(""))
            );
            out.push_str(line.trim_end());
            out.push('\n');
        }
    }
    out
}
fn flake_run(installable: Option<&str>, args: Vec<String>) -> Result<i32, String> {
    let app = app_name(installable)?;
    let result = match waiting_call_typed::<contract::sandbox::FlakeRun>(
        "flake-run",
        contract::FlakeRunParams { app },
    )? {
        Ok(result) => result,
        Err(message) => {
            eprintln!("goblins: {message}");
            return Ok(1);
        }
    };
    let program = store_program(result.program.as_deref())?;
    use std::os::unix::process::CommandExt;
    let error = std::process::Command::new(&program).args(args).exec();
    Err(format!("cannot execute {}: {error}", program.display()))
}
fn permission_params(kind: &str, reason: String) -> contract::PermissionParams {
    contract::PermissionParams {
        kind: kind.into(),
        package: String::new(),
        reason,
        update: None,
        lock: false,
        host: None,
    }
}
fn request(
    params: contract::PermissionParams,
) -> Result<(i32, contract::PermissionRequestReply), String> {
    let id = random_key()?;
    let exchange = || -> Result<serde_json::Value, Box<dyn std::error::Error>> {
        let mut socket = UnixStream::connect(REQUEST_SOCKET)?;
        let init = contract::initialize(&mut socket, json!(0), contract::EndpointRole::Sandbox)?;
        if params.kind == "docker"
            && !init
                .features
                .iter()
                .any(|feature| feature == "docker-enable")
        {
            return Err("daemon does not support on-demand Docker; start a new sandbox with an updated daemon".into());
        }
        socket.set_write_timeout(Some(rpc::TIMEOUT))?;
        use std::io::Write;
        socket.write_all(&rpc::encode(&rpc::request(
            json!(id),
            contract::sandbox::PermissionsRequest::NAME,
            contract::params_value::<contract::sandbox::PermissionsRequest>(params.clone())?,
        ))?)?;
        let reply = rpc::read(&mut socket)?;
        rpc::validate_response(&reply, &json!(id))?;
        if let Some(error) = reply.get("error") {
            return Ok(json!({"status":"error","message":error}));
        }
        let result = contract::decode_reply_profile::<
            contract::sandbox::PermissionsRequest,
            contract::sandbox_reply_profiles::PermissionsRequestGrant,
        >(reply["result"].clone())?;
        Ok(serde_json::to_value(result)?)
    };
    match exchange() {
        Ok(reply) => {
            println!("{}", serde_json::to_string(&reply).unwrap());
            let normalized = contract::decode_reply::<contract::sandbox::PermissionsRequest>(reply)
                .map_err(|e| e.to_string())?;
            Ok((
                if normalized.status.as_deref() == Some("ready") {
                    0
                } else {
                    1
                },
                normalized,
            ))
        }
        Err(e) => {
            eprintln!("goblins-request: outcome unknown; do not retry automatically: {e}");
            Ok((
                2,
                contract::decode_reply::<contract::sandbox::PermissionsRequest>(json!(null))
                    .map_err(|e| e.to_string())?,
            ))
        }
    }
}
pub(crate) fn call_typed_with_wire<M: contract::Method>(
    params: M::Params,
) -> Result<(M::Result, serde_json::Value), String> {
    let (mut socket, _) = connect()?;
    contract::exchange_with_wire::<M>(&mut socket, json!(1), params).map_err(|e| e.to_string())
}
pub(crate) fn call_typed<M: contract::Method>(params: M::Params) -> Result<M::Result, String> {
    let (mut socket, _) = connect()?;
    contract::exchange::<M>(&mut socket, json!(1), params).map_err(|e| e.to_string())
}
fn mailbox_command(method: &str, params: serde_json::Value) -> Result<i32, String> {
    if method != "inbox.status" {
        goblins_protocol::messages::Mutation::parse(method, params.clone())?;
    }
    let (mut socket, init) = connect()?;
    if !init
        .features
        .iter()
        .any(|feature| feature == goblins_protocol::messages::FEATURE)
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
fn random_key() -> Result<String, String> {
    let mut random = [0; 16];
    std::fs::File::open("/dev/urandom")
        .and_then(|mut f| f.read_exact(&mut random))
        .map_err(|e| e.to_string())?;
    Ok(random.iter().map(|b| format!("{b:02x}")).collect())
}
fn connect() -> Result<(UnixStream, contract::InitializeReply), String> {
    let mut socket = UnixStream::connect(REQUEST_SOCKET).map_err(|e| e.to_string())?;
    let init = contract::initialize(&mut socket, json!(0), contract::EndpointRole::Sandbox)
        .map_err(|e| e.to_string())?;
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

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn launch_options_are_name_choice_pairs_and_targets_list_them() {
        let options =
            |args: &[&str]| launch_options(&args.iter().map(|a| a.to_string()).collect::<Vec<_>>());
        assert_eq!(
            json!(options(&["model=gpt-5.5", "mode=a=b"]).unwrap()),
            json!({"model": "gpt-5.5", "mode": "a=b"})
        );
        for bad in [&["fast"][..], &["=x"], &["model="], &["model=a", "model=b"]] {
            assert!(options(bad).is_err(), "{bad:?}");
        }
        let list = json!([
            {"name": "claude", "description": "Claude Code", "options": {
                "model": {"description": null, "choices": {"opus": {}, "sonnet": {}}},
                "effort": {"description": "Reasoning effort", "choices": {"high": {}}}}},
            {"name": "shell", "description": "", "options": {}}
        ]);
        assert_eq!(
            targets(&<contract::RunTargetListReply as contract::Response>::decode(list).unwrap()),
            "claude  Claude Code\n  effort  high  Reasoning effort\n  model   opus | sonnet\nshell\n"
        );
        assert_eq!(
            targets(
                &<contract::RunTargetListReply as contract::Response>::decode(json!([])).unwrap()
            ),
            "No run targets\n"
        );
    }
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
    #[test]
    fn variables_split_at_the_first_equals_sign() {
        let set = variables(&["session_id=a=b".into(), "session_name=".into()]).unwrap();
        assert_eq!(set.variables["session_id"], "a=b");
        assert_eq!(set.variables["session_name"], "");
        assert!(variables(&["novalue".into()]).is_err());
        assert!(variables(&["a=1".into(), "a=2".into()]).is_err());
        assert!(variables(&["Bad=1".into()]).is_err());
    }
}
