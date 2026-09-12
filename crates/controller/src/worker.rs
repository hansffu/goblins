//! One session worker: blocking host work is kept off the controller event pump.
use crate::{
    Result,
    config::Configuration,
    process::WORKER_TICK,
    session::{Cancel, Identity, Session},
    unix,
};
use goblins_protocol::{Reply, Request};
use std::{
    os::{fd::OwnedFd, unix::net::UnixListener},
    path::PathBuf,
    sync::mpsc::{self, Receiver, Sender},
    thread::{self, JoinHandle},
};

pub(super) enum Work {
    Decide { request: Request, approved: bool },
}
pub(super) enum Completed {
    Started {
        master: OwnedFd,
        listener: UnixListener,
        identity: Identity,
    },
    Granted {
        reply: Reply,
        detail: Option<String>,
    },
    Failed(String),
    Stopped,
}
pub(super) struct Worker {
    pub cancel: Cancel,
    pub commands: Sender<Work>,
    pub results: Receiver<Completed>,
    thread: Option<JoinHandle<()>>,
}
impl Worker {
    pub fn start(
        config: Configuration,
        workspace: Option<PathBuf>,
        state: PathBuf,
        rows: u16,
        cols: u16,
    ) -> Self {
        let cancel = Cancel::default();
        let token = cancel.clone();
        let (tx, commands) = mpsc::channel();
        let (results, rx) = mpsc::channel();
        let thread = thread::spawn(move || {
            let run = || -> Result<()> {
                let (master, slave) = unix::pty(rows, cols)?;
                let mut launch = config.launch()?;
                launch.protected_paths.push(std::fs::canonicalize(state)?);
                let mut session = Session::new(launch, workspace.as_deref(), token.clone())?;
                session.start(Some(slave))?;
                results.send(Completed::Started {
                    master,
                    listener: session.listener.take().unwrap(),
                    identity: session.identity.clone().unwrap(),
                })?;
                loop {
                    if token.check().is_err() || !session.alive() {
                        break;
                    }
                    match commands.recv_timeout(WORKER_TICK) {
                        Ok(Work::Decide {
                            request: req,
                            approved,
                        }) => {
                            session.event(
                                "decision",
                                serde_json::json!({"request":req,"approved":approved}),
                            );
                            let result = if approved {
                                session.grant(&req.package, None)
                            } else {
                                Ok(())
                            };
                            let (reply, detail) = match result {
                                Ok(()) => (
                                    Reply::new(
                                        Some(req.id),
                                        if approved { "ready" } else { "denied" },
                                        None,
                                    ),
                                    None,
                                ),
                                Err(error) => {
                                    let detail = error.to_string();
                                    session.event("error", serde_json::json!({"detail":detail}));
                                    let category = if detail.starts_with("cannot resolve package") {
                                        format!(
                                            "cannot resolve package '{}' from pinned nixpkgs; see serve terminal",
                                            req.package
                                        )
                                    } else if detail.starts_with("could not build package") {
                                        format!(
                                            "could not build package '{}'; see serve terminal",
                                            req.package
                                        )
                                    } else {
                                        "grant failed; see serve terminal".into()
                                    };
                                    (
                                        Reply::new(Some(req.id), "error", Some(category)),
                                        Some(detail),
                                    )
                                }
                            };
                            if results.send(Completed::Granted { reply, detail }).is_err() {
                                break;
                            }
                        }
                        Err(mpsc::RecvTimeoutError::Timeout) => (),
                        Err(mpsc::RecvTimeoutError::Disconnected) => break,
                    }
                }
                Ok(())
            };
            if let Err(e) = run() {
                let _ = results.send(Completed::Failed(e.to_string()));
            }
            let _ = results.send(Completed::Stopped);
        });
        Self {
            cancel,
            commands: tx,
            results: rx,
            thread: Some(thread),
        }
    }
}
impl Drop for Worker {
    fn drop(&mut self) {
        self.cancel.cancel();
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}
