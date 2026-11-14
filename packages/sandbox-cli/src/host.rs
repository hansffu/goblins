//! `goblin host`: commands the host runs outside this sandbox once it
//! approves them. A run is a `host-command` permission request; its ID is
//! also the job's. The job's output is a file the daemon writes to
//! /run/goblins/host-jobs/JOB, which this client tails.
use crate::Output;
use crate::cli::HostCommand;
use contract::Method;
use goblins_jsonrpc as rpc;
use goblins_protocol::contract;
use serde_json::{Value, json};
use std::{
    fs::File,
    io::{Read, Seek, SeekFrom, Write},
    os::{fd::AsRawFd, unix::net::UnixStream},
    path::{Path, PathBuf},
    time::{Duration, Instant},
};

const FEATURE: &str = "host-commands";
/// The command did not run, or its outcome is unknown.
const NOT_RUN: i32 = 125;
/// `wait` gave up while the job still runs, like timeout(1).
const STILL_RUNNING: i32 = 124;

fn output(job: &str) -> PathBuf {
    Path::new("/run/goblins/host-jobs")
        .join(job)
        .join("output.log")
}

/// Copies new bytes of a job's output to stdout.
#[derive(Default)]
struct Tail {
    file: Option<File>,
    offset: u64,
}
impl Tail {
    fn copy(&mut self, job: &str) {
        if self.file.is_none() {
            self.file = File::open(output(job)).ok();
        }
        let Some(file) = &mut self.file else {
            return;
        };
        let mut bytes = Vec::new();
        if file.seek(SeekFrom::Start(self.offset)).is_ok() && file.read_to_end(&mut bytes).is_ok() {
            self.offset += bytes.len() as u64;
            let mut out = std::io::stdout();
            let _ = out.write_all(&bytes);
            let _ = out.flush();
        }
    }
}

/// The exit status a finished request or job stands for.
fn exit_code(result: &contract::PermissionRequestReply) -> i32 {
    match result.status.as_deref() {
        Some("finished" | "cancelled") if result.code.is_some() => result.code.unwrap() as i32,
        Some("failed") if result.code.is_some() => result.code.unwrap() as i32,
        _ => NOT_RUN,
    }
}

fn report(result: &contract::PermissionRequestReply) -> Output {
    let mut out = Output {
        code: exit_code(result),
        ..Output::default()
    };
    let status = result.status.as_deref().unwrap_or("unknown");
    if !(status == "finished" && result.code == Some(0)) {
        let detail = result
            .message
            .as_deref()
            .map(|m| format!(": {}", goblins_protocol::plain_text(m)))
            .unwrap_or_default();
        out.stderr = format!(
            "goblin: host command {} {status}{detail}\n",
            result.request.as_deref().unwrap_or("?")
        );
    }
    out
}

fn connect() -> Result<UnixStream, String> {
    let (socket, init) = crate::connect()?;
    if !init.features.iter().any(|candidate| candidate == FEATURE) {
        return Err("this daemon does not support host commands".into());
    }
    Ok(socket)
}

fn readable(socket: &UnixStream, timeout: Duration) -> bool {
    let mut poll = libc::pollfd {
        fd: socket.as_raw_fd(),
        events: libc::POLLIN,
        revents: 0,
    };
    unsafe { libc::poll(&mut poll, 1, timeout.as_millis() as i32) > 0 }
}

