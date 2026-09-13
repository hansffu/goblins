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
        Command::Stop { id_or_name } => println!(
            "{}",
            Client::connect(&state)?
                .call("sessions.stop", serde_json::json!({"session":id_or_name}))?
        ),
        Command::Rpc {
            method,
            params_json,
        } => println!(
            "{}",
            Client::connect(&state)?.call(&method, serde_json::from_str(&params_json)?)?
        ),
        Command::Run { config, name } => {
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
            return attachment::run(state, config, configuration, name);
        }
        Command::Completions { shell } => cli::completions(shell, cli.runtime.as_deref())?,
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
