//! Line-oriented foreground fallback for logging and simple terminals.
use super::*;
/// Match the PoC's ASCII-only JSON display: C1 controls, bidi controls and
/// non-ASCII text stay data even in terminals that interpret them specially.
pub(super) fn escaped_json(value: &impl serde::Serialize) -> Result<String> {
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
            Event::Preview { .. } => (),
            Event::RequestGone(id) => {
                if *pending == Some(id) {
                    *pending = None;
                }
            }
            Event::Request {
                request: req,
                approval,
            } => {
                *pending = Some(approval);
                writeln!(screen, "REQUEST {}", escaped_json(&req)?)?;
                write!(screen, "Approve package? Type approve or deny: ")?;
            }
            Event::Result(reply) | Event::DecisionFinished { reply, .. } => {
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
pub fn serve(manifest: Manifest, state: PathBuf, workspace: Option<PathBuf>) -> Result<()> {
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
