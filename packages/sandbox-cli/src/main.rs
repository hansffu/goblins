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

/// What a command shows for a reply: its stdout, its stderr and its exit
/// code.
#[derive(Debug, Default, PartialEq)]
pub(crate) struct Output {
    pub stdout: String,
    pub stderr: String,
    pub code: i32,
}
impl Output {
    /// Print the output and return its exit code.
    pub fn emit(self) -> i32 {
        print!("{}", self.stdout);
        eprint!("{}", self.stderr);
        self.code
    }
}

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
                    goblins_protocol::contract::sandbox::MessagesSend::NAME,
                    goblins_protocol::messages::send_request(key, recipient, body)
                        .map_err(|e| e.to_string())?,
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
                    goblins_protocol::contract::sandbox::MessagesReply::NAME,
                    serde_json::to_value(goblins_protocol::messages::Reply {
                        key,
                        message: message_id,
                        claim_generation,
                        body,
                    })
                    .map_err(|e| e.to_string())?,
                );
            }
            cli::Command::Inbox { command } => {
                let (method, params) = command.request().map_err(|e| e.to_string())?;
                return mailbox_command(method, params);
            }
            cli::Command::RequestPackage { .. } => (),
            cli::Command::EnableDocker { reason } => {
                return request_permission(docker_params(reason));
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
                let params = launch_params(random_key()?, config, name, worktree, &options)?;
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
    request_permission(package_params(legacy, &args)?)
}
/// The request of `request-package PACKAGE [--reason TEXT]`.
fn package_params(legacy: bool, args: &[String]) -> Result<contract::PermissionParams, String> {
    let (package, reason) = parse_args(legacy, args)?;
    Ok(contract::PermissionParams {
        package,
        ..permission_params("package", reason)
    })
}
/// The request of `enable-docker [--reason TEXT]`.
fn docker_params(reason: Option<String>) -> contract::PermissionParams {
    permission_params(
        "docker",
        reason.unwrap_or_else(|| "Enable Docker in this sandbox".into()),
    )
}
/// The request of `devshell refresh`.
fn refresh_params(
    update: Option<Vec<String>>,
    lock: bool,
    reason: Option<String>,
) -> contract::PermissionParams {
    contract::PermissionParams {
        update,
        lock,
        ..permission_params(
            "devshell",
            reason.unwrap_or_else(|| "Refresh the dev shell from the workspace flake".into()),
        )
    }
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
    install_lock(&call_typed::<contract::sandbox::DevShellLock>(
        contract::EmptyParams {},
    )?)
}
/// Write the trusted lock the daemon returned to its path unless it is
/// already there. Returns whether the file changed.
fn install_lock(trusted: &contract::DevShellLockReply) -> Result<(bool, u64), String> {
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
/// What `devshell restore-lock` prints.
fn restored(changed: bool, generation: u64) -> String {
    format!(
        "flake.lock {} the trusted lock of dev shell generation {generation}\n",
        if changed { "restored to" } else { "already is" }
    )
}
fn devshell(command: cli::DevshellCommand) -> Result<i32, String> {
    match command {
        cli::DevshellCommand::RestoreLock => {
            let (changed, generation) = write_trusted_lock()?;
            print!("{}", restored(changed, generation));
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
            let (code, reply) = request(refresh_params(update, lock, reason))?;
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
    served_app(installable, || {
        let trusted = call_typed::<contract::sandbox::DevShellLock>(contract::EmptyParams {})?;
        Ok(trusted.path.ok_or("missing lock path")?.into())
    })
}
/// `app_name` with `trusted_lock` returning the path of the dev shell's
/// trusted `flake.lock`, asked only when a flake is named.
fn served_app(
    installable: Option<&str>,
    trusted_lock: impl FnOnce() -> Result<std::path::PathBuf, String>,
) -> Result<String, String> {
    let installable = installable.unwrap_or(".");
    let (flake, app) = installable.split_once('#').unwrap_or((installable, ""));
    if !flake.is_empty() {
        let lock = &trusted_lock()?;
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
    let open = editor_request(args, || std::env::current_dir().map_err(|e| e.to_string()))?;
    match waiting_call_typed::<contract::sandbox::EditorOpen>(
        goblins_protocol::editor::FEATURE,
        open,
    )? {
        Ok(result) => Ok(edited(&result).emit()),
        Err(message) => {
            eprintln!("goblins: {message}");
            Ok(1)
        }
    }
}
/// The files to open, their paths relative to `cwd`.
fn editor_request(
    args: &[String],
    cwd: impl FnOnce() -> Result<std::path::PathBuf, String>,
) -> Result<goblins_protocol::editor::Open, String> {
    use goblins_protocol::editor;
    let (mut files, wait) = editor::parse(args)?;
    let cwd = cwd()?;
    for file in &mut files {
        file.path = editor::absolute(&cwd, std::path::Path::new(&file.path))
            .to_str()
            .ok_or("file names must be UTF-8")?
            .to_string();
    }
    let open = editor::Open { files, wait };
    open.validate()?;
    Ok(open)
}
/// What the editor's answer shows: its message, and its code as the exit
/// code (1 when it has none).
fn edited(result: &contract::EditorOpenReply) -> Output {
    Output {
        stderr: result
            .message
            .as_deref()
            .map(|message| format!("goblins: {}\n", goblins_protocol::plain_text(message)))
            .unwrap_or_default(),
        code: result.code.map_or(1, |c| c as i32),
        ..Output::default()
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
/// The request of `goblin run`, under the operation key `key`.
fn launch_params(
    key: String,
    config: String,
    name: Option<String>,
    worktree: Option<String>,
    options: &[String],
) -> Result<contract::MemberLaunchParams, String> {
    Ok(contract::MemberLaunchParams {
        key,
        name: config,
        agent_name: name,
        detached: true,
        worktree,
        options: launch_options(options)?,
    })
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
        grant(&reply)
    };
    let (output, reply) = granted(exchange());
    let code = output.emit();
    Ok((code, reply?))
}
/// The outcome a correlated reply frame to a permission request reports:
/// the daemon's error, or the result read through the grant profile.
fn grant(reply: &serde_json::Value) -> Result<serde_json::Value, Box<dyn std::error::Error>> {
    if let Some(error) = reply.get("error") {
        return Ok(json!({"status":"error","message":error}));
    }
    let result = contract::decode_reply_profile::<
        contract::sandbox::PermissionsRequest,
        contract::sandbox_reply_profiles::PermissionsRequestGrant,
    >(reply["result"].clone())?;
    Ok(serde_json::to_value(result)?)
}
/// What a permission request shows for its outcome: 0 when it is ready, 1
/// when it is not, and 2 when the exchange failed and the outcome is unknown.
fn granted(
    outcome: Result<serde_json::Value, Box<dyn std::error::Error>>,
) -> (Output, Result<contract::PermissionRequestReply, String>) {
    match outcome {
        Ok(reply) => {
            let stdout = format!("{}\n", serde_json::to_string(&reply).unwrap());
            let normalized = contract::decode_reply::<contract::sandbox::PermissionsRequest>(reply)
                .map_err(|e| e.to_string());
            let code = match &normalized {
                Ok(n) if n.status.as_deref() == Some("ready") => 0,
                _ => 1,
            };
            (
                Output {
                    stdout,
                    code,
                    ..Output::default()
                },
                normalized,
            )
        }
        Err(e) => (
            Output {
                stderr: format!(
                    "goblins-request: outcome unknown; do not retry automatically: {e}\n"
                ),
                code: 2,
                ..Output::default()
            },
            contract::decode_reply::<contract::sandbox::PermissionsRequest>(json!(null))
                .map_err(|e| e.to_string()),
        ),
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
fn connect() -> Result<(UnixStream, contract::InitializeResult), String> {
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
    use serde_json::Value;

    fn parse(args: &[&str]) -> cli::Command {
        cli::Cli::try_parse_from(std::iter::once("goblin").chain(args.iter().copied()))
            .unwrap()
            .command
    }
    fn strings(args: &[&str]) -> Vec<String> {
        args.iter().map(|a| a.to_string()).collect()
    }
    fn fixtures() -> Value {
        serde_json::from_str(include_str!("../../../libs/api/tests/fixtures.json")).unwrap()
    }
    /// A fresh directory for one test.
    fn scratch(name: &str) -> std::path::PathBuf {
        let dir =
            std::env::temp_dir().join(format!("goblins-client-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn permission_requests_keep_their_fields() {
        let wire = |params| {
            contract::params_value::<contract::sandbox::PermissionsRequest>(params).unwrap()
        };
        assert_eq!(
            wire(
                package_params(
                    false,
                    &strings(&["request-package", "hello", "--reason", "tests"])
                )
                .unwrap()
            ),
            json!({"kind":"package","package":"hello","reason":"tests"})
        );
        let cli::Command::EnableDocker { reason } = parse(&["enable-docker", "--reason", "tests"])
        else {
            unreachable!()
        };
        assert_eq!(
            wire(docker_params(reason)),
            json!({"kind":"docker","reason":"tests"})
        );
        let cli::Command::Devshell {
            command:
                cli::DevshellCommand::Refresh {
                    update,
                    lock,
                    reason,
                },
        } = parse(&[
            "devshell", "refresh", "--reason", "tests", "--update", "nixpkgs",
        ])
        else {
            unreachable!()
        };
        assert_eq!(
            wire(refresh_params(update, lock, reason)),
            json!({"kind":"devshell","reason":"tests","update":["nixpkgs"]})
        );
    }

    #[test]
    fn permission_outcomes_print_closed_results_and_flag_unknown_ones() {
        let shown = |frame: Value| granted(grant(&frame)).0;
        let result = |result: &Value| json!({"jsonrpc":"2.0","id":"k","result":result});
        let printed = |output: &Output| serde_json::from_str::<Value>(&output.stdout).unwrap();
        let good = json!({"request":"fixture-job-1","status":"ready","message":null});
        let with = |field: &str, value: Value| {
            let mut v = good.clone();
            v[field] = value;
            v
        };
        let denied = with("status", json!("denied"));
        // A refusal prints the reply as it came, and exits 1.
        let refused = json!({"request":"fixture-job-1","status":"denied","message":"no"});
        let output = shown(result(&refused));
        assert_eq!((output.code, printed(&output)), (1, refused));
        for (reply, code, normalized) in [
            (good.clone(), 0, good.clone()),
            (denied.clone(), 1, denied.clone()),
            (
                with("status", json!("error")),
                1,
                with("status", json!("error")),
            ),
        ] {
            let output = shown(result(&reply));
            assert_eq!(
                (output.code, printed(&output), output.stderr.as_str()),
                (code, normalized, ""),
                "{reply}"
            );
            assert!(output.stdout.ends_with('\n'));
        }
        for reply in [
            with("status", json!("failed")),
            with("status", json!("withdrawn")),
            with("extra", json!(true)),
            with("request", json!("bad id")),
            json!({"request":"fixture-job-1","status":"ready"}),
            json!(["fixture-job-1", "ready", null]),
            Value::Null,
            with("message", json!(7)),
        ] {
            let output = shown(result(&reply));
            assert_eq!((output.code, output.stdout.as_str()), (2, ""), "{reply}");
            assert!(
                output
                    .stderr
                    .starts_with("goblins-request: outcome unknown; do not retry automatically: "),
                "{reply}: {}",
                output.stderr
            );
        }
        let error = json!({"code":-32009,"message":"blocked","data":{"preserve":true}});
        let output = shown(json!({"jsonrpc":"2.0","id":"k","error":error}));
        assert_eq!(
            (output.code, printed(&output), output.stderr.as_str()),
            (1, json!({"status":"error","message":error}), "")
        );
    }

    #[test]
    fn editor_open_sends_absolute_paths_and_exits_with_the_editor_code() {
        let cli::Command::Editor {
            command: cli::EditorCommand::Open { args },
        } = parse(&[
            "editor",
            "open",
            "--no-wait",
            "+3:4",
            "../other/./file",
            "/tmp/msg",
            "+9",
            "last",
        ])
        else {
            unreachable!()
        };
        let wire = |open| contract::params_value::<contract::sandbox::EditorOpen>(open).unwrap();
        assert_eq!(contract::sandbox::EditorOpen::NAME, "editor.open");
        assert_eq!(
            wire(editor_request(&args, || Ok("/root/work".into())).unwrap()),
            json!({"files":[{"path":"/root/other/file","line":3,"column":4},{"path":"/tmp/msg"},
                            {"path":"/root/work/last","line":9}],"wait":false})
        );
        assert_eq!(
            wire(editor_request(&strings(&["/tmp/msg"]), || Ok("/root/work".into())).unwrap()),
            json!({"files":[{"path":"/tmp/msg"}],"wait":true})
        );
        for (result, code, stderr) in [
            (Value::Null, 1, ""),
            (json!([]), 1, ""),
            (json!({}), 1, ""),
            (json!({"code":true,"message":1}), 1, ""),
            (json!({"code":1.0}), 1, ""),
            (json!({"code":4294967296i64}), 0, ""),
            // Truncated to -1: the process exits 255.
            (json!({"code":i64::MAX}), -1, ""),
            (
                json!({"code":7,"message":"refused\u{1b}[2J"}),
                7,
                "goblins: refused\\u001b[2J\n",
            ),
        ] {
            let reply =
                contract::decode_reply::<contract::sandbox::EditorOpen>(result.clone()).unwrap();
            assert_eq!(
                edited(&reply),
                Output {
                    stdout: String::new(),
                    stderr: stderr.into(),
                    code
                },
                "{result}"
            );
        }
    }

    #[test]
    fn devshell_diff_sends_lock_and_update_only_when_given() {
        assert_eq!(contract::sandbox::DevShellDiff::NAME, "devshell.diff");
        for (args, params) in [
            (&[][..], json!({})),
            (&["--lock"], json!({"lock":true})),
            (&["--update"], json!({"update":[]})),
            (
                &["--update", "nixpkgs", "a/b"],
                json!({"update":["nixpkgs","a/b"]}),
            ),
        ] {
            let cli::Command::Devshell {
                command: cli::DevshellCommand::Diff { update, lock },
            } = parse(&[&["devshell", "diff"][..], args].concat())
            else {
                unreachable!()
            };
            assert_eq!(
                contract::params_value::<contract::sandbox::DevShellDiff>(
                    contract::DevShellDiffParams { update, lock }
                )
                .unwrap(),
                params,
                "{args:?}"
            );
        }
    }

    #[test]
    fn restore_lock_writes_the_trusted_lock_and_reports_its_generation() {
        assert_eq!(contract::sandbox::DevShellLock::NAME, "devshell.lock");
        assert_eq!(
            contract::params_value::<contract::sandbox::DevShellLock>(contract::EmptyParams {})
                .unwrap(),
            json!({})
        );
        let dir = scratch("lock");
        let path = dir.join("flake.lock");
        let decode =
            |v: Value| contract::decode_reply::<contract::sandbox::DevShellLock>(v).unwrap();
        for (result, generation, lock) in [
            (
                json!({"path":path,"generation":3,"lock":{"nodes":{}}}),
                3,
                json!({"nodes":{}}),
            ),
            (json!({"path":path,"generation":"bad"}), 0, Value::Null),
            (
                json!({"path":path,"generation":u64::MAX,"lock":["opaque"]}),
                u64::MAX,
                json!(["opaque"]),
            ),
        ] {
            let (changed, written) = install_lock(&decode(result.clone())).unwrap();
            assert_eq!((changed, written), (true, generation), "{result}");
            assert!(restored(changed, written).contains(&format!("generation {generation}")));
            assert_eq!(
                serde_json::from_slice::<Value>(&std::fs::read(&path).unwrap()).unwrap(),
                lock
            );
            assert_eq!(
                install_lock(&decode(result)).unwrap(),
                (false, generation),
                "an unchanged lock is left alone"
            );
        }
        assert_eq!(
            restored(false, 2),
            "flake.lock already is the trusted lock of dev shell generation 2\n"
        );
        for result in [Value::Null, json!([]), json!({"path":7})] {
            assert_eq!(
                install_lock(&decode(result.clone())).unwrap_err(),
                "missing lock path",
                "{result}"
            );
        }
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn flake_run_serves_only_the_dev_shell_flake_and_runs_store_programs() {
        let dir = scratch("flake");
        let other = dir.join("other");
        std::fs::create_dir(&other).unwrap();
        let trusted = || Ok(dir.join("flake.lock"));
        let error = served_app(Some(&format!("{}#hello", other.display())), trusted).unwrap_err();
        assert!(
            error.contains("serves only this sandbox's dev shell flake"),
            "{error}"
        );
        assert_eq!(
            served_app(Some(&format!("{}#hello", dir.display())), trusted).unwrap(),
            "hello"
        );
        let unused = || -> Result<_, String> { panic!("#APP needs no trusted lock") };
        assert_eq!(served_app(Some("#hello"), unused).unwrap(), "hello");
        assert_eq!(served_app(Some("#"), unused).unwrap(), "default");
        std::fs::remove_dir_all(dir).unwrap();

        assert_eq!(contract::sandbox::FlakeRun::NAME, "flake.run");
        assert_eq!(
            contract::params_value::<contract::sandbox::FlakeRun>(contract::FlakeRunParams {
                app: "hello".into()
            })
            .unwrap(),
            json!({"app":"hello"})
        );
        let program = |v: Value| {
            store_program(
                contract::decode_reply::<contract::sandbox::FlakeRun>(v)
                    .unwrap()
                    .program
                    .as_deref(),
            )
        };
        for (result, diagnostic) in [
            (Value::Null, "missing program"),
            (json!({}), "missing program"),
            (json!({"program":1}), "missing program"),
            (
                json!({"program":"/tmp/not-a-store-program"}),
                "invalid program",
            ),
        ] {
            let error = program(result.clone()).unwrap_err();
            assert!(error.contains(diagnostic), "{result}: {error}");
        }
        let true_ = std::env::split_paths(&std::env::var_os("PATH").unwrap())
            .map(|dir| dir.join("true"))
            .find(|p| p.is_file())
            .expect("true on PATH");
        assert!(true_.starts_with("/nix/store/"), "{}", true_.display());
        assert_eq!(
            program(json!({"program":true_,"generation":"ignored","extension":true})).unwrap(),
            true_
        );
    }

    #[test]
    fn run_keeps_exact_named_and_unset_request_fields() {
        assert_eq!(contract::sandbox::SessionsStart::NAME, "sessions.start");
        for named in [false, true] {
            let mut args = vec!["run", "shell"];
            if named {
                args.extend([
                    "--name",
                    "named",
                    "--worktree",
                    "branch",
                    "-o",
                    "model=fast",
                ]);
            }
            let cli::Command::Run {
                config,
                name,
                worktree,
                options,
                ..
            } = parse(&args)
            else {
                unreachable!()
            };
            let params = contract::params_value::<contract::sandbox::SessionsStart>(
                launch_params("k".into(), config, name, worktree, &options).unwrap(),
            )
            .unwrap();
            let mut expected = json!({"key":"k","name":"shell","agent_name":null,"detached":true,
                                      "options":{}});
            if named {
                expected = json!({"key":"k","name":"shell","agent_name":"named","detached":true,
                                  "options":{"model":"fast"},"worktree":"branch"});
            }
            assert_eq!(params, expected, "{args:?}");
        }
        let reply = fixtures()["forward_compatibility"]["member_launch"].clone();
        contract::decode_reply::<contract::sandbox::SessionsStart>(reply).unwrap();
    }

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
    fn flake_run_serves_only_the_dev_shell_flake() {
        let flake = std::env::temp_dir().join(format!("goblins-flake-run-{}", std::process::id()));
        std::fs::create_dir_all(&flake).unwrap();
        let lock = || Ok(flake.join("flake.lock"));
        let refused = format!(
            "flake run serves only this sandbox's dev shell flake ({}); use #APP",
            flake.display()
        );
        for installable in [
            "/tmp#greet",
            "github:NixOS/nixpkgs#hello",
            "/nonexistent#greet",
        ] {
            assert_eq!(
                served_app(Some(installable), lock),
                Err(refused.clone()),
                "{installable}"
            );
        }
        assert_eq!(
            served_app(Some(&format!("{}#greet", flake.display())), lock).unwrap(),
            "greet"
        );
        assert_eq!(
            served_app(Some(&format!("{}/.", flake.display())), lock).unwrap(),
            "default"
        );
        // Without a flake, the dev shell's own is meant and nothing is asked.
        let unasked = || -> Result<std::path::PathBuf, String> { panic!("asked for the lock") };
        assert_eq!(served_app(Some("#greet"), unasked).unwrap(), "greet");
        std::fs::remove_dir_all(flake).unwrap();
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
