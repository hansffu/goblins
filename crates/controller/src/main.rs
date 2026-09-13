//! Headless daemon entry point, host API frontends and raw terminal client.
//! The controller library never opens the approval terminal.
mod attachment;
mod plain;
mod tui;
use goblins_controller::{Result, config::Manifest, controller::Controller, host::Client};
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
fn main() {
    signals();
    let mut args: Vec<String> = std::env::args().skip(1).collect();
    if args.iter().any(|a| a == "--help" || a == "-h") {
        println!(
            "usage: goblins [--runtime MANIFEST] [--state-dir DIRECTORY] daemon [--workspace DIRECTORY]\n       goblins [--state-dir DIRECTORY] serve [--plain]\n       goblins [--state-dir DIRECTORY] run NAME\n       goblins [--state-dir DIRECTORY] shell\n       goblins [--state-dir DIRECTORY] list\n       goblins [--state-dir DIRECTORY] stop SESSION\n       goblins [--state-dir DIRECTORY] rpc METHOD PARAMS_JSON"
        );
        return;
    }
    let option = |args: &mut Vec<String>, key: &str| -> Result<Option<String>> {
        if let Some(i) = args.iter().position(|a| a == key) {
            args.remove(i);
            if i == args.len() {
                return Err(format!("{key} requires a value").into());
            }
            Ok(Some(args.remove(i)))
        } else {
            Ok(None)
        }
    };
    let execute = || -> Result<i32> {
        let runtime = option(&mut args, "--runtime")?.map(PathBuf::from);
        let uid = unsafe { libc::getuid() };
        let root = std::env::var_os("XDG_RUNTIME_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(|| format!("/run/user/{uid}").into());
        let default = if root.is_dir() {
            root.join("goblins")
        } else {
            format!("/tmp/goblins-control-{uid}").into()
        };
        let state = option(&mut args, "--state-dir")?
            .map(PathBuf::from)
            .unwrap_or(default);
        let workspace = option(&mut args, "--workspace")?.map(PathBuf::from);
        let plain = if let Some(i) = args.iter().position(|a| a == "--plain") {
            args.remove(i);
            true
        } else {
            false
        };
        if workspace.is_some() && args.first().is_none_or(|a| a != "daemon") {
            return Err("--workspace is only valid with daemon".into());
        }
        if plain && args.first().is_none_or(|a| a != "serve") {
            return Err("--plain is only valid with serve".into());
        }
        match args.as_slice() {
            [command] if command == "daemon" => {
                let mut daemon = Controller::new(&state, workspace)?;
                println!(
                    "Goblins daemon ready at {}",
                    state.join("host.sock").display()
                );
                while !STOP.load(Ordering::Relaxed) {
                    daemon.tick()?;
                    std::thread::sleep(std::time::Duration::from_millis(5));
                }
            }
            [command] if command == "serve" => {
                if workspace.is_some() {
                    return Err("--workspace belongs to daemon".into());
                }
                if plain {
                    plain::serve(state)?;
                } else {
                    tui::serve(state)?;
                }
            }
            [command] if command == "list" => {
                println!(
                    "{}",
                    Client::connect(&state)?.call("sessions.list", serde_json::json!({}))?
                );
            }
            [command, id] if command == "stop" => {
                println!(
                    "{}",
                    Client::connect(&state)?
                        .call("sessions.stop", serde_json::json!({"session":id}))?
                );
            }
            [command, method, params] if command == "rpc" => {
                println!(
                    "{}",
                    Client::connect(&state)?.call(method, serde_json::from_str(params)?)?
                );
            }
            _ => {
                let name = match args.as_slice() {
                    [command] if command == "shell" => "shell",
                    [command, name] if command == "run" => name,
                    _ => return Err("expected daemon, serve, run NAME, shell, list, stop SESSION or rpc METHOD PARAMS_JSON".into()),
                };
                let runtime =
                    runtime.ok_or("run requires --runtime (use the Nix-built goblins command)")?;
                let configuration = fs::canonicalize(&runtime)?.display().to_string();
                let manifest = Manifest::read(&runtime)?;
                if !manifest.goblins.contains_key(name) {
                    eprintln!(
                        "goblins: unknown goblin; available: {}",
                        manifest
                            .goblins
                            .keys()
                            .cloned()
                            .collect::<Vec<_>>()
                            .join(", ")
                    );
                    return Ok(2);
                }
                return attachment::run(state, name.into(), configuration);
            }
        }
        Ok(0)
    };
    let mut execute = execute;
    std::process::exit(match execute() {
        Ok(code) => code,
        Err(e) => {
            eprintln!("goblins: {e}");
            1
        }
    });
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
