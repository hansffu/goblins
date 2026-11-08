//! `goblins terminal ...`: the terminals of a goblin's sandbox, from the host.
//! Each is a call on the host endpoint; `attach` also connects this
//! terminal to the goblin's attach socket.
use crate::cli::TerminalCommand;
use crate::{RESIZE, STOP, attachment};
use goblins_controller::{Result, host::Client};
use goblins_protocol::plain_text;
use serde_json::{Value, json};
use std::{path::PathBuf, sync::atomic::Ordering};

pub fn run(state: PathBuf, command: TerminalCommand) -> Result<i32> {
    let mut client = Client::connect(&state)?;
    if !client.features.iter().any(|f| f == "terminals") {
        return Err("this daemon has no terminals; restart it".into());
    }
    match command {
        TerminalCommand::Create {
            goblin,
            name,
            cwd,
            env,
            size,
            argv,
        } => {
            let mut pairs = vec![];
            for entry in &env {
                let (name, value) = entry
                    .split_once('=')
                    .ok_or_else(|| format!("--env expects NAME=VALUE, not {entry}"))?;
                pairs.push(json!([name, value]));
            }
            let mut params = json!({"session":goblin,"env":pairs});
            if !argv.is_empty() {
                params["argv"] = json!(argv);
            }
            if let Some(name) = name {
                params["name"] = json!(name);
            }
            if let Some(cwd) = cwd {
                params["cwd"] = json!(cwd);
            }
            if let Some((rows, cols)) = size {
                params["rows"] = json!(rows);
                params["cols"] = json!(cols);
            }
            let terminal = client.call_patient("terminals.create", params, &STOP)?;
            println!("{}", plain_text(terminal["id"].as_str().unwrap_or("?")));
        }
        TerminalCommand::List { goblin } => {
            let list = client.call_patient("terminals.list", json!({"session":goblin}), &STOP)?;
            for terminal in list["terminals"].as_array().into_iter().flatten() {
                let text = |key: &str| plain_text(terminal[key].as_str().unwrap_or("-"));
                println!(
                    "{}\t{}\t{}\t{}x{}\t{}\t{}",
                    text("id"),
                    text("name"),
                    text("origin"),
                    terminal["rows"],
                    terminal["cols"],
                    if terminal["attached"] == true {
                        "attached"
                    } else {
                        "detached"
                    },
                    text("label"),
                );
            }
        }
        TerminalCommand::Read {
            goblin,
            terminal,
            source,
            lines,
        } => {
            let mut params = json!({"session":goblin,"terminal":terminal});
            if let Some(source) = source {
                params["source"] = json!(source);
            }
            if let Some(lines) = lines {
                params["lines"] = json!(lines);
            }
            let read = client.call_patient("terminals.read", params, &STOP)?;
            println!("{}", read["text"].as_str().unwrap_or_default());
        }
        TerminalCommand::Send {
            goblin,
            terminal,
            text,
            keys,
        } => {
            send(&mut client, goblin, terminal, text, keys)?;
        }
        TerminalCommand::Run {
            goblin,
            terminal,
            command,
        } => {
            send(
                &mut client,
                goblin,
                terminal,
                Some(command),
                vec!["enter".into()],
            )?;
        }
        TerminalCommand::Wait {
            goblin,
            terminal,
            text,
            regex,
            exit,
            timeout,
            lines,
        } => {
            let mut params = json!({"session":goblin,"terminal":terminal});
            match (text, regex, exit) {
                (Some(text), None, false) => params["match"] = json!(text),
                (None, Some(regex), false) => params["regex"] = json!(regex),
                (None, None, true) => params["exit"] = json!(true),
                _ => return Err("give one of --match, --regex and --exit".into()),
            }
            if let Some(timeout) = timeout {
                params["timeout"] = json!(timeout);
            }
            if let Some(lines) = lines {
                params["lines"] = json!(lines);
            }
            let result = client.call_patient("terminals.wait", params, &STOP)?;
            return Ok(report(&terminal, &result));
        }
        TerminalCommand::Close { goblin, terminal } => {
            client.call_patient(
                "terminals.close",
                json!({"session":goblin,"terminal":terminal}),
                &STOP,
            )?;
        }
        TerminalCommand::Detach { goblin, terminal } => {
            client.call_patient(
                "terminals.detach",
                json!({"session":goblin,"terminal":terminal}),
                &STOP,
            )?;
        }
        TerminalCommand::Attach { goblin, terminal } => return attach(client, goblin, terminal),
    }
    Ok(0)
}

fn send(
    client: &mut Client,
    goblin: String,
    terminal: String,
    text: Option<String>,
    keys: Vec<String>,
) -> Result<()> {
    let mut params = json!({"session":goblin,"terminal":terminal,"keys":keys});
    if let Some(text) = text {
        params["text"] = json!(text);
    }
    client.call_patient("terminals.send", params, &STOP)?;
    Ok(())
}

/// Print what a wait found: the exit code is 0 for a match or an exit, 1 for
/// a timeout.
fn report(terminal: &str, result: &Value) -> i32 {
    match result["status"].as_str() {
        Some("matched") => {
            println!("{}", result["line"].as_str().unwrap_or_default());
            0
        }
        Some("exited") => {
            println!("{}", result["text"].as_str().unwrap_or_default());
            let how = match (result["code"].as_i64(), result["signal"].as_i64()) {
                (Some(code), _) => format!("with code {code}"),
                (None, Some(signal)) => format!("from signal {signal}"),
                (None, None) => "with no status".into(),
            };
            eprintln!("{} exited {how}", plain_text(terminal));
            0
        }
        _ => {
            eprintln!("timed out waiting for {}", plain_text(terminal));
            1
        }
    }
}

/// Connect this terminal to `terminal`. Leaving detaches; the goblin and its
/// terminal go on.
fn attach(mut client: Client, goblin: String, terminal: String) -> Result<i32> {
    if unsafe { libc::isatty(0) != 1 } {
        return Err("goblins terminal attach requires terminal input".into());
    }
    let record = client.call("sessions.get", json!({"session":goblin}))?;
    let session = record["id"].as_str().ok_or("missing session ID")?.to_string();
    let stream = attachment::handshake(&client, &session, &record, Some(&terminal))?;
    // The frontend's window is applied once the stream is relayed.
    RESIZE.store(true, Ordering::Relaxed);
    let mut outcome = None;
    let code = goblins_protocol::terminal::relay_with(
        stream,
        client,
        |client, rows, cols| {
            client
                .call(
                    "terminals.resize",
                    json!({"session":session,"terminal":terminal,"rows":rows,"cols":cols}),
                )
                .map(drop)
        },
        |client| {
            // The stream ended because the terminal was detached or ended;
            // the list says which. An ended terminal may still be listed, but
            // its attachment is not released while it is.
            let list = client.call("terminals.list", json!({"session":session}))?;
            let label = |t: &Value| t["id"] == terminal.as_str() || t["name"] == terminal.as_str();
            let detached = list["terminals"]
                .as_array()
                .into_iter()
                .flatten()
                .find(|t| label(t))
                .is_some_and(|t| t["attached"] == false);
            let id = list["terminals"]
                .as_array()
                .into_iter()
                .flatten()
                .find(|t| label(t))
                .and_then(|t| t["id"].as_str())
                .unwrap_or(&terminal)
                .to_string();
            outcome = Some(if detached {
                format!("detached from {id}")
            } else {
                format!("{id} ended")
            });
            Ok(0)
        },
        &STOP,
        &RESIZE,
    )?;
    if let Some(outcome) = outcome {
        eprintln!("{}", plain_text(&outcome));
    }
    Ok(code)
}
