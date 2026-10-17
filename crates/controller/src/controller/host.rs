//! Host commands (`goblin host run`): an approved `host-command` permission
//! request runs one command on the host, outside the sandbox, under the
//! helper's `--host-job` supervisor. The request ID is also the job ID. The
//! supervisor writes the job's output and outcome to the session's host-jobs
//! directory, which the sandbox sees read-only at /run/goblins/host-jobs, and
//! stops the job and everything it started when its lifetime pipe closes.
use super::{Controller, Fault, conflict, missing, params};
use goblins_protocol::rpc::{self, HostCommand};
use serde::Deserialize;
use serde_json::{Value, json};
use std::{
    fs::{self, File},
    os::{
        fd::{AsRawFd, OwnedFd},
        unix::process::CommandExt,
    },
    path::PathBuf,
    process::{Child, Command, Stdio},
};

/// Host commands per sandbox, pending or running.
const JOBS: usize = 4;
/// Output kept per job; the rest is discarded with a note.
const LOG_LIMIT: u64 = 16 * 1024 * 1024;
/// Finished jobs whose output stays readable per sandbox.
const KEPT: usize = 8;

/// A validated request, held with the pending approval.
pub(super) struct Spec {
    argv: Vec<String>,
    /// The pinned host directory the job starts in.
    cwd: File,
    cwd_path: PathBuf,
    env: Vec<(String, String)>,
    timeout: Option<u64>,
    pub(super) detach: bool,
}
impl Spec {
    /// The one-line subject the approval view and notices show.
    pub(super) fn subject(&self) -> String {
        format!("{} (in {})", quote(&self.argv), self.cwd_path.display())
    }
    /// Everything the host approves.
    pub(super) fn preview(&self) -> Value {
        let mut lines = vec![
            "Runs on the host as you, outside the sandbox.".to_string(),
            format!("Command: {}", quote(&self.argv)),
            format!("Directory: {}", self.cwd_path.display()),
        ];
        if !self.env.is_empty() {
            let env: Vec<_> = self
                .env
                .iter()
                .map(|(k, v)| format!("{k}={}", quote(std::slice::from_ref(v))))
                .collect();
            lines.push(format!("Environment: {}", env.join(" ")));
        }
        lines.push(match self.timeout {
            Some(t) => format!("Time limit: {t} s"),
            None => "Time limit: none".into(),
        });
        json!({"kind":"host-command","description":lines.join("\n"),
            "argv":self.argv,"cwd":self.cwd_path,"env":self.env,"timeout":self.timeout})
    }
}

