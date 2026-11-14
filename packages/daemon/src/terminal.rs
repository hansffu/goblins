//! `goblins terminal ...`: the terminals of a goblin's sandbox, from the host.
//! Each is a call on the host endpoint; `attach` also connects this
//! terminal to the goblin's attach socket.
use crate::cli::TerminalCommand;
use crate::{RESIZE, STOP, attachment};
use goblins_controller::{Result, host::Client};
use goblins_protocol::contract;
use goblins_protocol::plain_text;
use std::{fmt::Write, path::PathBuf, sync::atomic::Ordering};

pub fn run(state: PathBuf, command: TerminalCommand) -> Result<i32> {
    let mut client = Client::connect(&state)?;
    if !client.features.iter().any(|f| f == "terminals") {
        return Err("this daemon has no terminals; restart it".into());
    }
    if let TerminalCommand::Attach { goblin, terminal } = command {
        return attach(client, goblin, terminal);
    }
    let output = call(&mut client, command)?;
    print!("{}", output.stdout);
    eprint!("{}", output.stderr);
    Ok(output.code)
}

/// What a terminal command prints, and its exit code.
#[derive(Default)]
struct Output {
    code: i32,
    stdout: String,
    stderr: String,
}

/// Make the host call a terminal command (other than `attach`) stands for.
fn call(client: &mut Client, command: TerminalCommand) -> Result<Output> {
    let mut output = Output::default();
    let out = &mut output.stdout;
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
            writeln!(out, "{}", plain_text(terminal.id.as_deref().unwrap_or("?")))?;
        }
        TerminalCommand::List { goblin } => {
            let list = client.call_patient_typed::<contract::host::TerminalsList>(
                contract::SessionSelectorParams { session: goblin },
                &STOP,
            )?;
            for terminal in list.terminals {
                let text = |value: Option<&str>| plain_text(value.unwrap_or("-"));
                writeln!(
                    out,
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
                )?;
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
            writeln!(out, "{}", read.text.as_deref().unwrap_or_default())?;
        }
        TerminalCommand::Send {
            goblin,
            terminal,
            text,
            keys,
        } => {
            send(client, goblin, terminal, text, keys)?;
        }
        TerminalCommand::Run {
            goblin,
            terminal,
            command,
        } => {
            send(
                client,
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
            report(&terminal, &result, &mut output);
            return Ok(output);
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
        TerminalCommand::Attach { .. } => unreachable!("run attaches"),
    }
    Ok(output)
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

/// Report what a wait found: the exit code is 0 for a match or an exit, 1
/// for a timeout.
fn report(terminal: &str, result: &contract::TerminalWaitReply, output: &mut Output) {
    let (out, err) = (&mut output.stdout, &mut output.stderr);
    // Writing to a String cannot fail.
    output.code = match result.status.as_deref() {
        Some("matched") => {
            let _ = writeln!(out, "{}", result.line.as_deref().unwrap_or_default());
            0
        }
        Some("exited") => {
            let _ = writeln!(out, "{}", result.text.as_deref().unwrap_or_default());
            let how = match (result.code, result.signal) {
                (Some(code), _) => format!("with code {code}"),
                (None, Some(signal)) => format!("from signal {signal}"),
                (None, None) => "with no status".into(),
            };
            let _ = writeln!(err, "{} exited {how}", plain_text(terminal));
            0
        }
        _ => {
            let _ = writeln!(err, "timed out waiting for {}", plain_text(terminal));
            1
        }
    };
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cli::{Cli, Command};
    use crate::fake_daemon::{Daemon, fixtures};
    use clap::Parser;
    use serde_json::{Value, json};

    /// Run `goblins terminal ARGS` against a daemon answering `result`: the
    /// call it made, and what it printed.
    fn terminal(args: &[&str], result: Value) -> (Value, Output) {
        let Some(Command::Terminal { command }) =
            Cli::try_parse_from(["goblins", "terminal"].iter().chain(args))
                .unwrap()
                .command
        else {
            unreachable!()
        };
        let _no_forks = goblins_unix::no_forks();
        let daemon = Daemon::start(&["terminals"], result);
        let output = call(&mut Client::connect(&daemon.state).unwrap(), command).unwrap();
        (daemon.call(), output)
    }

    #[test]
    fn terminal_io_commands_send_their_requests_and_report_replies() {
        let base = json!({"session":"a","terminal":"t1"});
        let with = |extra: Value| {
            let mut params = base.clone();
            params
                .as_object_mut()
                .unwrap()
                .extend(extra.as_object().unwrap().clone());
            params
        };
        let cases = [
            (
                &["read", "a", "t1"][..],
                "read",
                base.clone(),
                json!({"text":"hello"}),
                0,
                "hello\n",
                "",
            ),
            (
                &["read", "a", "t1", "--source", "recent", "--lines", "7"],
                "read",
                with(json!({"source":"recent","lines":7})),
                Value::Null,
                0,
                "\n",
                "",
            ),
            (
                &["read", "a", "t1"],
                "read",
                base.clone(),
                json!({"text":7}),
                0,
                "\n",
                "",
            ),
            (
                &["send", "a", "t1", "--text", ""],
                "send",
                with(json!({"text":"","keys":[]})),
                json!([]),
                0,
                "",
                "",
            ),
            (
                &["send", "a", "t1", "--keys", "g", "A", "ctrl+c"],
                "send",
                with(json!({"keys":["g","A","ctrl+c"]})),
                Value::Null,
                0,
                "",
                "",
            ),
            (
                &["run", "a", "t1", "echo hi"],
                "send",
                with(json!({"text":"echo hi","keys":["enter"]})),
                json!([]),
                0,
                "",
                "",
            ),
            (
                &[
                    "wait",
                    "a",
                    "t1",
                    "--match",
                    "done",
                    "--timeout",
                    "0",
                    "--lines",
                    "5",
                ],
                "wait",
                with(json!({"match":"done","timeout":0,"lines":5})),
                json!({"status":"matched","line":7}),
                0,
                "\n",
                "",
            ),
            (
                &["wait", "a", "t1", "--regex", "^done$"],
                "wait",
                with(json!({"regex":"^done$"})),
                json!({"status":"matched","line":"done","future":true}),
                0,
                "done\n",
                "",
            ),
            (
                &["wait", "a", "t1", "--exit"],
                "wait",
                with(json!({"exit":true})),
                json!({"status":"exited","code":"bad","signal":15,"text":7}),
                0,
                "\n",
                "t1 exited from signal 15\n",
            ),
            (
                &["wait", "a", "t1", "--exit"],
                "wait",
                with(json!({"exit":true})),
                json!({"status":"exited","code":7,"signal":"bad","text":"last"}),
                0,
                "last\n",
                "t1 exited with code 7\n",
            ),
            (
                &["wait", "a", "t1", "--match", "done"],
                "wait",
                with(json!({"match":"done"})),
                json!({"status":"future"}),
                1,
                "",
                "timed out waiting for t1\n",
            ),
            (
                &["wait", "a", "t1", "--match", "done"],
                "wait",
                with(json!({"match":"done"})),
                Value::Null,
                1,
                "",
                "timed out waiting for t1\n",
            ),
            (
                &["detach", "a", "t1"],
                "detach",
                base.clone(),
                json!([]),
                0,
                "",
                "",
            ),
        ];
        for (args, method, params, result, code, stdout, stderr) in cases {
            let (call, output) = terminal(args, result.clone());
            assert_eq!(call["method"], format!("terminals.{method}"), "{args:?}");
            assert_eq!(call["params"], params, "{args:?}");
            assert_eq!(
                (output.code, output.stdout.as_str(), output.stderr.as_str()),
                (code, stdout, stderr),
                "{args:?} {result}"
            );
        }
    }

    #[test]
    fn terminal_lifecycle_commands_send_their_requests_and_show_replies() {
        let record = fixtures()["terminal_record"].clone();
        let cases = [
            (
                &["create", "a"][..],
                json!({"session":"a","env":[]}),
                record.clone(),
                "t1\n",
            ),
            (
                &[
                    "create", "a", "--name", "app", "--cwd", "/work", "--env", "A=b", "--size",
                    "30x90", "--", "sh",
                ],
                json!({"session":"a","env":[["A","b"]],"name":"app","cwd":"/work","rows":30,"cols":90,"argv":["sh"]}),
                json!({"id":7}),
                "?\n",
            ),
            (
                &["create", "a"],
                json!({"session":"a","env":[]}),
                Value::Null,
                "?\n",
            ),
            (
                &["list", "a"],
                json!({"session":"a"}),
                json!({"terminals":[record]}),
                "t1\t-\thost\t40x120\tdetached\tshell\n",
            ),
            (
                &["list", "a"],
                json!({"session":"a"}),
                json!({"terminals":[{"id":7,"name":"odd","origin":"remote","rows":"wide","cols":null,"attached":"false","label":"bad"},null]}),
                "-\todd\tremote\t\"wide\"xnull\tdetached\tbad\n-\t-\t-\tnullxnull\tdetached\t-\n",
            ),
            (&["list", "a"], json!({"session":"a"}), Value::Null, ""),
            (
                &["close", "a", "app"],
                json!({"session":"a","terminal":"app"}),
                json!([]),
                "",
            ),
        ];
        for (args, params, result, stdout) in cases {
            let (call, output) = terminal(args, result.clone());
            assert_eq!(call["method"], format!("terminals.{}", args[0]), "{args:?}");
            assert_eq!(call["params"], params, "{args:?}");
            assert_eq!(
                (output.code, output.stdout.as_str(), output.stderr.as_str()),
                (0, stdout, ""),
                "{args:?} {result}"
            );
        }
    }
}