/// The `host-command` permission request for a run from `here`.
#[allow(clippy::too_many_arguments)]
fn params(
    reason: String,
    name: String,
    detach: bool,
    cwd: Option<PathBuf>,
    env: Vec<String>,
    timeout: Option<u64>,
    command: Vec<String>,
    here: PathBuf,
) -> Result<contract::PermissionParams, String> {
    let cwd = match cwd {
        Some(dir) if dir.is_absolute() => dir,
        Some(dir) => here.join(dir),
        None => here,
    };
    let env = env
        .iter()
        .map(|kv| {
            kv.split_once('=')
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .ok_or_else(|| format!("--env expects KEY=VALUE, not {kv}"))
        })
        .collect::<Result<Vec<_>, _>>()?;
    let host = goblins_protocol::rpc::HostCommand {
        argv: command,
        cwd: cwd
            .to_str()
            .ok_or("the directory must be UTF-8")?
            .to_string(),
        env,
        timeout,
        detach,
        name: Some(name),
    };
    if !host
        .name
        .as_deref()
        .is_some_and(goblins_protocol::rpc::host_name)
    {
        return Err(format!(
            "invalid --name: {}",
            goblins_protocol::rpc::HOST_NAME_RULE
        ));
    }
    if !host.validate() {
        return Err("invalid host command: check its arguments, environment and size".into());
    }
    Ok(contract::PermissionParams {
        host: Some(host),
        ..crate::permission_params("host-command", reason)
    })
}

/// What the final reply to a run shows, and the job whose output to copy
/// before it.
fn settle(frame: &Value, detach: bool) -> Result<(Option<String>, Output), String> {
    if let Some(error) = frame.get("error") {
        return Ok((
            None,
            Output {
                stderr: format!(
                    "goblin: host command refused: {}\n",
                    goblins_protocol::plain_text(
                        error["message"].as_str().unwrap_or("request failed")
                    )
                ),
                code: NOT_RUN,
                ..Output::default()
            },
        ));
    }
    let result = &frame["result"];
    if detach {
        return Ok((
            None,
            Output {
                stdout: format!("{result}\n"),
                ..Output::default()
            },
        ));
    }
    let result = contract::decode_reply::<contract::sandbox::PermissionsRequest>(result.clone())
        .map_err(|e| e.to_string())?;
    Ok((result.request.clone(), report(&result)))
}

fn run(
    reason: String,
    name: String,
    detach: bool,
    cwd: Option<PathBuf>,
    env: Vec<String>,
    timeout: Option<u64>,
    command: Vec<String>,
) -> Result<i32, String> {
    let here = std::env::current_dir().map_err(|e| e.to_string())?;
    let params = params(reason, name, detach, cwd, env, timeout, command, here)?;
    let mut socket = connect()?;
    socket
        .set_write_timeout(Some(rpc::TIMEOUT))
        .map_err(|e| e.to_string())?;
    socket
        .write_all(
            &rpc::encode(&rpc::request(
                json!(1),
                contract::sandbox::PermissionsRequest::NAME,
                contract::params_value::<contract::sandbox::PermissionsRequest>(params)
                    .map_err(|e| e.to_string())?,
            ))
            .map_err(|e| e.to_string())?,
        )
        .map_err(|e| e.to_string())?;
    // Interrupting this command closes the connection, which withdraws the
    // request or stops the job.
    let mut job: Option<String> = None;
    let mut tail = Tail::default();
    loop {
        if job.is_some() && !readable(&socket, Duration::from_millis(200)) {
            tail.copy(job.as_deref().unwrap());
            continue;
        }
        let frame = match rpc::read(&mut socket) {
            Ok(frame) => frame,
            Err(e) => {
                eprintln!(
                    "goblin: the outcome of host command {} is unknown ({e}); check goblin host list before running it again",
                    job.as_deref().unwrap_or("(not yet named)")
                );
                return Ok(NOT_RUN);
            }
        };
        if let Some(submitted) = contract::host_submitted(&frame).map_err(|e| e.to_string())? {
            eprintln!(
                "goblin: host command {} is waiting for approval",
                submitted.request
            );
            job = Some(submitted.request);
            continue;
        }
        rpc::validate_response(&frame, &json!(1)).map_err(|e| e.to_string())?;
        let (job, output) = settle(&frame, detach)?;
        if let Some(job) = job.as_deref() {
            tail.copy(job);
        }
        return Ok(output.emit());
    }
}

fn status(job: &str) -> Result<(contract::PermissionRequestReply, Value), String> {
    crate::call_typed_with_wire::<contract::sandbox::HostStatus>(contract::HostJobParams {
        request: job.into(),
    })
}

