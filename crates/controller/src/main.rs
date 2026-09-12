//! Temporary foreground presentation and host terminal attachment. The library
//! consumes explicit decisions and emits events; it never accesses this tty.
mod attachment;
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
/// Match the PoC's ASCII-only JSON display: C1 controls, bidi controls and
/// non-ASCII text stay data even in terminals that interpret them specially.
fn escaped_json(value: &impl serde::Serialize) -> Result<String> {
    let json = serde_json::to_string(value)?;
    let mut result = String::new();
    for ch in json.chars() {
        if ch.is_ascii() {
            result.push(ch);
        } else {
            for unit in ch.encode_utf16(&mut [0; 2]) {
                result.push_str(&format!("\\u{unit:04x}"));
            }
        }
    }
    Ok(result)
}
fn show(screen: &mut File, events: Vec<Event>, pending: &mut Option<ApprovalId>) -> Result<()> {
    for event in events {
        match event {
            Event::Connected(name) => writeln!(
                screen,
                "{name} connected. Package requests will appear here."
            )?,
            Event::Stopped => {
                *pending = None;
                writeln!(screen, "Goblin stopped. Ready for goblins run NAME.")?;
            }
            Event::Request {
                request: req,
                approval,
            } => {
                *pending = Some(approval);
                writeln!(screen, "REQUEST {}", escaped_json(&req)?)?;
                write!(screen, "Approve package? Type approve or deny: ")?;
            }
            Event::Result(reply) => {
                writeln!(screen, "RESULT {}", escaped_json(&reply)?)?;
            }
            Event::Detail(detail) => {
                // JSON escapes control characters, including terminal sequences.
                let detail: String = detail
                    .chars()
                    .rev()
                    .take(12000)
                    .collect::<String>()
                    .chars()
                    .rev()
                    .collect();
                writeln!(screen, "DETAIL {}", escaped_json(&detail)?)?;
            }
        }
    }
    screen.flush()?;
    Ok(())
}
fn serve(manifest: Manifest, state: PathBuf, workspace: Option<PathBuf>) -> Result<()> {
    let mut approval = OpenOptions::new().read(true).open("/dev/tty")?;
    let mut screen = OpenOptions::new().write(true).open("/dev/tty")?;
    let names = manifest
        .goblins
        .keys()
        .cloned()
        .collect::<Vec<_>>()
        .join(", ");
    let mut controller = Controller::new(&state, workspace)?;
    writeln!(
        screen,
        "Goblins serving. Run goblins run NAME in another terminal.\nGoblins: {names}\nEach run supplies its own configuration.\nPackages: pinned nixpkgs attributes (e.g. hello, cowsay, python3Packages.black). Type quit to stop."
    )?;
    let mut pending = None;
    let mut input = Vec::new();
    while !STOP.load(Ordering::Relaxed) {
        show(&mut screen, controller.tick()?, &mut pending)?;
        if !unix::readable(approval.as_raw_fd(), 20)? {
            continue;
        }
        let mut bytes = [0; 1024];
        let n = approval.read(&mut bytes)?;
        if n == 0 {
            break;
        }
        for byte in &bytes[..n] {
            if *byte != b'\n' {
                if input.len() < 4096 {
                    input.push(*byte);
                }
                continue;
            }
            let line = String::from_utf8_lossy(&input).trim().to_string();
            input.clear();
            match line.as_str() {
                "quit" | ":quit" => return Ok(()),
                "status" | ":status" => writeln!(screen, "{}", controller.status())?,
                "approve" | "deny" => {
                    if !pending
                        .take()
                        .is_some_and(|id| controller.decide(id, line == "approve"))
                    {
                        writeln!(screen, "No approval pending. Type status or quit.")?;
                    }
                }
                _ => writeln!(
                    screen,
                    "Type approve or deny for a pending request, status or quit."
                )?,
            }
        }
        screen.flush()?;
    }
    Ok(())
}
fn main() {
    signals();
    let mut args: Vec<String> = std::env::args().skip(1).collect();
    if args.iter().any(|a| a == "--help" || a == "-h") {
        println!(
            "usage: goblins [--runtime MANIFEST] [--state-dir DIRECTORY] serve [--workspace DIRECTORY]\n       goblins [--state-dir DIRECTORY] run NAME\n       goblins [--state-dir DIRECTORY] shell"
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
        let manifest = Manifest::read(&runtime)?;
        let configuration = fs::canonicalize(runtime)?.display().to_string();
        match args.as_slice() {
            [command] if command == "serve" => serve(manifest, state, workspace)?,
            _ => {
                let name = match args.as_slice() {
                    [command] if command == "shell" => "shell",
                    [command, name] if command == "run" => name,
                    _ => return Err("expected serve, run NAME or shell".into()),
                };
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
        let shown = super::escaped_json(&text).unwrap();
        assert!(shown.is_ascii());
        assert!(!shown.contains('\x1b'));
        assert_eq!(serde_json::from_str::<String>(&shown).unwrap(), text);
    }
}
