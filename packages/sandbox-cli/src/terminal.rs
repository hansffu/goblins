//! `goblin terminal ...`: the terminals of this goblin's own sandbox. Each is
//! a call on the request socket, which names no goblin: the daemon reaches
//! the caller's own.
use crate::Output;
use crate::cli::TerminalCommand;
use goblins_jsonrpc as rpc;
use goblins_protocol::contract::{self, Method, Response};
use goblins_protocol::plain_text;
use serde_json::{Value, json};
use std::io::Write;
use std::path::PathBuf;

const FEATURE: &str = "terminals";

/// One call. A wait may take an hour: nothing limits how long the answer
/// takes, and interrupting this command closes the connection, which
/// cancels the wait.
fn call(method: &str, params: Value) -> Result<Value, String> {
    let (mut socket, init) = crate::connect()?;
    if !init.features.iter().any(|candidate| candidate == FEATURE) {
        return Err("this daemon has no terminals for goblins; restart it".into());
    }
    socket
        .set_write_timeout(Some(rpc::TIMEOUT))
        .map_err(|e| e.to_string())?;
    socket
        .write_all(
            &rpc::encode(&rpc::request(json!(1), method, params)).map_err(|e| e.to_string())?,
        )
        .map_err(|e| e.to_string())?;
    let frame = rpc::read(&mut socket).map_err(|e| e.to_string())?;
    rpc::validate_response(&frame, &json!(1)).map_err(|e| e.to_string())?;
    if let Some(error) = frame.get("error") {
        return Err(plain_text(
            error["message"].as_str().unwrap_or("request failed"),
        ));
    }
    Ok(frame["result"].clone())
}

pub fn run(command: TerminalCommand) -> Result<i32, String> {
    let terminal = match &command {
        TerminalCommand::Wait { terminal, .. } => terminal.clone(),
        _ => String::new(),
    };
    let (method, params) = request(command, || {
        std::env::current_dir().map_err(|e| e.to_string())
    })?;
    Ok(reply(method, &terminal, call(method, params)?)?.emit())
}

fn wire<M: Method>(params: M::Params) -> Result<(&'static str, Value), String> {
    Ok((
        M::NAME,
        contract::params_value::<M>(params).map_err(|e| e.to_string())?,
    ))
}

/// The call a command makes: its method and its params on the wire. A
/// relative --cwd is relative to `here`, where this command runs.
fn request(
    command: TerminalCommand,
    here: impl FnOnce() -> Result<PathBuf, String>,
) -> Result<(&'static str, Value), String> {
    use contract::sandbox as m;
    match command {
        TerminalCommand::Create {
            name,
            cwd,
            env,
            size,
            notify,
            argv,
        } => {
            let mut pairs = vec![];
            for entry in &env {
                let (name, value) = entry
                    .split_once('=')
                    .ok_or_else(|| format!("--env expects NAME=VALUE, not {entry}"))?;
                pairs.push((name.to_string(), value.to_string()));
            }
            let cwd = if let Some(cwd) = cwd {
                // Relative to where this command runs.
                let cwd = if cwd.is_absolute() {
                    cwd
                } else {
                    here()?.join(cwd)
                };
                Some(
                    cwd.to_str()
                        .ok_or("the directory must be UTF-8")?
                        .to_string(),
                )
            } else {
                None
            };
            wire::<m::TerminalsCreate>(contract::MemberTerminalCreateParams {
                env: pairs,
                argv: (!argv.is_empty()).then_some(argv),
                name,
                cwd,
                rows: size.map(|s| s.0),
                cols: size.map(|s| s.1),
                notify,
            })
        }
        TerminalCommand::List => wire::<m::TerminalsList>(contract::EmptyParams {}),
        TerminalCommand::Read {
            terminal,
            source,
            lines,
        } => wire::<m::TerminalsRead>(contract::MemberTerminalReadParams {
            terminal,
            source,
            lines,
        }),
        TerminalCommand::Send {
            terminal,
            text,
            keys,
        } => wire::<m::TerminalsSend>(contract::MemberTerminalSendParams {
            terminal,
            keys,
            text,
        }),
        TerminalCommand::Run { terminal, command } => {
            wire::<m::TerminalsSend>(contract::MemberTerminalSendParams {
                terminal,
                keys: vec!["enter".into()],
                text: Some(command),
            })
        }
        TerminalCommand::Wait {
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
            wire::<m::TerminalsWait>(contract::MemberTerminalWaitParams {
                terminal,
                match_,
                regex,
                exit,
                timeout,
                lines,
            })
        }
        TerminalCommand::Close { terminal } => {
            wire::<m::TerminalsClose>(contract::MemberTerminalTargetParams { terminal })
        }
    }
}