fn terminal(result: &contract::PermissionRequestReply) -> bool {
    !matches!(result.status.as_deref(), Some("pending" | "running"))
}

pub fn command(command: HostCommand) -> Result<i32, String> {
    match command {
        HostCommand::Run {
            reason,
            name,
            detach,
            cwd,
            env,
            timeout,
            command,
        } => run(reason, name, detach, cwd, env, timeout, command),
        HostCommand::Status { job } => {
            println!("{}", status(&job)?.1);
            Ok(0)
        }
        HostCommand::List => {
            println!(
                "{}",
                crate::call_typed_with_wire::<contract::sandbox::HostList>(
                    contract::EmptyParams {}
                )?
                .1
            );
            Ok(0)
        }
        HostCommand::Cancel { job } => {
            println!(
                "{}",
                crate::call_typed_with_wire::<contract::sandbox::HostCancel>(
                    contract::HostJobParams { request: job }
                )?
                .1
            );
            Ok(0)
        }
        HostCommand::Logs { job, follow } => {
            let mut tail = Tail::default();
            loop {
                tail.copy(&job);
                if !follow || terminal(&status(&job)?.0) {
                    tail.copy(&job);
                    return Ok(0);
                }
                std::thread::sleep(Duration::from_millis(500));
            }
        }
        HostCommand::Wait { job, timeout } => {
            let deadline = Instant::now() + Duration::from_secs(timeout);
            loop {
                let (result, _) = status(&job)?;
                if terminal(&result) {
                    return Ok(report(&result).emit());
                }
                if Instant::now() >= deadline {
                    eprintln!(
                        "goblin: host command {job} is still {}",
                        json!(result.status)
                    );
                    return Ok(STILL_RUNNING);
                }
                std::thread::sleep(Duration::from_secs(1));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn job_status_binding_retains_old_terminal_and_exit_classification() {
        for wire in [
            json!({"status":"pending"}),
            json!({"status":"running"}),
            json!({"status":"finished","code":0}),
            json!({"status":"cancelled","code":143}),
            json!({"status":"failed","code":127}),
            json!({"status":"finished","code":0.0}),
            json!({"status":"finished","code":4294967296i64}),
            json!({"status":7,"code":0}),
            Value::Null,
            json!([]),
        ] {
            let old_terminal = !matches!(wire["status"].as_str(), Some("pending" | "running"));
            let old_code = match wire["status"].as_str() {
                Some("finished" | "cancelled" | "failed") => {
                    wire["code"].as_i64().map(|n| n as i32).unwrap_or(NOT_RUN)
                }
                _ => NOT_RUN,
            };
            let reply =
                contract::decode_reply::<contract::sandbox::HostStatus>(wire.clone()).unwrap();
            assert_eq!(terminal(&reply), old_terminal, "{wire}");
            assert_eq!(exit_code(&reply), old_code, "{wire}");
        }
    }

    /// The params `goblin host ARGS` sends from `/work/here`.
    fn sent(args: &[&str]) -> Value {
        use crate::cli::{Cli, Command};
        use clap::Parser;
        let Command::Host {
            command:
                HostCommand::Run {
                    reason,
                    name,
                    detach,
                    cwd,
                    env,
                    timeout,
                    command,
                },
        } = Cli::try_parse_from(["goblin", "host"].iter().chain(args))
            .unwrap()
            .command
        else {
            unreachable!()
        };
        let params = params(
            reason,
            name,
            detach,
            cwd,
            env,
            timeout,
            command,
            "/work/here".into(),
        )
        .unwrap();
        contract::params_value::<contract::sandbox::PermissionsRequest>(params).unwrap()
    }

    /// What a run shows for the final reply `frame`.
    fn shown(frame: Value, detach: bool) -> (Option<String>, Output) {
        settle(&frame, detach).unwrap()
    }

    #[test]
    fn a_run_sends_a_host_command_request_with_its_cwd_relative_to_here() {
        assert_eq!(
            contract::sandbox::PermissionsRequest::NAME,
            "permissions.request"
        );
        assert_eq!(
            sent(&[
                "run",
                "--name",
                "Tests",
                "--reason",
                "tests",
                "--cwd",
                "sub",
                "--env",
                "A=b",
                "--timeout",
                "3",
                "--",
                "true"
            ]),
            json!({"kind":"host-command","reason":"tests","host":{
                "argv":["true"],"cwd":"/work/here/sub","env":[["A","b"]],"timeout":3,"name":"Tests"}})
        );
        assert_eq!(
            sent(&[
                "run", "--name", "Tests", "--reason", "tests", "--detach", "--", "true"
            ]),
            json!({"kind":"host-command","reason":"tests","host":{
                "argv":["true"],"cwd":"/work/here","env":[],"name":"Tests","detach":true}})
        );
    }

    #[test]
    fn a_run_reports_its_outcome_and_a_detached_run_prints_the_reply() {
        let frame = |result: Value| json!({"jsonrpc":"2.0","id":1,"result":result});
        let finished =
            json!({"request":"fixture-job-1","status":"finished","message":"exit code 0","code":0});
        assert_eq!(
            shown(frame(finished), false),
            (Some("fixture-job-1".into()), Output::default())
        );
        for result in [
            json!({"request":"fixture-job-1","status":"pending","extra":1}),
            Value::Null,
            json!([]),
            json!({"weird":true}),
        ] {
            let (job, output) = shown(frame(result.clone()), true);
            assert_eq!((job, output.code, output.stderr), (None, 0, String::new()));
            assert_eq!(
                serde_json::from_str::<Value>(&output.stdout).unwrap(),
                result
            );
            assert!(output.stdout.ends_with('\n'));
        }
        for (result, code, stderr) in [
            (json!({"status":"finished","code":0}), 0, ""),
            (
                json!({"status":"finished","code":0.0}),
                NOT_RUN,
                "goblin: host command ? finished\n",
            ),
            // The code is truncated to 32 bits, but it is not a plain 0.
            (
                json!({"status":"finished","code":4294967296i64}),
                0,
                "goblin: host command ? finished\n",
            ),
            (
                json!({"status":"cancelled","code":143,"message":"x\u{1b}"}),
                143,
                "goblin: host command ? cancelled: x\\u001b\n",
            ),
            (
                json!({"status":"failed","code":127}),
                127,
                "goblin: host command ? failed\n",
            ),
            (
                json!({"status":"future","code":0}),
                NOT_RUN,
                "goblin: host command ? future\n",
            ),
            (Value::Null, NOT_RUN, "goblin: host command ? unknown\n"),
        ] {
            let (_, output) = shown(frame(result.clone()), false);
            assert_eq!(
                output,
                Output {
                    stdout: String::new(),
                    stderr: stderr.into(),
                    code
                },
                "{result}"
            );
        }
        assert_eq!(
            shown(
                json!({"jsonrpc":"2.0","id":1,"error":{"code":-32009,"message":"re\u{1b}fused"}}),
                false
            ),
            (
                None,
                Output {
                    stdout: String::new(),
                    stderr: "goblin: host command refused: re\\u001bfused\n".into(),
                    code: NOT_RUN
                }
            )
        );
    }

    #[test]
    fn outcomes_become_exit_codes() {
        for (result, code) in [
            (json!({"status":"finished","code":0}), 0),
            (json!({"status":"finished","code":3}), 3),
            (json!({"status":"cancelled","code":143}), 143),
            (json!({"status":"failed","code":127}), 127),
            (json!({"status":"failed","message":"supervisor"}), NOT_RUN),
            (json!({"status":"denied"}), NOT_RUN),
            (json!({"status":"withdrawn"}), NOT_RUN),
        ] {
            assert_eq!(
                exit_code(
                    &contract::decode_reply::<contract::sandbox::PermissionsRequest>(
                        result.clone()
                    )
                    .unwrap()
                ),
                code,
                "{result}"
            );
        }
    }
}
