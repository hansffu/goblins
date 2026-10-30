//! `editor.open`: a sandbox asks the host editor to open its files. The
//! daemon builds every file name itself, always as the goblin's TRAMP path,
//! and runs the configured emacsclient without a shell. It first pins that
//! TRAMP host in Emacs to the requesting session, so the files can only come
//! from this goblin of this daemon, whatever daemon Emacs is configured for.
//! The sandbox's request stays open until the editor is done; closing it
//! kills the client.
use super::{Connection, Controller, Fault, capacity, params};
use goblins_jsonrpc as rpc;
use goblins_protocol::editor;
use serde_json::{Value, json};
use std::{
    io::Read,
    os::fd::AsRawFd,
    process::{Child, Command, Stdio},
};

/// Pending edits per sandbox.
const EDITS: usize = 8;

pub(super) struct Edit {
    session: String,
    connection: u64,
    rpc: Value,
    child: Child,
    diagnostic: Vec<u8>,
    command: String,
    /// The editor arguments to run once the pin succeeds.
    open: Option<Vec<String>>,
}

/// An Emacs Lisp string literal.
fn elisp_string(text: &str) -> String {
    format!("\"{}\"", text.replace('\\', "\\\\").replace('"', "\\\""))
}

/// emacsclient arguments that pin /goblins:HOST: to one session of one daemon
/// in Emacs, failing when Emacs has it pinned to another goblin.
pub(super) fn pin_arguments(
    host: &str,
    directory: &str,
    instance: &str,
    session: &str,
) -> Vec<String> {
    vec![
        "-a".into(),
        "false".into(),
        "--eval".into(),
        format!(
            "(goblins-pin-host {} {} {} {})",
            elisp_string(host),
            elisp_string(directory),
            elisp_string(instance),
            elisp_string(session)
        ),
    ]
}

fn spawn(command: &str, args: &[String]) -> Result<Child, Fault> {
    let child = Command::new(command)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| (-32009, format!("cannot run the editor {command}: {e}")))?;
    if let Some(stderr) = &child.stderr {
        let _ = crate::unix::nonblocking(stderr.as_raw_fd());
    }
    Ok(child)
}

/// emacsclient arguments for `open` in the goblin with TRAMP host `host`.
pub(super) fn arguments(host: &str, open: &editor::Open) -> Vec<String> {
    let mut args = Vec::new();
    if !open.wait {
        args.push("-n".to_string());
    }
    // Fail fast instead of starting another editor when Emacs is not running.
    args.extend(["-a".into(), "false".into()]);
    for file in &open.files {
        match (file.line, file.column) {
            (Some(line), Some(column)) => args.push(format!("+{line}:{column}")),
            (Some(line), None) => args.push(format!("+{line}")),
            _ => (),
        }
        // Never a leading '-': every name starts with the method.
        args.push(format!("/goblins:{host}:{}", file.path));
    }
    args
}

