//! `goblin host`: commands the host runs outside this sandbox once it
//! approves them. A run is a `host-command` permission request; its ID is
//! also the job's. The job's output is a file the daemon writes to
//! /run/goblins/host-jobs/JOB, which this client tails.
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

fn report(result: &contract::PermissionRequestReply) -> i32 {
    let code = exit_code(result);
    let status = result.status.as_deref().unwrap_or("unknown");
    if !(status == "finished" && result.code == Some(0)) {
        let detail = result
            .message
            .as_deref()
            .map(|m| format!(": {}", goblins_protocol::plain_text(m)))
            .unwrap_or_default();
        eprintln!(
            "goblin: host command {} {status}{detail}",
            result.request.as_deref().unwrap_or("?")
        );
    }
    code
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
    let mut socket = connect()?;
    socket
        .set_write_timeout(Some(rpc::TIMEOUT))
        .map_err(|e| e.to_string())?;
    let params = contract::PermissionParams {
        host: Some(host),
        ..crate::permission_params("host-command", reason)
    };
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
        if let Some(submitted) = contract::host_submitted(&frame) {
            eprintln!(
                "goblin: host command {} is waiting for approval",
                submitted.request
            );
            job = Some(submitted.request);
            continue;
        }
        rpc::validate_response(&frame, &json!(1)).map_err(|e| e.to_string())?;
        if let Some(error) = frame.get("error") {
            eprintln!(
                "goblin: host command refused: {}",
                goblins_protocol::plain_text(error["message"].as_str().unwrap_or("request failed"))
            );
            return Ok(NOT_RUN);
        }
        let result = &frame["result"];
        if detach {
            println!("{result}");
            return Ok(0);
        }
        let result =
            contract::decode_reply::<contract::sandbox::PermissionsRequest>(result.clone())
                .map_err(|e| e.to_string())?;
        if let Some(job) = result.request.as_deref() {
            tail.copy(job);
        }
        return Ok(report(&result));
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
                    return Ok(report(&result));
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
