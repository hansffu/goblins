//! Headless daemon entry point, host API frontends and raw terminal client.
//! The controller library never opens the approval terminal.
mod attachment;
mod cli;
mod plain;
mod server;
mod tui;
use clap::{CommandFactory, Parser};
use cli::{Cli, Command, ServerCommand};
use goblins_controller::{Result, config::Manifest, host::Client};
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
    let state = cli.state_dir.unwrap_or_else(|| {
        if root.is_dir() {
            root.join("goblins")
        } else {
            format!("/tmp/goblins-control-{uid}").into()
        }
    });
    match command {
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
        Command::Integration { command } => {
            use cli::IntegrationCommand::*;
            let (method, session) = match command {
                Status { session } => ("integration.status", session),
                Pause { session } => ("integration.pause", session),
                Resume { session } => ("integration.resume", session),
            };
            let mut client = Client::connect(&state)?;
            println!(
                "{}",
                client.call(method, serde_json::json!({"session":session}))?
            );
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
            } => {
                if foreground {
                    server::foreground(&state, workspace)?;
                } else {
                    server::start(&state, workspace.as_deref())?;
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
        Command::Stop {
            id_or_name,
            kill_children,
        }
        | Command::Kill {
            id_or_name,
            kill_children,
        } => println!(
            "{}",
            Client::connect(&state)?.call(
                "sessions.stop",
                serde_json::json!({"session":id_or_name,"kill_children":kill_children})
            )?
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
            parent,
            detatched,
        } => {
            let runtime = cli
                .runtime
                .ok_or("run requires --runtime (use the Nix-built goblins command)")?;
            let configuration = fs::canonicalize(&runtime)?.display().to_string();
            let manifest = Manifest::read(&runtime)?;
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
            return attachment::run(state, config, configuration, name, parent, detatched);
        }
        Command::Configurations => {
            let runtime = cli
                .runtime
                .ok_or("configurations requires --runtime (use the Nix-built goblins command)")?;
            let manifest = Manifest::read(&runtime)?;
            println!(
                "{}",
                serde_json::json!({
                    "configuration": fs::canonicalize(runtime)?,
                    "names": manifest.goblins.keys().collect::<Vec<_>>(),
                    "descriptions": manifest.goblins.iter().map(|(name, config)| {
                        (name.clone(), config.description.clone())
                    }).collect::<std::collections::BTreeMap<_, _>>()
                })
            );
        }
        Command::Attach { session, instance } => {
            return attachment::attach(state, session, instance);
        }
        Command::Completions { shell } => cli::completions(shell, cli.runtime.as_deref())?,
        Command::CompleteNames { words } => return Ok(cli::complete_names(&state, words)),
    }
    Ok(0)
}
fn main() {
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
