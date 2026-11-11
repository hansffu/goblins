//! `goblins terminal ...`: the terminals of a goblin's sandbox, from the host.
//! Each is a call on the host endpoint; `attach` also connects this
//! terminal to the goblin's attach socket.
use crate::cli::TerminalCommand;
use crate::{RESIZE, STOP, attachment};
use goblins_controller::{Result, host::Client};
use goblins_protocol::contract;
use goblins_protocol::plain_text;
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
                pairs.push((name.to_string(), value.to_string()));
            }
            let params = contract::HostTerminalCreateParams {
                session: goblin,
                env: pairs,
                argv: (!argv.is_empty()).then_some(argv),
                name,
                cwd,
                rows: size.map(|s| s.0),
                cols: size.map(|s| s.1),
                notify: false,
            };
            let terminal =
                client.call_patient_typed::<contract::host::TerminalsCreate>(params, &STOP)?;
            println!("{}", plain_text(terminal.id.as_deref().unwrap_or("?")));
        }
        TerminalCommand::List { goblin } => {
            let list = client.call_patient_typed::<contract::host::TerminalsList>(
                contract::SessionSelectorParams { session: goblin },
                &STOP,
            )?;
            for terminal in list.terminals {
                let text = |value: Option<&str>| plain_text(value.unwrap_or("-"));
                println!(
                    "{}\t{}\t{}\t{}x{}\t{}\t{}",
                    text(terminal.id.as_deref()),
                    text(terminal.name.as_deref()),
                    text(terminal.origin.as_deref()),
                    terminal.rows,
                    terminal.cols,
                    if terminal.attached == Some(true) {
                        "attached"
                    } else {
                        "detached"
                    },
                    text(terminal.label.as_deref()),
                );
            }
        }
        TerminalCommand::Read {
            goblin,
            terminal,
            source,
            lines,
        } => {
            let params = contract::HostTerminalReadParams {
                session: goblin,
                terminal,
                source,
                lines,
            };
            let read = client.call_patient_typed::<contract::host::TerminalsRead>(params, &STOP)?;
            println!("{}", read.text.as_deref().unwrap_or_default());
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
            let (match_, regex, exit) = match (text, regex, exit) {
                (Some(text), None, false) => (Some(text), None, None),
                (None, Some(regex), false) => (None, Some(regex), None),
                (None, None, true) => (None, None, Some(true)),
                _ => return Err("give one of --match, --regex and --exit".into()),
            };
            let params = contract::HostTerminalWaitParams {
                session: goblin,
                terminal: terminal.clone(),
                match_,
                regex,
                exit,
                timeout,
                lines,
            };
            let result =
                client.call_patient_typed::<contract::host::TerminalsWait>(params, &STOP)?;
            return Ok(report(&terminal, &result));
        }
        TerminalCommand::Close { goblin, terminal } => {
            client.call_patient_typed::<contract::host::TerminalsClose>(
                contract::HostTerminalTargetParams {
                    session: goblin,
                    terminal,
                },
                &STOP,
            )?;
        }
        TerminalCommand::Detach { goblin, terminal } => {
            client.call_patient_typed::<contract::host::TerminalsDetach>(
                contract::HostTerminalTargetParams {
                    session: goblin,
                    terminal,
                },
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
    let params = contract::HostTerminalSendParams {
        session: goblin,
        terminal,
        keys,
        text,
    };
    client.call_patient_typed::<contract::host::TerminalsSend>(params, &STOP)?;
    Ok(())
}

/// Print what a wait found: the exit code is 0 for a match or an exit, 1 for
/// a timeout.
fn report(terminal: &str, result: &contract::TerminalWaitReply) -> i32 {
    match result.status.as_deref() {
        Some("matched") => {
            println!("{}", result.line.as_deref().unwrap_or_default());
            0
        }
        Some("exited") => {
            println!("{}", result.text.as_deref().unwrap_or_default());
            let how = match (result.code, result.signal) {
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
    let record = client.call_typed::<goblins_protocol::contract::host::SessionsGet>(
        goblins_protocol::contract::SessionSelectorParams { session: goblin },
    )?;
    let session = record.id.clone();
    let stream = attachment::handshake(
        &client,
        &session,
        &serde_json::to_value(record)?,
        Some(&terminal),
    )?;
    // The frontend's window is applied once the stream is relayed.
    RESIZE.store(true, Ordering::Relaxed);
    let mut outcome = None;
    let code = goblins_protocol::terminal::relay_with(
        stream,
        client,
        |client, rows, cols| {
            client
                .call_typed::<contract::host::TerminalsResize>(contract::HostTerminalResizeParams {
                    session: session.clone(),
                    terminal: terminal.clone(),
                    rows,
                    cols,
                })
                .map(drop)
        },
        |client| {
            // The stream ended because the terminal was detached or ended;
            // the list says which. An ended terminal may still be listed, but
            // its attachment is not released while it is.
            let list = client.call_typed::<contract::host::TerminalsList>(
                contract::SessionSelectorParams {
                    session: session.clone(),
                },
            )?;
            let label = |t: &contract::TerminalDisplay| {
                t.id.as_deref() == Some(terminal.as_str())
                    || t.name.as_deref() == Some(terminal.as_str())
            };
            let detached = list
                .terminals
                .iter()
                .find(|t| label(t))
                .is_some_and(|t| t.attached == Some(false));
            let id = list
                .terminals
                .iter()
                .find(|t| label(t))
                .and_then(|t| t.id.as_deref())
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
