//! Headless daemon entry point, host API frontends and raw terminal client.
//! The controller library never opens the approval terminal.
mod attachment;
mod cli;
mod exec;
mod plain;
mod records;
mod server;
mod tui;
use clap::{CommandFactory, Parser};
use cli::{Cli, Command, ServerCommand};
use goblins_controller::{Result, host::Client};
use std::{
    fs,
    path::PathBuf,
    sync::atomic::{AtomicBool, Ordering},
};
static STOP: AtomicBool = AtomicBool::new(false);
static RESIZE: AtomicBool = AtomicBool::new(true);
extern "C" fn interrupted(_: i32) {
    STOP.store(true, Ordering::Relaxed);
}
extern "C" fn resized(_: i32) {
    RESIZE.store(true, Ordering::Relaxed);
}
fn signals() {
    unsafe {
        libc::signal(libc::SIGINT, interrupted as *const () as _);
        libc::signal(libc::SIGTERM, interrupted as *const () as _);
        libc::signal(libc::SIGHUP, interrupted as *const () as _);
        libc::signal(libc::SIGWINCH, resized as *const () as _);
        libc::signal(libc::SIGPIPE, libc::SIG_IGN);
    }
}
fn execute(cli: Cli) -> Result<i32> {
    let Some(command) = cli.command else {
        Cli::command().print_help()?;
        println!();
        return Ok(0);
    };
    let uid = unsafe { libc::getuid() };
    let root = std::env::var_os("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| format!("/run/user/{uid}").into());
    let completion_default_allowed =
        cli.state_dir.is_some() || std::env::var_os("GOBLINS_HOST_JOB").is_none();
    // A host command (`goblin host run`) works for a goblin of the default
    // server. Tests it runs must name their own server, never reach or stop
    // that one by accident.
    if cli.state_dir.is_none()
        && std::env::var_os("GOBLINS_HOST_JOB").is_some()
        && !matches!(
            command,
            Command::Configurations | Command::Completions { .. } | Command::CompleteNames { .. }
        )
    {
        return Err("inside a Goblins host command (GOBLINS_HOST_JOB), pass --state-dir: the default server runs the goblin that asked for this command".into());
    }
    let state = cli.state_dir.unwrap_or_else(|| {
        if root.is_dir() {
            root.join("goblins")
        } else {
            format!("/tmp/goblins-control-{uid}").into()
        }
    });
    match command {
        Command::Scope { command } => match command {
            cli::ScopeCommand::Clear {
                name,
                all: _,
                dry_run,
            } => {
                goblins_controller::cleanup::clear_scope(
                    cli.runtime.as_deref(),
                    name.as_deref(),
                    dry_run,
                )?;
            }
        },
        Command::Gc { dry_run } => {
            goblins_controller::cleanup::gc(cli.runtime.as_deref(), &state, dry_run)?;
        }
        Command::Send {
            recipient,
            source,
            key,
        } => {
            let body =
                goblins_protocol::messages::load_body(source.message, source.file.as_deref())?;
            let key = goblins_protocol::messages::operation_key(key)?;
            return mailbox_command(
                &state,
                "messages.send",
                serde_json::json!({"key":key,"to":recipient,"body":body}),
            );
        }
        Command::Reply {
            message_id,
            claim_generation,
            source,
            key,
        } => {
            let body =
                goblins_protocol::messages::load_body(source.message, source.file.as_deref())?;
            let key = goblins_protocol::messages::operation_key(key)?;
            return mailbox_command(
                &state,
                "messages.reply",
                serde_json::json!({"key":key,"message":message_id,"claim_generation":claim_generation,"body":body}),
            );
        }
        Command::Inbox { command } => {
            let (method, params) = command.request()?;
            return mailbox_command(&state, method, params);
        }
        Command::Raid { command } => {
            use cli::RaidCommand::*;
            let mut client = Client::connect(&state)?;
            match command {
                Invite { raid, goblins } => println!(
                    "{}",
                    client.call(
                        "raids.invite",
                        serde_json::json!({"raid":raid,"sessions":goblins})
                    )?
                ),
                Destroy { raid, stop_members } => println!(
                    "{}",
                    client.call(
                        "raids.destroy",
                        serde_json::json!({"raid":raid,"stop_members":stop_members})
                    )?
                ),
                SetOwner { raid, goblin } => println!(
                    "{}",
                    client.call(
                        "raids.set_owner",
                        serde_json::json!({"raid":raid,"session":goblin})
                    )?
                ),
                Members { raid, json } => {
                    let record = client.call("raids.members", serde_json::json!({"raid":raid}))?;
                    if json {
                        println!("{record}");
                    } else {
                        print!("{}", goblins_protocol::raid_members(&record));
                    }
                }
                SetRole { raid, goblin, role } => println!(
                    "{}",
                    client.call(
                        "raids.set_role",
                        serde_json::json!({"raid":raid,"session":goblin,"role":role})
                    )?
                ),
            }
        }
        Command::CommunicationsLog {
            log,
            session,
            follow,
            json,
        } => {
            if let Some(path) = log {
                for event in goblins_controller::messaging::offline_log(&path, session)? {
                    print_event(&event, json)?;
                }
                return Ok(0);
            }
            let mut session = session;
            let mut cursor = 0;
            let mut instance = None;
            loop {
                let mut client = Client::connect(&state)?;
                if instance.as_ref().is_some_and(|id| *id != client.instance) {
                    return Err(
                        "daemon instance changed; choose the intended log explicitly".into(),
                    );
                }
                instance = Some(client.instance.clone());
                let page = client.call(
                    "communications.list",
                    serde_json::json!({"session":session,"after":cursor}),
                )?;
                session = page["session"].as_str().map(String::from);
                for event in page["events"]
                    .as_array()
                    .ok_or("invalid communications page")?
                {
                    print_event(event, json)?;
                }
                cursor = page["cursor"]
                    .as_u64()
                    .ok_or("invalid communications cursor")?;
                let caught_up = cursor
                    >= page["latest"]
                        .as_u64()
                        .ok_or("invalid communications sequence")?;
                if STOP.load(Ordering::Relaxed) || (caught_up && !follow) {
                    break;
                }
                if caught_up {
                    std::thread::sleep(std::time::Duration::from_millis(250));
                }
            }
        }
        Command::Server { command } => match command {
            ServerCommand::Start {
                workspace,
                foreground,
                keep_sessions,
            } => {
                // The manifest the wrapper passes is the first current one
                // raid owners run members from.
                let runtime = cli.runtime.as_deref().map(fs::canonicalize).transpose()?;
                if foreground {
                    server::foreground(&state, workspace, keep_sessions, runtime.as_deref())?;
                } else {
                    server::start(
                        &state,
                        workspace.as_deref(),
                        keep_sessions,
                        runtime.as_deref(),
                    )?;
                }
            }
            ServerCommand::Stop => server::stop(&state)?,
            ServerCommand::Status => return server::status(&state),
            ServerCommand::Logs => server::logs(&state)?,
        },
        Command::Tui { plain } => {
            if plain {
                plain::serve(state)?;
            } else {
                tui::serve(state)?;
            }
        }
        Command::List => println!(
            "{}",
            Client::connect(&state)?.call("sessions.list", serde_json::json!({}))?
        ),
        Command::Stop { id_or_name } | Command::Kill { id_or_name } => println!(
            "{}",
            Client::connect(&state)?
                .call("sessions.stop", serde_json::json!({"session":id_or_name}))?
        ),
        Command::Detach { id_or_name } => println!(
            "{}",
            Client::connect(&state)?
                .call("sessions.detach", serde_json::json!({"session":id_or_name}))?
        ),
        Command::Rpc {
            method,
            params_json,
        } => println!(
            "{}",
            Client::connect(&state)?.call(&method, serde_json::from_str(&params_json)?)?
        ),
        Command::Run {
            config,
            name,
            scope,
            dev_shell,
            detatched,
            raid,
        } => {
            let runtime = cli
                .runtime
                .ok_or("run requires --runtime (use the Nix-built goblins command)")?;
            let configuration = fs::canonicalize(&runtime)?.display().to_string();
            let manifest = goblins_controller::config::read(&runtime)?;
            if !manifest.goblins.contains_key(&config) {
                return Err(format!(
                    "unknown configuration '{config}'; available: {}",
                    manifest
                        .goblins
                        .keys()
                        .cloned()
                        .collect::<Vec<_>>()
                        .join(", ")
                )
                .into());
            }
            // Report an unknown or disallowed scope before starting.
            goblins_controller::config::select(manifest, &config, scope.as_deref())?;
            // Resolve a relative flake path against the caller's directory.
            let dev_shell = dev_shell
                .map(|reference| -> goblins_controller::Result<String> {
                    let (path, attr) = reference
                        .split_once('#')
                        .map_or((reference.as_str(), None), |(p, a)| (p, Some(a)));
                    let path = fs::canonicalize(path)
                        .map_err(|e| format!("dev shell flake {path}: {e}"))?;
                    let reference = match attr {
                        Some(attr) => format!("{}#{attr}", path.display()),
                        None => path.display().to_string(),
                    };
                    goblins_controller::devshell::parse_reference(&reference)?;
                    Ok(reference)
                })
                .transpose()?;
            return attachment::run(
                state,
                attachment::Start {
                    name: config,
                    configuration,
                    agent_name: name,
                    scope,
                    dev_shell,
                    raid,
                },
                detatched,
            );
        }
        Command::Resume { id, detached } => {
            let runtime = cli
                .runtime
                .ok_or("resume requires --runtime (use the Nix-built goblins command)")?;
            let configuration = fs::canonicalize(&runtime)?.display().to_string();
            return attachment::resume(state, configuration, id, detached);
        }
        Command::History {
            command:
                Some(cli::HistoryCommand::Prune {
                    older_than,
                    before,
                    keep,
                    dry_run,
                }),
            ..
        } => {
            return records::prune(
                &state,
                goblins_controller::history::Prune {
                    older_than,
                    before,
                    keep,
                    dry_run,
                },
            );
        }
        Command::History {
            json,
            no_pager,
            command: None,
        } => return records::list(&state, json, !no_pager),
        Command::Configurations => {
            let runtime = cli
                .runtime
                .ok_or("configurations requires --runtime (use the Nix-built goblins command)")?;
            let manifest = goblins_controller::config::read(&runtime)?;
            // Configurations with allowNix use Nix rather than a Goblins dev
            // shell, so frontends don't offer one for them.
            let mut nix = Vec::new();
            for (name, config) in &manifest.goblins {
                if goblins_controller::config::allows_nix(config)? {
                    nix.push(name);
                }
            }
            println!(
                "{}",
                serde_json::json!({
                    "configuration": fs::canonicalize(runtime)?,
                    "names": manifest.goblins.keys().collect::<Vec<_>>(),
                    "descriptions": manifest.goblins.iter().map(|(name, config)| {
                        (name.clone(), config.description.clone())
                    }).collect::<std::collections::BTreeMap<_, _>>(),
                    "nix": nix,
                })
            );
        }
        Command::Exec {
            tty,
            env,
            workdir,
            instance,
            session,
            goblin,
            command,
        } => {
            return exec::run(
                state,
                exec::Options {
                    goblin,
                    command,
                    tty,
                    env,
                    workdir,
                    instance,
                    session,
                },
            );
        }
        Command::Attach { session, instance } => {
            return attachment::attach(state, session, instance);
        }
        Command::Completions { shell } => cli::completions(shell, cli.runtime.as_deref())?,
        Command::CompleteNames { words } => {
            return Ok(cli::complete_names(
                completion_default_allowed.then_some(state.as_path()),
                words,
            ));
        }
    }
    Ok(0)
}
fn main() {
    goblins_controller::unix::raise_descriptor_limit();
    signals();
    let code = match execute(Cli::parse()) {
        Ok(code) => code,
        Err(e) => {
            eprintln!("goblins: {e}");
            1
        }
    };
    std::process::exit(code);
}

fn mailbox_command(
    state: &std::path::Path,
    method: &str,
    params: serde_json::Value,
) -> Result<i32> {
    if method != "inbox.status" {
        goblins_protocol::messages::Mutation::parse(method, params.clone())?;
    }
    let mut client = Client::connect(state)?;
    if !client
        .features
        .iter()
        .any(|f| f == goblins_protocol::messages::FEATURE)
    {
        return Err("daemon does not support agent inboxes".into());
    }
    let key = params["key"].as_str().map(String::from);
    if let Some(key) = &key {
        eprintln!("{}", serde_json::json!({"key":key}));
    }
    let result = client.mailbox_call(method, params);
    Ok(goblins_protocol::messages::print_outcome(
        result,
        key.as_deref(),
    ))
}

fn print_event(event: &serde_json::Value, json: bool) -> Result<()> {
    if json {
        println!("{event}");
    } else {
        println!(
            "{} {} {} {}",
            event["sequence"],
            event["kind"].as_str().unwrap_or("unknown"),
            plain::escaped_json(&event["actor"])?,
            plain::escaped_json(&event["data"])?
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    #[test]
    fn terminal_text_is_ascii_json_data() {
        let text = "\x1b[2J\n approve\u{009b}\u{202e}🐟";
        let shown = super::plain::escaped_json(&text).unwrap();
        assert!(shown.is_ascii());
        assert!(!shown.contains('\x1b'));
        assert_eq!(serde_json::from_str::<String>(&shown).unwrap(), text);
    }
}
