//! `terminals.*`: named, long-lived terminals in a sandbox. The daemon keeps
//! each one's record; its supervisor owns the PTY and emulates the screen
//! (`Command::Spawn` with `SpawnKind::Terminal`), so a terminal runs with
//! nobody attached and answers reads, input and waits.
use super::exec::validate_command;
use super::{Active, Connection, Controller, Fault, capacity, conflict, params};
use goblins_jsonrpc as rpc;
use goblins_runtime::policy::Operation;
use goblins_runtime::terminal::{self, Call, Condition, Source};
use goblins_runtime::{Command, Event, SpawnKind, Stdio, TerminalId, exec};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{os::fd::OwnedFd, os::unix::net::UnixStream};

/// What a snapshot's `Session.terminals` and `terminals.list` hold for one
/// terminal.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct TerminalRecord {
    /// `t1`, `t2`, …: counting up for the goblin's run, never reused in it.
    pub id: String,
    pub name: Option<String>,
    /// For display: the name, else the program's name.
    pub label: String,
    /// `host` or `agent`: where it was created.
    pub origin: String,
    /// Unix milliseconds.
    pub started_at: u64,
    pub rows: u16,
    pub cols: u16,
    /// A frontend is attached.
    pub attached: bool,
}

/// A host connection waiting for its terminal to start.
struct Create {
    connection: u64,
    rpc: Value,
}

pub(super) struct Entry {
    pub(super) record: TerminalRecord,
    /// The ID of its process in the sandbox.
    pub(super) exec: u64,
    /// Until it has started, its creation waits for the reply.
    create: Option<Create>,
    started: bool,
}

/// One goblin's terminals.
#[derive(Default)]
pub(super) struct Set {
    next: u64,
    pub(super) entries: Vec<Entry>,
}

impl Set {
    pub(super) fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
    /// The started terminal an ID or name designates.
    fn find(&self, designator: &str) -> Option<usize> {
        self.entries.iter().position(|e| {
            e.started && (e.record.id == designator || e.record.name.as_deref() == Some(designator))
        })
    }
    fn by_exec(&self, exec: u64) -> Option<usize> {
        self.entries.iter().position(|e| e.exec == exec)
    }
    fn records(&self) -> Vec<TerminalRecord> {
        self.entries
            .iter()
            .filter(|e| e.started)
            .map(|e| e.record.clone())
            .collect()
    }
}