impl Controller {
    pub(super) fn editor_open(
        &mut self,
        c: &mut Connection,
        session: &str,
        rpc_id: Value,
        value: Value,
    ) -> Result<(), Fault> {
        let open: editor::Open = params(value)?;
        open.validate().map_err(|e| (-32602, e))?;
        let a = &self.sessions[session];
        let command = a
            .policy
            .as_ref()
            .and_then(|p| p.editor.as_ref())
            .ok_or((
                -32009,
                "no editor is configured; enable editor.emacs in mkGoblins".to_string(),
            ))?
            .command
            .clone();
        if self.edits.iter().filter(|e| e.session == session).count() >= EDITS {
            return Err(capacity());
        }
        // The TRAMP host is the goblin's agent name, unique among live goblins.
        let host = a.record.agent_name.clone();
        let directory = self.state.to_str().ok_or((
            -32009,
            "the daemon's state directory is not UTF-8".to_string(),
        ))?;
        let child = spawn(
            &command,
            &pin_arguments(&host, directory, &self.instance, session),
        )?;
        self.edits.push(Edit {
            session: session.to_string(),
            connection: c.id,
            rpc: rpc_id.clone(),
            child,
            diagnostic: Vec::new(),
            command,
            open: Some(arguments(&host, &open)),
        });
        c.waiting = Some(("editor".into(), rpc_id));
        Ok(())
    }
    /// Reply to edits whose editor client exited.
    pub(super) fn poll_edits(&mut self) {
        let mut index = 0;
        while index < self.edits.len() {
            let edit = &mut self.edits[index];
            if let Some(stderr) = &mut edit.child.stderr {
                let mut bytes = [0; 1024];
                while edit.diagnostic.len() < 4096
                    && let Ok(n @ 1..) = stderr.read(&mut bytes)
                {
                    edit.diagnostic.extend_from_slice(&bytes[..n]);
                }
            }
            match edit.child.try_wait() {
                Ok(None) => index += 1,
                status => {
                    let code = status.ok().flatten().and_then(|s| s.code()).unwrap_or(1);
                    // Pinned: now open the files.
                    if code == 0
                        && let Some(open) = edit.open.take()
                    {
                        match spawn(&edit.command, &open) {
                            Ok(child) => {
                                edit.child = child;
                                edit.diagnostic.clear();
                                index += 1;
                            }
                            Err((_, message)) => {
                                let edit = self.edits.remove(index);
                                self.reply_edit(edit, Err(message));
                            }
                        }
                        continue;
                    }
                    let edit = self.edits.remove(index);
                    let mut result = json!({"code":code});
                    if code != 0 {
                        // emacsclient explains a missing server or an abort.
                        let text = String::from_utf8_lossy(&edit.diagnostic);
                        let message = text
                            .lines()
                            .filter(|l| !l.trim().is_empty() && !l.starts_with("Waiting for Emacs"))
                            .map(|l| l.trim_start_matches("*ERROR*: "))
                            .collect::<Vec<_>>()
                            .join("; ");
                        result["message"] = json!(super::bounded(&message));
                    }
                    self.reply_edit(edit, Ok(result));
                }
            }
        }
    }
    fn reply_edit(&mut self, mut edit: Edit, result: Result<Value, String>) {
        let _ = edit.child.kill();
        let _ = edit.child.wait();
        if let Some(c) = self
            .connections
            .iter_mut()
            .find(|c| c.id == edit.connection)
        {
            c.waiting = None;
            c.close = true;
            c.queue(match result {
                Ok(value) => rpc::result(edit.rpc, value),
                Err(message) => rpc::error(edit.rpc, -32009, &message),
            });
        }
    }
    /// The requester disconnected: it no longer waits for the editor.
    pub(super) fn edit_withdraw(&mut self, connection: u64) {
        while let Some(index) = self.edits.iter().position(|e| e.connection == connection) {
            let mut edit = self.edits.remove(index);
            let _ = edit.child.kill();
            let _ = edit.child.wait();
        }
    }
    pub(super) fn end_edits(&mut self, session: &str) {
        while let Some(index) = self.edits.iter().position(|e| e.session == session) {
            let edit = self.edits.remove(index);
            self.reply_edit(edit, Err("the goblin has stopped".into()));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn pins_are_quoted_elisp_strings() {
        assert_eq!(
            pin_arguments("gribble.snaga", "/run/a \"b\\", "i", "i-s2"),
            [
                "-a",
                "false",
                "--eval",
                r#"(goblins-pin-host "gribble.snaga" "/run/a \"b\\" "i" "i-s2")"#
            ]
        );
    }
    #[test]
    fn editor_names_are_built_by_the_daemon() {
        let open = editor::Open {
            files: vec![
                editor::File {
                    path: "/tmp/a b".into(),
                    line: Some(3),
                    column: Some(4),
                },
                editor::File {
                    path: "/-x".into(),
                    line: Some(9),
                    column: None,
                },
                editor::File {
                    path: "/c".into(),
                    line: None,
                    column: None,
                },
            ],
            wait: false,
        };
        assert_eq!(
            arguments("gribble.snaga", &open),
            [
                "-n",
                "-a",
                "false",
                "+3:4",
                "/goblins:gribble.snaga:/tmp/a b",
                "+9",
                "/goblins:gribble.snaga:/-x",
                "/goblins:gribble.snaga:/c"
            ]
        );
    }
}
