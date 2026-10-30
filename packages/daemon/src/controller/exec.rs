//! `sessions.exec`: commands the host runs inside a running sandbox through
//! its goblins-init. The client passes stdio or a PTY slave with SCM_RIGHTS
//! and keeps its connection open for the exec's whole life; when that
//! connection closes, init cancels the exec.
use super::{
    Connection, Controller, Fault, capacity, conflict, params,
    policy::Operation,
    runtime::{Command, Event, SpawnKind, Stdio},
};
use goblins_init as init;
use goblins_jsonrpc as rpc;
use serde::Deserialize;
use serde_json::{Value, json};

/// Live execs per sandbox; TRAMP, eshell and magit open one per command.
const EXECS: usize = 64;

pub(super) struct Exec {
    session: String,
    connection: u64,
    rpc: Value,
    /// An interactive shell rather than a TRAMP, eshell or script command.
    interactive: bool,
    argv: Option<Vec<String>>,
    cwd: Option<String>,
    started: bool,
}

impl Exec {
    pub(super) fn session(&self) -> &str {
        &self.session
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct ExecParams {
    /// Agent name or session ID.
    session: String,
    /// Require exactly this session ID; never re-resolve the name.
    #[serde(default)]
    pin: Option<String>,
    /// The sandbox's `$SHELL` when absent.
    #[serde(default)]
    argv: Option<Vec<String>>,
    #[serde(default)]
    cwd: Option<String>,
    #[serde(default)]
    env: Vec<(String, String)>,
    #[serde(default)]
    tty: bool,
    /// Asked for a terminal explicitly (`-t`): an interactive shell, counted
    /// and logged as one. TRAMP's terminals are not.
    #[serde(default)]
    interactive: bool,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Signal {
    signal: i32,
}

fn invalid(message: &str) -> Fault {
    (-32602, message.into())
}

impl ExecParams {
    fn validate(&self) -> Result<(), Fault> {
        let text = |s: &String| !s.contains('\0') && s.len() <= 65536;
        if let Some(argv) = &self.argv
            && (argv.is_empty()
                || argv.len() > goblins_protocol::MAX_ARGS
                || !argv.iter().all(text))
        {
            return Err(invalid("invalid command"));
        }
        if let Some(cwd) = &self.cwd
            && (!cwd.starts_with('/') || !text(cwd) || cwd.len() > 4096)
        {
            return Err(invalid("the working directory must be an absolute path"));
        }
        if self.env.len() > goblins_protocol::MAX_ENV
            || !self
                .env
                .iter()
                .all(|(k, v)| goblins_protocol::env_name(k) && text(v))
        {
            return Err(invalid("invalid environment variable"));
        }
        Ok(())
    }
}

impl Controller {
    /// Resolve an exec target. A pin names the exact session: a stopped or
    /// different goblin fails instead of falling back to the name.
    fn exec_target(&self, designator: &str, pin: Option<&str>) -> Result<String, Fault> {
        let id = match pin {
            Some(pin) => {
                let a = self.sessions.get(pin).ok_or_else(|| {
                    (
                        -32009,
                        format!("goblin {designator} ({pin}) has stopped or is not on this daemon"),
                    )
                })?;
                if ![pin, a.record.agent_name.as_str()].contains(&designator) {
                    return Err((-32009, format!("goblin {designator} is not session {pin}")));
                }
                pin.to_string()
            }
            None => self.resolve_session(designator)?,
        };
        let a = &self.sessions[&id];
        if a.record.state != "running" || a.runtime.is_none() {
            return Err((
                -32009,
                format!("goblin {} ({id}) has stopped", a.record.agent_name),
            ));
        }
        Ok(id)
    }
    /// Start an exec for host connection `c`. The reply waits for its exit.
    pub(super) fn exec_start(
        &mut self,
        c: &mut Connection,
        rpc_id: Value,
        value: Value,
    ) -> Result<(), Fault> {
        // Descriptors belong to this call only, whatever its outcome.
        let fds = std::mem::take(&mut c.fds);
        if self.shutdown.is_some() || c.exec.is_some() {
            return Err(conflict());
        }
        let p: ExecParams = params(value)?;
        p.validate()?;
        if fds.len() != if p.tty { 1 } else { 3 } {
            return Err(invalid(
                "pass a PTY slave with tty, or stdin, stdout and stderr",
            ));
        }
        let session = self.exec_target(&p.session, p.pin.as_deref())?;
        self.sessions[&session].allow(&Operation::Exec)?;
        if self.execs.values().filter(|e| e.session == session).count() >= EXECS {
            return Err(capacity());
        }
        let id = self.number();
        if !self.sessions[&session].init {
            return Err((-32009, "the sandbox has no init channel".to_string()));
        }
        // What init would be sent must fit its message limit.
        let command = init::Command::Spawn {
            id,
            argv: p.argv.clone(),
            cwd: p.cwd.clone(),
            env: p.env.clone(),
            tty: p.tty,
        };
        let bytes = serde_json::to_vec(&command).map_err(|e| invalid(&e.to_string()))?;
        if bytes.len() > init::MAX_MESSAGE {
            return Err(invalid("command too long"));
        }
        let mut fds = fds.into_iter().map(Into::into);
        let stdio = match (fds.next(), fds.next(), fds.next()) {
            (Some(tty), None, None) => Stdio::Tty(tty),
            (Some(stdin), Some(stdout), Some(stderr)) => Stdio::Fds([stdin, stdout, stderr]),
            _ => unreachable!("the descriptor count was checked"),
        };
        self.sessions
            .get_mut(&session)
            .unwrap()
            .runtime
            .as_mut()
            .ok_or_else(conflict)?
            .send(Command::Spawn {
                id,
                kind: SpawnKind::Exec,
                argv: p.argv.clone(),
                cwd: p.cwd.clone(),
                env: p.env,
                stdio,
            })
            .map_err(|_| capacity())?;
        self.execs.insert(
            id,
            Exec {
                session,
                connection: c.id,
                rpc: rpc_id.clone(),
                interactive: p.interactive && p.tty,
                argv: p.argv,
                cwd: p.cwd,
                started: false,
            },
        );
        c.exec = Some(id);
        c.waiting = Some((format!("exec-{id}"), rpc_id));
        Ok(())
    }
    /// A frame on a connection running an exec: only signal notifications.
    pub(super) fn exec_input(&mut self, c: &mut Connection, id: u64, call: rpc::Call) {
        let signal = (call.id.is_none() && call.method == "exec.signal")
            .then(|| params::<Signal>(call.params).ok())
            .flatten()
            .filter(|s| init::forwardable(s.signal));
        match signal {
            Some(Signal { signal }) => self.exec_command(id, Command::Signal { id, signal }),
            None => c.dead = true,
        }
    }
    /// Send `command` for exec `id` to its sandbox, best effort.
    fn exec_command(&mut self, id: u64, command: Command) {
        let Some(exec) = self.execs.get(&id) else {
            return;
        };
        if let Some(a) = self.sessions.get_mut(&exec.session)
            && a.init
            && let Some(runtime) = &mut a.runtime
        {
            let _ = runtime.send(command);
        }
    }
    /// `sessions.kill_execs`: cancel every exec in a sandbox.
    pub(super) fn kill_execs(&mut self, session: &str) -> Result<Value, Fault> {
        let id = self.resolve_session(&session.replace('.', "/"))?;
        let a = self.sessions.get_mut(&id).unwrap();
        if !a.init {
            return Err(conflict());
        }
        let _ = a
            .runtime
            .as_mut()
            .ok_or_else(conflict)?
            .send(Command::KillAll);
        Ok(json!({"accepted":true}))
    }
    /// The exec's client disconnected: cancel it.
    pub(super) fn exec_withdraw(&mut self, connection: u64) {
        let ids: Vec<_> = self
            .execs
            .iter()
            .filter(|(_, e)| e.connection == connection)
            .map(|(id, _)| *id)
            .collect();
        for id in ids {
            self.exec_command(
                id,
                Command::CancelProcess {
                    id,
                    everything: false,
                },
            );
            self.exec_finished(id, None);
        }
    }
    /// Answer an exec's client, if it is still connected, and forget the exec.
    fn exec_finished(&mut self, id: u64, reply: Option<Result<Value, Fault>>) {
        let Some(exec) = self.execs.remove(&id) else {
            return;
        };
        if exec.started {
            if let Some(a) = self.sessions.get_mut(&exec.session) {
                if exec.interactive {
                    a.record.shells = a.record.shells.saturating_sub(1);
                } else {
                    a.record.execs = a.record.execs.saturating_sub(1);
                }
            }
            self.dirty = true;
        }
        if exec.interactive
            && let Some(a) = self.sessions.get(&exec.session)
        {
            let outcome = match &reply {
                Some(Ok(value)) => value.clone(),
                Some(Err((_, message))) => json!({"error":message}),
                None => json!({"cancelled":true}),
            };
            crate::session::log_event(
                &a.directory.join("resources"),
                "exec-exited",
                json!({"exec":id,"outcome":outcome}),
            );
        }
        let Some(reply) = reply else {
            return;
        };
        if let Some(c) = self
            .connections
            .iter_mut()
            .find(|c| c.id == exec.connection)
        {
            c.waiting = None;
            c.close = true;
            c.queue(match reply {
                Ok(value) => rpc::result(exec.rpc, value),
                Err((code, message)) => rpc::error(exec.rpc, code, &message),
            });
        }
    }
    /// The sandbox stopped or its init went away.
    pub(super) fn end_execs(&mut self, session: &str, message: &str) {
        let ids: Vec<_> = self
            .execs
            .iter()
            .filter(|(_, e)| e.session == session)
            .map(|(id, _)| *id)
            .collect();
        for id in ids {
            self.exec_finished(id, Some(Err((-32009, message.to_string()))));
        }
    }
    fn exec_event(&mut self, session: &str, event: Event) {
        match event {
            Event::Spawned { id, pid, .. } => {
                let Some(exec) = self.execs.get_mut(&id).filter(|e| e.session == session) else {
                    return;
                };
                exec.started = true;
                let (interactive, argv, cwd) =
                    (exec.interactive, exec.argv.clone(), exec.cwd.clone());
                let a = self.sessions.get_mut(session).unwrap();
                if interactive {
                    a.record.shells += 1;
                    // Interactive shells only: TRAMP and eshell would flood
                    // the size-capped log and crowd out lifecycle events.
                    crate::session::log_event(
                        &a.directory.join("resources"),
                        "exec-started",
                        json!({"exec":id,"argv":argv,"cwd":cwd,"pid":pid}),
                    );
                } else {
                    a.record.execs += 1;
                }
                self.dirty = true;
            }
            Event::SpawnFailed { id, code, message }
                if self.execs.get(&id).is_some_and(|e| e.session == session) =>
            {
                // Init answered, or could not be asked.
                let reply = match code {
                    Some(code) => Ok(json!({"code":code,"message":message})),
                    None => Err((-32010, message)),
                };
                self.exec_finished(id, Some(reply));
            }
            Event::Exited { id, code, signal }
                if self.execs.get(&id).is_some_and(|e| e.session == session) =>
            {
                self.exec_finished(id, Some(Ok(json!({"code":code,"signal":signal}))));
            }
            _ => (),
        }
    }
    /// A process event from goblin `session`'s runtime: for an attach
    /// client, an exec, or the loss of init.
    pub(super) fn process_event(&mut self, session: &str, event: Event) {
        match event {
            Event::InitLost => {
                if let Some(a) = self.sessions.get_mut(session) {
                    a.init = false;
                }
                self.end_execs(session, "the goblin has stopped");
                self.attach_lost(session);
            }
            event => {
                if let Some(event) = self.attach_event(session, event) {
                    self.exec_event(session, event);
                }
            }
        }
    }
}