/// A call whose answer the supervisor owes a connection.
pub(super) struct Pending {
    session: String,
    connection: u64,
    rpc: Value,
    wait: bool,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Create1 {
    session: String,
    #[serde(default)]
    argv: Option<Vec<String>>,
    #[serde(default)]
    cwd: Option<String>,
    #[serde(default)]
    env: Vec<(String, String)>,
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    rows: Option<u16>,
    #[serde(default)]
    cols: Option<u16>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Session {
    session: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Target {
    session: String,
    terminal: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Read {
    session: String,
    terminal: String,
    #[serde(default)]
    source: Option<String>,
    #[serde(default)]
    lines: Option<usize>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Send {
    session: String,
    terminal: String,
    #[serde(default)]
    text: Option<String>,
    #[serde(default)]
    keys: Vec<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Wait {
    session: String,
    terminal: String,
    #[serde(default, rename = "match")]
    text: Option<String>,
    #[serde(default)]
    regex: Option<String>,
    #[serde(default)]
    exit: Option<bool>,
    #[serde(default)]
    timeout: Option<u64>,
    #[serde(default)]
    lines: Option<usize>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Resize {
    session: String,
    terminal: String,
    rows: u16,
    cols: u16,
}

fn invalid(message: &str) -> Fault {
    (-32602, message.into())
}

/// A program's name for display: the last component of its path.
fn label(argv: &Option<Vec<String>>) -> String {
    argv.as_ref()
        .and_then(|argv| argv.first())
        .and_then(|program| program.rsplit('/').find(|part| !part.is_empty()))
        .unwrap_or("shell")
        .to_string()
}

impl Active {
    /// Publish the terminals in the goblin's record; true when it changed.
    fn sync_terminals(&mut self) -> bool {
        let records = self.terminals.records();
        let changed = self.record.terminals != records;
        self.record.terminals = records;
        changed
    }
    /// Attach a frontend's stream to terminal `designator`, or refuse it.
    pub(super) fn attach_terminal(
        &mut self,
        mut stream: UnixStream,
        designator: &str,
        size: (u16, u16),
    ) {
        let refused = (|| {
            if self.record.state != "running" {
                return Err("the goblin is not running".to_string());
            }
            self.allow(&Operation::Exec).map_err(|(_, m)| m)?;
            if !terminal::valid_size(size.0, size.1) {
                return Err(format!(
                    "terminal sizes are 1 to {} rows and columns",
                    terminal::MAX_SIZE
                ));
            }
            let index = self
                .terminals
                .find(designator)
                .ok_or_else(|| format!("no terminal {designator}"))?;
            if self.terminals.entries[index].record.attached {
                return Err("terminal is already attached".into());
            }
            Ok(index)
        })();
        let index = match refused {
            Ok(index) => index,
            Err(message) => {
                super::attach::refuse(&mut stream, &message);
                return;
            }
        };
        let exec = self.terminals.entries[index].exec;
        let Some(runtime) = self.runtime.as_mut() else {
            super::attach::refuse(&mut stream, "the goblin has stopped");
            return;
        };
        // Reserve before sending, so another hello cannot take it.
        let entry = &mut self.terminals.entries[index];
        entry.record.attached = true;
        (entry.record.rows, entry.record.cols) = size;
        if let Err(Command::Attach { stream, .. }) = runtime.send(Command::Attach {
            terminal: TerminalId::Process(exec),
            stream: OwnedFd::from(stream).into(),
            rows: size.0,
            cols: size.1,
        }) {
            let mut stream = UnixStream::from(OwnedFd::from(stream));
            super::attach::refuse(&mut stream, "the goblin cannot take more work right now");
            self.terminals.entries[index].record.attached = false;
        }
        self.sync_terminals();
    }
}

impl Controller {
    /// Resolve a terminal of a running goblin.
    fn terminal_target(
        &self,
        session: &str,
        terminal: &str,
    ) -> Result<(String, usize), Fault> {
        let id = self.exec_target(session, None)?;
        let index = self.sessions[&id].terminals.find(terminal).ok_or_else(|| {
            (
                -32004,
                format!("no terminal {terminal} in goblin {session}"),
            )
        })?;
        Ok((id, index))
    }
    /// The `terminals.*` methods of the host endpoint. A call answered by the
    /// supervisor returns None and is answered when the supervisor does.
    pub(super) fn host_terminals(
        &mut self,
        c: &mut Connection,
        rpc_id: Value,
        method: &str,
        value: Value,
    ) -> Result<Option<Value>, Fault> {
        if self.shutdown.is_some() {
            return Err((-32009, "server is stopping".into()));
        }
        if c.waiting.is_some() {
            return Err(conflict());
        }
        match method {
            "terminals.create" => self.terminal_create(c, rpc_id, params(value)?),
            "terminals.list" => {
                let p: Session = params(value)?;
                let id = self.resolve_session(&p.session)?;
                Ok(Some(json!({"terminals": self.sessions[&id].record.terminals})))
            }
            "terminals.read" => {
                let p: Read = params(value)?;
                let source = match p.source.as_deref() {
                    None | Some("visible") => Source::Visible,
                    Some("recent") => Source::Recent,
                    Some(_) => return Err(invalid("source is visible or recent")),
                };
                let call = Call::Read {
                    source,
                    lines: p.lines,
                };
                self.terminal_call(c, rpc_id, &p.session, &p.terminal, call)
            }
            "terminals.send" => {
                let p: Send = params(value)?;
                let call = Call::Send {
                    text: p.text,
                    keys: p.keys,
                };
                self.terminal_call(c, rpc_id, &p.session, &p.terminal, call)
            }
            "terminals.wait" => {
                let p: Wait = params(value)?;
                let condition = match (p.text, p.regex, p.exit) {
                    (Some(text), None, None | Some(false)) => Condition::Match(text),
                    (None, Some(regex), None | Some(false)) => Condition::Regex(regex),
                    (None, None, Some(true)) => Condition::Exit,
                    _ => return Err(invalid("give exactly one of match, regex and exit")),
                };
                let call = Call::Wait {
                    condition,
                    timeout: p.timeout.unwrap_or(terminal::DEFAULT_TIMEOUT),
                    lines: p.lines,
                };
                self.terminal_call(c, rpc_id, &p.session, &p.terminal, call)
            }
            "terminals.close" => {
                let p: Target = params(value)?;
                let (session, index) = self.terminal_target(&p.session, &p.terminal)?;
                let a = self.sessions.get_mut(&session).unwrap();
                let exec = a.terminals.entries[index].exec;
                // As a withdrawn exec is cancelled: its process group.
                a.runtime
                    .as_mut()
                    .ok_or_else(conflict)?
                    .send(Command::CancelProcess {
                        id: exec,
                        everything: false,
                    })
                    .map_err(|_| capacity())?;
                Ok(Some(json!({"accepted":true})))
            }
            "terminals.detach" => {
                let p: Target = params(value)?;
                let (session, index) = self.terminal_target(&p.session, &p.terminal)?;
                let a = self.sessions.get_mut(&session).unwrap();
                let entry = &mut a.terminals.entries[index];
                if !entry.record.attached {
                    return Err((-32009, "terminal is not attached".into()));
                }
                let exec = entry.exec;
                a.runtime
                    .as_mut()
                    .ok_or_else(conflict)?
                    .send(Command::Detach {
                        terminal: TerminalId::Process(exec),
                    })
                    .map_err(|_| capacity())?;
                a.terminals.entries[index].record.attached = false;
                a.sync_terminals();
                self.dirty = true;
                Ok(Some(json!({"accepted":true})))
            }
            "terminals.resize" => {
                let p: Resize = params(value)?;
                if !terminal::valid_size(p.rows, p.cols) {
                    return Err(invalid("invalid dimensions"));
                }
                let (session, index) = self.terminal_target(&p.session, &p.terminal)?;
                let a = self.sessions.get_mut(&session).unwrap();
                let exec = a.terminals.entries[index].exec;
                a.runtime
                    .as_mut()
                    .ok_or_else(conflict)?
                    .send(Command::Resize {
                        terminal: TerminalId::Process(exec),
                        rows: p.rows,
                        cols: p.cols,
                    })
                    .map_err(|_| capacity())?;
                let record = &mut a.terminals.entries[index].record;
                (record.rows, record.cols) = (p.rows, p.cols);
                a.sync_terminals();
                self.dirty = true;
                Ok(Some(json!({"accepted":true})))
            }
            _ => Err((-32601, "method unavailable on host endpoint".into())),
        }
    }
    fn terminal_create(
        &mut self,
        c: &mut Connection,
        rpc_id: Value,
        p: Create1,
    ) -> Result<Option<Value>, Fault> {
        validate_command(&p.argv, &p.cwd, &p.env)?;
        if let Some(name) = &p.name
            && !terminal::valid_name(name)
        {
            return Err(invalid(
                "a terminal's name is a lowercase letter, then letters, digits, - and _, at most 32 in all, and not t followed by digits",
            ));
        }
        let (rows, cols) = match (p.rows, p.cols) {
            (None, None) => terminal::DEFAULT_SIZE,
            (rows, cols) => (
                rows.unwrap_or(terminal::DEFAULT_SIZE.0),
                cols.unwrap_or(terminal::DEFAULT_SIZE.1),
            ),
        };
        if !terminal::valid_size(rows, cols) {
            return Err(invalid(&format!(
                "terminal sizes are 1 to {} rows and columns",
                terminal::MAX_SIZE
            )));
        }
        let session = self.exec_target(&p.session, None)?;
        self.sessions[&session].allow(&Operation::Exec)?;
        let a = &self.sessions[&session];
        if !a.init {
            return Err((-32009, "the sandbox has no init channel".to_string()));
        }
        if a.terminals.entries.len() >= terminal::MAX_TERMINALS {
            return Err((
                -32010,
                format!(
                    "a goblin has at most {} terminals",
                    terminal::MAX_TERMINALS
                ),
            ));
        }
        if let Some(name) = &p.name
            && a.terminals
                .entries
                .iter()
                .any(|e| e.record.name.as_deref() == Some(name))
        {
            return Err((-32009, format!("a terminal named {name} exists")));
        }
        let exec = self.number();
        exec::validate_spawn(exec, &p.argv, &p.cwd, &p.env, true)
            .map_err(|message| invalid(&message))?;
        let a = self.sessions.get_mut(&session).unwrap();
        a.runtime
            .as_mut()
            .ok_or_else(conflict)?
            .send(Command::Spawn {
                id: exec,
                kind: SpawnKind::Terminal,
                argv: p.argv.clone(),
                cwd: p.cwd,
                env: p.env,
                stdio: Stdio::NewPty { rows, cols },
            })
            .map_err(|_| capacity())?;
        a.terminals.next += 1;
        let id = format!("t{}", a.terminals.next);
        a.terminals.entries.push(Entry {
            record: TerminalRecord {
                label: p.name.clone().unwrap_or_else(|| label(&p.argv)),
                id,
                name: p.name,
                origin: "host".into(),
                started_at: 0,
                rows,
                cols,
                attached: false,
            },
            exec,
            create: Some(Create {
                connection: c.id,
                rpc: rpc_id.clone(),
            }),
            started: false,
        });
        c.waiting = Some((format!("terminal-{exec}"), rpc_id));
        Ok(None)
    }
    fn terminal_call(
        &mut self,
        c: &mut Connection,
        rpc_id: Value,
        session: &str,
        designator: &str,
        call: Call,
    ) -> Result<Option<Value>, Fault> {
        terminal::validate(&call).map_err(|message| invalid(&message))?;
        let (session, index) = self.terminal_target(session, designator)?;
        let wait = matches!(call, Call::Wait { .. });
        if wait
            && self
                .terminal_calls
                .values()
                .filter(|p| p.wait && p.session == session)
                .count()
                >= terminal::MAX_WAITS
        {
            return Err((
                -32010,
                format!("a goblin has at most {} waits", terminal::MAX_WAITS),
            ));
        }
        let request = self.number();
        let a = self.sessions.get_mut(&session).unwrap();
        let terminal = a.terminals.entries[index].exec;
        a.runtime
            .as_mut()
            .ok_or_else(conflict)?
            .send(Command::TerminalCall {
                request,
                terminal,
                call,
            })
            .map_err(|_| capacity())?;
        self.terminal_calls.insert(
            request,
            Pending {
                session,
                connection: c.id,
                rpc: rpc_id.clone(),
                wait,
            },
        );
        c.waiting = Some((format!("terminal-call-{request}"), rpc_id));
        Ok(None)
    }
    /// Answer the connection that waits on `rpc`.
    fn terminal_answer(&mut self, connection: u64, rpc: Value, result: Result<Value, Fault>) {
        if let Some(c) = self.connections.iter_mut().find(|c| c.id == connection) {
            c.waiting = None;
            c.queue(match result {
                Ok(value) => rpc::result(rpc, value),
                Err((code, message)) => rpc::error(rpc, code, &message),
            });
        }
    }
    /// A process event of goblin `session`'s runtime, if it concerns one of
    /// its terminals; given back when it does not.
    pub(super) fn terminal_event(&mut self, session: &str, event: Event) -> Option<Event> {
        match event {
            Event::TerminalResult { request, result } => {
                if let Some(call) = self.terminal_calls.remove(&request) {
                    self.terminal_answer(
                        call.connection,
                        call.rpc,
                        result.map_err(|message| (-32009, message)),
                    );
                }
                None
            }
            Event::Spawned { id, .. }
                if self
                    .sessions
                    .get(session)
                    .is_some_and(|a| a.terminals.by_exec(id).is_some()) =>
            {
                let a = self.sessions.get_mut(session).unwrap();
                let index = a.terminals.by_exec(id).unwrap();
                let entry = &mut a.terminals.entries[index];
                entry.started = true;
                entry.record.started_at = crate::messaging::now();
                let create = entry.create.take();
                let record = entry.record.clone();
                a.sync_terminals();
                self.dirty = true;
                if let Some(create) = create {
                    self.terminal_answer(create.connection, create.rpc, Ok(json!(record)));
                }
                None
            }
            Event::SpawnFailed { id, code, message }
                if self
                    .sessions
                    .get(session)
                    .is_some_and(|a| a.terminals.by_exec(id).is_some()) =>
            {
                let a = self.sessions.get_mut(session).unwrap();
                let index = a.terminals.by_exec(id).unwrap();
                let entry = a.terminals.entries.remove(index);
                a.sync_terminals();
                self.dirty = true;
                if let Some(create) = entry.create {
                    let fault = match code {
                        Some(_) => (-32009, message),
                        None => (-32010, message),
                    };
                    self.terminal_answer(create.connection, create.rpc, Err(fault));
                }
                None
            }
            Event::Exited { id, .. }
                if self
                    .sessions
                    .get(session)
                    .is_some_and(|a| a.terminals.by_exec(id).is_some()) =>
            {
                let a = self.sessions.get_mut(session).unwrap();
                let index = a.terminals.by_exec(id).unwrap();
                a.terminals.entries.remove(index);
                a.sync_terminals();
                self.dirty = true;
                None
            }
            Event::Terminal {
                terminal: TerminalId::Process(id),
                status,
                ..
            } if self
                .sessions
                .get(session)
                .is_some_and(|a| a.terminals.by_exec(id).is_some()) =>
            {
                let a = self.sessions.get_mut(session).unwrap();
                let index = a.terminals.by_exec(id).unwrap();
                a.terminals.entries[index].record.attached = status.attached;
                if a.sync_terminals() {
                    self.dirty = true;
                }
                None
            }
            event => Some(event),
        }
    }
    /// The sandbox stopped or its init went away: no terminal is left, and
    /// no call on one will be answered.
    pub(super) fn end_terminals(&mut self, session: &str, message: &str) {
        let requests: Vec<_> = self
            .terminal_calls
            .iter()
            .filter(|(_, p)| p.session == session)
            .map(|(request, _)| *request)
            .collect();
        for request in requests {
            if let Some(call) = self.terminal_calls.remove(&request) {
                self.terminal_answer(call.connection, call.rpc, Err((-32009, message.into())));
            }
        }
        let Some(a) = self.sessions.get_mut(session) else {
            return;
        };
        let entries = std::mem::take(&mut a.terminals.entries);
        let changed = a.sync_terminals();
        for create in entries.into_iter().filter_map(|e| e.create) {
            self.terminal_answer(create.connection, create.rpc, Err((-32009, message.into())));
        }
        self.dirty |= changed;
    }
    /// A connection went away: its calls need no answer, and a wait no
    /// longer needs the supervisor's attention.
    pub(super) fn terminal_withdraw(&mut self, connection: u64) {
        let requests: Vec<_> = self
            .terminal_calls
            .iter()
            .filter(|(_, p)| p.connection == connection)
            .map(|(request, _)| *request)
            .collect();
        for request in requests {
            let Some(call) = self.terminal_calls.remove(&request) else {
                continue;
            };
            if call.wait
                && let Some(runtime) = self
                    .sessions
                    .get_mut(&call.session)
                    .and_then(|a| a.runtime.as_mut())
            {
                let _ = runtime.send(Command::TerminalCancel { request });
            }
        }
        // A terminal whose creator left is still created.
        for a in self.sessions.values_mut() {
            for entry in &mut a.terminals.entries {
                if entry.create.as_ref().is_some_and(|c| c.connection == connection) {
                    entry.create = None;
                }
            }
        }
    }
}