/// Shell-style quoting for display only; nothing is run through a shell.
fn quote(words: &[String]) -> String {
    words
        .iter()
        .map(|w| {
            if !w.is_empty()
                && w.bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b"-_./=:,+@%".contains(&b))
            {
                w.clone()
            } else {
                format!("'{}'", w.replace('\'', "'\\''"))
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

pub(super) struct Job {
    request: String,
    session: String,
    /// The attached request waiting for the outcome.
    connection: Option<u64>,
    child: Child,
    /// Dropping it stops the job.
    lifetime: Option<OwnedFd>,
    directory: PathBuf,
    notify: bool,
    /// Why the daemon stopped it, when it did.
    stopped: Option<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct JobId {
    request: String,
}

#[derive(Deserialize, Default)]
struct Outcome {
    exit_code: Option<i32>,
    signal: Option<i32>,
    spawn_error: Option<String>,
    cancelled: bool,
    timed_out: bool,
    truncated: bool,
}

impl Controller {
    /// Check a `host-command` request from `session` before it is shown.
    pub(super) fn host_spec(&self, session: &str, host: &HostCommand) -> Result<Spec, Fault> {
        let a = self.sessions.get(session).ok_or_else(missing)?;
        let inheritance = a.inheritance.as_ref().ok_or_else(conflict)?;
        if !inheritance.host_commands() {
            return Err((
                -32602,
                "this goblin may not run host commands (mkGoblin hostCommands.enable)".into(),
            ));
        }
        let active = self
            .host_jobs
            .iter()
            .filter(|j| j.session == session)
            .count();
        if active >= JOBS {
            return Err((-32003, format!("at most {JOBS} host commands run at once")));
        }
        let (cwd, cwd_path) = inheritance
            .host_directory(std::path::Path::new(&host.cwd))
            .map_err(|e| (-32602, e.to_string()))?;
        Ok(Spec {
            argv: host.argv.clone(),
            cwd,
            cwd_path,
            env: host.env.clone(),
            timeout: host.timeout,
            detach: host.detach,
        })
    }

    /// The host's decision on a pending host command; approval starts it.
    pub(super) fn decide_host(&mut self, p: super::Decision, actor: &str) -> Result<Value, Fault> {
        let a = self.sessions.get_mut(&p.session).ok_or_else(missing)?;
        let pending = a.pending.take().ok_or_else(conflict)?;
        let spec = pending.host.ok_or_else(conflict)?;
        if let Some(r) = self.permission_mut(&p.request) {
            r.approved = Some(p.approved);
            r.state = if p.approved { "running" } else { "denied" }.into();
        }
        self.operation(
            &p.request,
            actor,
            crate::operation::Change::Decided {
                approved: p.approved,
            },
        );
        if !p.approved {
            self.reply_permission(&p.request, "denied", None);
            self.operation_finished(&p.request, pending.notify, None);
        } else {
            let connection = (!pending.detached).then_some(pending.connection);
            if let Err(e) =
                self.start_host_job(&p.session, &p.request, spec, connection, pending.notify)
            {
                let message = format!("could not start the host command: {e}");
                if let Some(r) = self.permission_mut(&p.request) {
                    r.state = "failed".into();
                    r.message = Some(message.clone());
                }
                self.reply_permission(&p.request, "failed", Some(message.clone()));
                self.operation_finished(&p.request, pending.notify, Some(message));
            }
        }
        self.dirty = true;
        Ok(json!({"accepted":true,"request":p.request,"approved":p.approved}))
    }

    /// Start an approved job. The outcome arrives through `poll_host_jobs`.
    pub(super) fn start_host_job(
        &mut self,
        session: &str,
        request: &str,
        spec: Spec,
        connection: Option<u64>,
        notify: bool,
    ) -> crate::Result<()> {
        let a = self.sessions.get(session).ok_or("the goblin has stopped")?;
        let helper = a
            .inheritance
            .as_ref()
            .ok_or("the goblin has stopped")?
            .helper()
            .to_path_buf();
        let jobs = a.directory.join("resources/host-jobs");
        prune(&jobs);
        let directory = jobs.join(request);
        fs::create_dir(&directory)?;
        let (lifetime_read, lifetime_write) = crate::unix::pipe()?;
        let mut command = Command::new(helper);
        command
            .arg("--host-job")
            .arg(directory.join("output.log"))
            .arg(directory.join("status.json"))
            .arg(LOG_LIMIT.to_string())
            .arg(spec.timeout.unwrap_or(0).to_string())
            .arg("--")
            .args(&spec.argv)
            // Temporary files go straight to /tmp unless the request says
            // otherwise: nested daemons and nix develop put sockets beneath
            // TMPDIR, and Unix socket paths are limited to 107 bytes.
            .env_remove("TMPDIR")
            .envs(spec.env.iter().map(|(k, v)| (k, v)))
            .env("GOBLINS_HOST_JOB", request)
            .stdin(Stdio::from(lifetime_read))
            .stdout(Stdio::null())
            .stderr(File::create(directory.join("supervisor.log"))?);
        let cwd = spec.cwd.as_raw_fd();
        let parent = unsafe { libc::getpid() };
        unsafe {
            command.pre_exec(move || {
                // Lifetime: the pipe closes with the daemon, but also stop
                // promptly if the daemon dies before reaching it.
                crate::unix::cvt(libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGTERM))?;
                if libc::getppid() != parent {
                    return Err(std::io::Error::from_raw_os_error(libc::ESRCH));
                }
                crate::unix::cvt(libc::fchdir(cwd))?;
                // Only stdio crosses: no daemon socket or session descriptor.
                crate::unix::cvt(libc::syscall(libc::SYS_close_range, 3u32, u32::MAX, 4u32) as i32)?;
                Ok(())
            });
        }
        let child = command.spawn();
        drop(command);
        let child = child?;
        self.host_jobs.push(Job {
            request: request.to_string(),
            session: session.to_string(),
            connection,
            child,
            lifetime: Some(lifetime_write),
            directory,
            notify,
            stopped: None,
        });
        Ok(())
    }

    /// Finish jobs whose supervisor has exited.
    pub(super) fn poll_host_jobs(&mut self) {
        let mut index = 0;
        while index < self.host_jobs.len() {
            if matches!(self.host_jobs[index].child.try_wait(), Ok(None)) {
                index += 1;
                continue;
            }
            let job = self.host_jobs.remove(index);
            self.finish_host_job(job);
        }
    }

    fn finish_host_job(&mut self, mut job: Job) {
        drop(job.lifetime.take());
        let _ = job.child.wait();
        let outcome: Option<Outcome> = fs::read(job.directory.join("status.json"))
            .ok()
            .and_then(|bytes| serde_json::from_slice(&bytes).ok());
        let (state, message, code) = match (&outcome, &job.stopped) {
            (None, Some(why)) => ("cancelled", why.clone(), None),
            (None, None) => (
                "failed",
                format!(
                    "the host command supervisor failed: {}",
                    super::bounded(&crate::process::diagnostic(
                        &job.directory.join("supervisor.log")
                    ))
                ),
                None,
            ),
            (Some(o), _) if o.spawn_error.is_some() => {
                let error = o.spawn_error.clone().unwrap();
                let code = match error.as_str() {
                    "not found" => 127,
                    "not executable" => 126,
                    _ => 125,
                };
                ("failed", format!("the command was {error}"), Some(code))
            }
            (Some(o), stopped) => {
                let mut message = match (o.exit_code, o.signal) {
                    (Some(c), _) => format!("exit code {c}"),
                    (None, Some(s)) => format!("ended by signal {s}"),
                    (None, None) => "ended".into(),
                };
                if o.timed_out {
                    message = format!("timed out; {message}");
                } else if let Some(why) = stopped.as_ref().filter(|_| o.cancelled) {
                    message = format!("{why}; {message}");
                }
                if o.truncated {
                    message.push_str("; output truncated");
                }
                let code = o.exit_code.or(o.signal.map(|s| 128 + s));
                (
                    if o.cancelled { "cancelled" } else { "finished" },
                    message,
                    code,
                )
            }
        };
        let result = json!({"request":job.request,"status":state,"message":message,
            "exit_code":outcome.as_ref().and_then(|o| o.exit_code),
            "signal":outcome.as_ref().and_then(|o| o.signal),
            "code":code,
            "log":format!("/run/goblins/host-jobs/{}/output.log", job.request)});
        self.host_outcomes
            .insert(job.request.clone(), result.clone());
        if let Some(r) = self.permission_mut(&job.request) {
            r.state = state.into();
            r.message = Some(message.clone());
        }
        // The attached request waits under the request ID.
        for c in &mut self.connections {
            if let Some((waiting, rpc_id)) = &c.waiting
                && *waiting == job.request
            {
                c.queue(rpc::result(rpc_id.clone(), result.clone()));
                c.close = true;
            }
        }
        self.operation_finished(&job.request, job.notify, Some(message));
        self.dirty = true;
    }

    /// Stop `request`'s job; its outcome follows when the supervisor exits.
    fn stop_host_job(&mut self, request: &str, why: &str) -> bool {
        match self.host_jobs.iter_mut().find(|j| j.request == request) {
            Some(job) => {
                job.lifetime.take();
                job.stopped.get_or_insert_with(|| why.to_string());
                true
            }
            None => false,
        }
    }

    /// The attached caller went away: it no longer wants its job.
    pub(super) fn host_withdraw(&mut self, connection: u64) {
        let requests: Vec<_> = self
            .host_jobs
            .iter()
            .filter(|j| j.connection == Some(connection))
            .map(|j| j.request.clone())
            .collect();
        for request in requests {
            self.stop_host_job(&request, "its request connection closed");
        }
    }

    /// The goblin stopped: so do its jobs. Nobody is left to notify.
    pub(super) fn end_host_jobs(&mut self, session: &str) {
        for job in self.host_jobs.iter_mut().filter(|j| j.session == session) {
            job.notify = false;
            job.lifetime.take();
            job.stopped
                .get_or_insert_with(|| "the goblin stopped".into());
        }
    }

    /// `host.status`, `host.list` and `host.cancel` from `session`, or
    /// `host.cancel` from the host (`session` None).
    pub(super) fn host_control(
        &mut self,
        session: Option<&str>,
        method: &str,
        value: Value,
    ) -> Result<Value, Fault> {
        let view = |c: &Controller, r: &super::PermissionRecord| -> Value {
            let mut v = c.host_outcomes.get(&r.id).cloned().unwrap_or_else(|| {
                json!({"request":r.id,"status":r.state,"message":r.message,
                    "log":format!("/run/goblins/host-jobs/{}/output.log", r.id)})
            });
            v["status"] = json!(r.state);
            v["subject"] = json!(r.package);
            v["reason"] = json!(r.reason);
            v
        };
        let mine = |r: &super::PermissionRecord| {
            r.kind == "host-command" && session.is_none_or(|s| r.session == s)
        };
        match method {
            "host.list" => {
                let _: super::Empty = params(value)?;
                Ok(json!({"jobs":self.permissions.iter().filter(|r| mine(r))
                    .map(|r| view(self, r)).collect::<Vec<_>>()}))
            }
            "host.status" => {
                let p: JobId = params(value)?;
                let r = self
                    .permissions
                    .iter()
                    .find(|r| r.id == p.request && mine(r))
                    .ok_or((-32004, format!("no host command {}", p.request)))?;
                Ok(view(self, r))
            }
            "host.cancel" => {
                let p: JobId = params(value)?;
                let r = self
                    .permissions
                    .iter()
                    .find(|r| r.id == p.request && mine(r))
                    .ok_or((-32004, format!("no host command {}", p.request)))?;
                let (owner, state) = (r.session.clone(), r.state.clone());
                let actor = if session.is_some() {
                    "the goblin"
                } else {
                    "the host"
                };
                if state == "pending" {
                    let a = self.sessions.get_mut(&owner).ok_or_else(missing)?;
                    let pending = a
                        .pending
                        .take_if(|pending| pending.id == p.request)
                        .ok_or_else(conflict)?;
                    pending.cancel.cancel();
                    if let Some(r) = self.permission_mut(&p.request) {
                        r.state = "withdrawn".into();
                    }
                    let message = format!("withdrawn by {actor}");
                    self.reply_permission(&p.request, "withdrawn", Some(message.clone()));
                    self.operation_finished(&p.request, pending.notify, Some(message));
                    self.dirty = true;
                    return Ok(json!({"accepted":true,"request":p.request}));
                }
                if !self.stop_host_job(&p.request, &format!("cancelled by {actor}")) {
                    return Err((-32009, format!("host command {} is {state}", p.request)));
                }
                Ok(json!({"accepted":true,"request":p.request}))
            }
            _ => Err((-32601, "unknown host command method".into())),
        }
    }
}

/// Keep the output of the most recent finished jobs only.
fn prune(jobs: &std::path::Path) {
    let Ok(entries) = fs::read_dir(jobs) else {
        return;
    };
    let mut finished: Vec<_> = entries
        .filter_map(|e| e.ok())
        .filter(|e| e.path().join("status.json").exists())
        .filter_map(|e| Some((e.metadata().ok()?.modified().ok()?, e.path())))
        .collect();
    finished.sort();
    let excess = finished.len().saturating_sub(KEPT - 1);
    for (_, path) in finished.into_iter().take(excess) {
        let _ = fs::remove_dir_all(path);
    }
}
