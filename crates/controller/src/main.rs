//! Temporary foreground presentation and host terminal attachment. The library
//! consumes explicit decisions and emits events; it never accesses this tty.
mod attachment;
mod plain;
mod tui;
use goblins_controller::{
    Result,
    config::Manifest,
    controller::{ApprovalId, Controller, Event},
    unix,
};
use std::{
    fs::{self, File, OpenOptions},
    io::{Read, Write},
    os::fd::AsRawFd,
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
            "usage: goblins [--runtime MANIFEST] [--state-dir DIRECTORY] serve [--workspace DIRECTORY] [--plain | --theme terminal|onedark]\n       goblins [--state-dir DIRECTORY] run NAME\n       goblins [--state-dir DIRECTORY] shell"
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
        let runtime = PathBuf::from(
            option(&mut args, "--runtime")?
                .ok_or("missing --runtime manifest (use the Nix-built goblins command)")?,
        );
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
        let theme = option(&mut args, "--theme")?;
        let manifest = Manifest::read(&runtime)?;
        let configuration = fs::canonicalize(runtime)?.display().to_string();
        match args.as_slice() {
            [command] if command == "serve" => {
                if plain {
                    plain::serve(manifest, state, workspace)?;
                } else {
                    tui::serve(state, workspace, tui::Theme::parse(theme.as_deref())?)?;
                }
            }
            _ => {
                let name = match args.as_slice() {
                    [command] if command == "shell" => "shell",
                    [command, name] if command == "run" => name,
                    _ => return Err("expected serve, run NAME or shell".into()),
                };
                if plain || theme.is_some() {
                    return Err("--plain and --theme are only valid with serve".into());
                }
                if workspace.is_some() {
                    return Err("--workspace is only valid with serve".into());
                }
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
                attachment::run(state, name.into(), configuration)?;
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