/// What the reply to `method` shows. `terminal` names the terminal a wait
/// waited for.
fn reply(method: &str, terminal: &str, wire: Value) -> Result<Output, String> {
    use contract::sandbox as m;
    fn decode<M: Method>(wire: Value) -> Result<M::Result, String> {
        M::Result::decode(wire).map_err(|e| e.to_string())
    }
    let mut out = Output::default();
    match method {
        m::TerminalsCreate::NAME => {
            let terminal = decode::<m::TerminalsCreate>(wire)?;
            out.stdout = format!("{}\n", plain_text(terminal.id.as_deref().unwrap_or("?")));
        }
        m::TerminalsList::NAME => {
            for terminal in decode::<m::TerminalsList>(wire)?.terminals {
                let text = |value: Option<&str>| plain_text(value.unwrap_or("-"));
                out.stdout += &format!(
                    "{}\t{}\t{}\t{}x{}\t{}\t{}\n",
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
        m::TerminalsRead::NAME => {
            let read = decode::<m::TerminalsRead>(wire)?;
            out.stdout = format!("{}\n", read.text.as_deref().unwrap_or_default());
        }
        m::TerminalsSend::NAME => {
            decode::<m::TerminalsSend>(wire)?;
        }
        m::TerminalsWait::NAME => return Ok(report(terminal, &decode::<m::TerminalsWait>(wire)?)),
        m::TerminalsClose::NAME => {
            decode::<m::TerminalsClose>(wire)?;
        }
        other => return Err(format!("no reply for {other}")),
    }
    Ok(out)
}

/// What a wait found: the exit code is 0 for a match or an exit, 1 for a
/// timeout.
fn report(terminal: &str, result: &contract::TerminalWaitReply) -> Output {
    match result.status.as_deref() {
        Some("matched") => Output {
            stdout: format!("{}\n", result.line.as_deref().unwrap_or_default()),
            ..Output::default()
        },
        Some("exited") => {
            let how = match (result.code, result.signal) {
                (Some(code), _) => format!("with code {code}"),
                (None, Some(signal)) => format!("from signal {signal}"),
                (None, None) => "with no status".into(),
            };
            Output {
                stdout: format!("{}\n", result.text.as_deref().unwrap_or_default()),
                stderr: format!("{} exited {how}\n", plain_text(terminal)),
                code: 0,
            }
        }
        _ => Output {
            stderr: format!("timed out waiting for {}\n", plain_text(terminal)),
            code: 1,
            ..Output::default()
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cli::{Cli, Command};
    use clap::Parser;

    /// The method and params `goblin terminal ARGS` sends from `here`.
    fn sent(args: &[&str], here: &str) -> (&'static str, Value) {
        let Command::Terminal { command } =
            Cli::try_parse_from(["goblin", "terminal"].iter().chain(args))
                .unwrap()
                .command
        else {
            unreachable!()
        };
        request(command, || Ok(PathBuf::from(here))).unwrap()
    }

    /// What `goblin terminal ARGS` shows for the reply `result`.
    fn shown(args: &[&str], result: Value) -> Output {
        let (method, _) = sent(args, "/");
        let terminal = match args[0] {
            "wait" => args[1],
            _ => "",
        };
        reply(method, terminal, result).unwrap()
    }

    fn output(stdout: &str, stderr: &str, code: i32) -> Output {
        Output {
            stdout: stdout.into(),
            stderr: stderr.into(),
            code,
        }
    }

    #[test]
    fn terminal_io_requests_and_reports() {
        for (args, method, params, result, expected) in [
            (
                &["read", "t1"][..],
                "read",
                json!({"terminal":"t1"}),
                json!({"text":"hello"}),
                output("hello\n", "", 0),
            ),
            (
                &["read", "t1", "--source", "recent", "--lines", "7"],
                "read",
                json!({"terminal":"t1","source":"recent","lines":7}),
                Value::Null,
                output("\n", "", 0),
            ),
            (
                &["read", "t1"],
                "read",
                json!({"terminal":"t1"}),
                json!({"text":7}),
                output("\n", "", 0),
            ),
            (
                &["send", "t1", "--text", ""],
                "send",
                json!({"terminal":"t1","text":"","keys":[]}),
                json!([]),
                output("", "", 0),
            ),
            (
                &["send", "t1", "--keys", "g", "A", "ctrl+c"],
                "send",
                json!({"terminal":"t1","keys":["g","A","ctrl+c"]}),
                Value::Null,
                output("", "", 0),
            ),
            (
                &["run", "t1", "echo hi"],
                "send",
                json!({"terminal":"t1","text":"echo hi","keys":["enter"]}),
                json!([]),
                output("", "", 0),
            ),
            (
                &[
                    "wait",
                    "t1",
                    "--match",
                    "done",
                    "--timeout",
                    "0",
                    "--lines",
                    "5",
                ],
                "wait",
                json!({"terminal":"t1","match":"done","timeout":0,"lines":5}),
                json!({"status":"matched","line":7}),
                output("\n", "", 0),
            ),
            (
                &["wait", "t1", "--regex", "^done$"],
                "wait",
                json!({"terminal":"t1","regex":"^done$"}),
                json!({"status":"matched","line":"done","future":true}),
                output("done\n", "", 0),
            ),
            (
                &["wait", "t1", "--exit"],
                "wait",
                json!({"terminal":"t1","exit":true}),
                json!({"status":"exited","code":"bad","signal":15,"text":7}),
                output("\n", "t1 exited from signal 15\n", 0),
            ),
            (
                &["wait", "t1", "--exit"],
                "wait",
                json!({"terminal":"t1","exit":true}),
                json!({"status":"exited","code":7,"signal":"bad","text":"last"}),
                output("last\n", "t1 exited with code 7\n", 0),
            ),
            (
                &["wait", "t1", "--match", "done"],
                "wait",
                json!({"terminal":"t1","match":"done"}),
                json!({"status":"future"}),
                output("", "timed out waiting for t1\n", 1),
            ),
            (
                &["wait", "t1", "--match", "done"],
                "wait",
                json!({"terminal":"t1","match":"done"}),
                Value::Null,
                output("", "timed out waiting for t1\n", 1),
            ),
        ] {
            let (sent_method, sent_params) = sent(args, "/");
            assert_eq!(
                (sent_method, sent_params),
                (format!("terminals.{method}").as_str(), params),
                "{args:?}"
            );
            assert_eq!(shown(args, result.clone()), expected, "{args:?} {result}");
        }
    }

    #[test]
    fn terminal_lifecycle_requests_resolve_cwd_and_keep_display_fallbacks() {
        for (args, method, params, result, stdout) in [
            (
                &["create"][..],
                "create",
                json!({"env":[]}),
                json!({"id":"t1"}),
                "t1\n",
            ),
            (
                &[
                    "create", "--name", "app", "--cwd", "relative", "--notify", "--env", "A=b",
                    "--size", "30x90", "--", "sh",
                ],
                "create",
                json!({"env":[["A","b"]],"name":"app","cwd":"/work/here/relative","notify":true,
                       "rows":30,"cols":90,"argv":["sh"]}),
                json!({"id":7}),
                "?\n",
            ),
            (&["create"], "create", json!({"env":[]}), Value::Null, "?\n"),
            (
                &["list"],
                "list",
                json!({}),
                json!({"terminals":[{"id":7,"name":"odd","origin":"remote","rows":"wide","cols":null,
                                     "attached":"false","label":"bad"},null]}),
                "-\todd\tremote\t\"wide\"xnull\tdetached\tbad\n-\t-\t-\tnullxnull\tdetached\t-\n",
            ),
            (&["list"], "list", json!({}), Value::Null, ""),
            (
                &["close", "app"],
                "close",
                json!({"terminal":"app"}),
                json!([]),
                "",
            ),
        ] {
            let (sent_method, sent_params) = sent(args, "/work/here");
            assert_eq!(
                (sent_method, sent_params),
                (format!("terminals.{method}").as_str(), params),
                "{args:?}"
            );
            assert_eq!(
                shown(args, result.clone()),
                output(stdout, "", 0),
                "{args:?} {result}"
            );
        }
        // An absolute --cwd is sent as given.
        assert_eq!(
            sent(&["create", "--cwd", "/abs"], "/work/here").1,
            json!({"env":[],"cwd":"/abs"})
        );
    }

    #[test]
    fn a_wait_exits_0_on_a_match_or_an_exit_and_1_on_a_timeout() {
        let report = |terminal: &str, wire: &Value| {
            super::report(
                terminal,
                &contract::TerminalWaitReply::decode(wire.clone()).unwrap(),
            )
            .code
        };
        assert_eq!(report("t1", &json!({"status":"matched","line":"done"})), 0);
        assert_eq!(
            report(
                "t1",
                &json!({"status":"exited","code":3,"signal":null,"text":""})
            ),
            0
        );
        assert_eq!(report("t1", &json!({"status":"timeout"})), 1);
    }
}
