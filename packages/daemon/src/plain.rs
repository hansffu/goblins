//! Line-oriented approval frontend. It owns presentation, never sessions.
use crate::STOP;
use goblins_controller::{
    Result,
    host::{Client, decision},
    unix,
};
use std::{
    fs::OpenOptions,
    io::{Read, Write},
    os::fd::AsRawFd,
    path::PathBuf,
    sync::atomic::Ordering,
};
pub(super) fn escaped_json(value: &impl serde::Serialize) -> Result<String> {
    let mut result = String::new();
    for ch in serde_json::to_string(value)?.chars() {
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
pub fn serve(state: PathBuf) -> Result<()> {
    let mut client = Client::connect(&state)?;
    let mut subscription = Client::connect(&state)?.subscribe()?;
    if client.instance != subscription.snapshot.instance {
        return Err("daemon changed while connecting; reconnect".into());
    }
    let mut input = OpenOptions::new().read(true).open("/dev/tty")?;
    let mut output = OpenOptions::new().write(true).open("/dev/tty")?;
    writeln!(
        output,
        "Goblins approval frontend. Daemon {}. Type status or quit. Sessions survive quit.",
        client.instance
    )?;
    let mut line = Vec::new();
    let mut show = true;
    while !STOP.load(Ordering::Relaxed) {
        show |= subscription.tick()?;
        if show {
            writeln!(output, "STATE {}", escaped_json(&subscription.snapshot)?)?;
            for p in &subscription.snapshot.permissions {
                if p.state == "pending" {
                    writeln!(
                        output,
                        "REQUEST {}\nType approve {} or deny {}",
                        escaped_json(p)?,
                        p.approval,
                        p.approval
                    )?;
                }
            }
            output.flush()?;
            show = false;
        }
        if !unix::readable(input.as_raw_fd(), 20)? {
            continue;
        }
        let mut bytes = [0; 1024];
        let n = input.read(&mut bytes)?;
        if n == 0 {
            break;
        }
        for byte in &bytes[..n] {
            if *byte != b'\n' {
                if line.len() < 4096 {
                    line.push(*byte);
                }
                continue;
            }
            let command = String::from_utf8_lossy(&line).trim().to_string();
            line.clear();
            match command.as_str() {
                "quit" | ":quit" => return Ok(()),
                "status" | ":status" => show = true,
                _ => {
                    if let Some((p, yes)) = decision(&command, &subscription.snapshot) {
                        match client.decide(p, yes) {
                            Ok(r) => writeln!(output, "DECISION {}", escaped_json(&r)?)?,
                            Err(e) => writeln!(
                                output,
                                "DECISION rejected: {}",
                                escaped_json(&e.to_string())?
                            )?,
                        }
                    } else {
                        writeln!(
                            output,
                            "No matching pending approval. Use approve TOKEN or deny TOKEN."
                        )?;
                    }
                }
            }
        }
        output.flush()?;
    }
    Ok(())
}
