//! `goblin terminal ...`: the terminals of this goblin's own sandbox. Each is
//! a call on the request socket, which names no goblin: the daemon reaches
//! the caller's own.
use crate::cli::TerminalCommand;
use goblins_jsonrpc as rpc;
use goblins_protocol::plain_text;
use serde_json::{Value, json};
use std::io::Write;

const FEATURE: &str = "terminals";

/// One call. A wait may take an hour: nothing limits how long the answer
/// takes, and interrupting this command closes the connection, which
/// cancels the wait.
fn call(method: &str, params: Value) -> Result<Value, String> {
    let (mut socket, init) = crate::connect()?;
    if !init.features.iter().any(|candidate| candidate == FEATURE)
    {
        return Err("this daemon has no terminals for goblins; restart it".into());
    }
    socket
        .set_write_timeout(Some(rpc::TIMEOUT))
        .map_err(|e| e.to_string())?;
    socket
        .write_all(&rpc::encode(&rpc::request(json!(1), method, params)).map_err(|e| e.to_string())?)
        .map_err(|e| e.to_string())?;
    let frame = rpc::read(&mut socket).map_err(|e| e.to_string())?;
    rpc::validate_response(&frame, &json!(1)).map_err(|e| e.to_string())?;
    if let Some(error) = frame.get("error") {
        return Err(plain_text(error["message"].as_str().unwrap_or("request failed")));
    }
    Ok(frame["result"].clone())
}

pub fn run(command: TerminalCommand) -> Result<i32, String> {
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
                pairs.push(json!([name, value]));
            }
            let mut params = json!({"env":pairs});
            if !argv.is_empty() {
                params["argv"] = json!(argv);
            }
            if let Some(name) = name {
                params["name"] = json!(name);
            }
            if let Some(cwd) = cwd {
                // Relative to where this command runs.
                let cwd = if cwd.is_absolute() {
                    cwd
                } else {
                    std::env::current_dir().map_err(|e| e.to_string())?.join(cwd)
                };
                params["cwd"] = json!(cwd.to_str().ok_or("the directory must be UTF-8")?);
            }
            if let Some((rows, cols)) = size {
                params["rows"] = json!(rows);
                params["cols"] = json!(cols);
            }
            if notify {
                params["notify"] = json!(true);
            }
            let terminal = call("terminals.create", params)?;
            println!("{}", plain_text(terminal["id"].as_str().unwrap_or("?")));
        }
        TerminalCommand::List => {
            let list = call("terminals.list", json!({}))?;
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
            terminal,
            source,
            lines,
        } => {
            let mut params = json!({"terminal":terminal});
            if let Some(source) = source {
                params["source"] = json!(source);
            }
            if let Some(lines) = lines {
                params["lines"] = json!(lines);
            }
            let read = call("terminals.read", params)?;
            println!("{}", read["text"].as_str().unwrap_or_default());
        }
        TerminalCommand::Send {
            terminal,
            text,
            keys,
        } => send(terminal, text, keys)?,
        TerminalCommand::Run { terminal, command } => {
            send(terminal, Some(command), vec!["enter".into()])?
        }
        TerminalCommand::Wait {
            terminal,
            text,
            regex,
            exit,
            timeout,
            lines,
        } => {
            let mut params = json!({"terminal":terminal});
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
            return Ok(report(&terminal, &call("terminals.wait", params)?));
        }
        TerminalCommand::Close { terminal } => {
            call("terminals.close", json!({"terminal":terminal}))?;
        }
    }
    Ok(0)
}

fn send(terminal: String, text: Option<String>, keys: Vec<String>) -> Result<(), String> {
    let mut params = json!({"terminal":terminal,"keys":keys});
    if let Some(text) = text {
        params["text"] = json!(text);
    }
    call("terminals.send", params).map(drop)
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

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn a_wait_exits_0_on_a_match_or_an_exit_and_1_on_a_timeout() {
        assert_eq!(report("t1", &json!({"status":"matched","line":"done"})), 0);
        assert_eq!(report("t1", &json!({"status":"exited","code":3,"signal":null,"text":""})), 0);
        assert_eq!(report("t1", &json!({"status":"timeout"})), 1);
    }
}
