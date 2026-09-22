//! One session worker: blocking host work is kept off the controller event pump.
use crate::{
    Result,
    config::Manifest,
    process::WORKER_TICK,
    session::{Cancel, Identity, Inheritance, Session},
    unix,
};
use goblins_protocol::{Reply, Request};
use std::{
    os::{
        fd::{AsRawFd, OwnedFd},
        unix::net::UnixListener,
    },
    path::PathBuf,
    sync::mpsc::{self, Receiver, SyncSender},
    thread::{self, JoinHandle},
};

pub(super) enum Work {
    Preview {
        approval: u64,
        package: String,
        cancel: Cancel,
    },
    Decide {
        request: Request,
        approved: bool,
        output: Option<PathBuf>,
    },
}
pub(super) enum Completed {
    Preview {
        approval: u64,
        result: std::result::Result<crate::catalog::Preview, String>,
    },
    Started {
        docker_enabled: bool,
        docker_scope: Option<String>,
        initial_packages: Vec<String>,
        master: OwnedFd,
        listener: UnixListener,
        identity: Identity,
        inheritance: Inheritance,
        exit_watch: OwnedFd,
    },
    Progress {
        request: String,
        state: &'static str,
    },
    Granted {
        reply: Reply,
        detail: Option<String>,
        output: Option<PathBuf>,
        docker_scope: Option<String>,
    },
    Failed(String),
    Stopped {
        exit_code: Option<i32>,
    },
}
pub(super) struct Worker {
    pub cancel: Cancel,
    pub commands: SyncSender<Work>,
    results: Receiver<Completed>,
    thread: Option<JoinHandle<()>>,
}
pub(super) enum Source {
    Host {
        configuration: PathBuf,
        name: String,
        workspace: Option<PathBuf>,
        cwd: Option<PathBuf>,
        state: PathBuf,
    },
    Child(Inheritance),
}
impl Worker {
    pub fn start(source: Source, directory: PathBuf, dimensions: (u16, u16)) -> Self {
        let cancel = Cancel::default();
        let token = cancel.clone();
        let (tx, commands) = mpsc::sync_channel(4);
        let (results, rx) = mpsc::sync_channel(16);
        let thread = thread::spawn(move || {
            let run = || -> Result<Option<i32>> {
                // The private host attachment chooses this launch's manifest.
                // Disk reads and launch adaptation stay off the event pump.
                token.check()?;
                let (master, slave) = unix::pty(dimensions.0, dimensions.1)?;
                let mut session = match source {
                    Source::Child(inherited) => {
                        Session::child(inherited, token.clone(), directory)?
                    }
                    Source::Host {
                        configuration,
                        name,
                        workspace,
                        cwd,
                        state,
                    } => {
                        let config = Manifest::read(&configuration)?.select(&name)?;
                        token.check()?;
                        let mut launch = config.launch()?;
                        launch.cwd = cwd;
                        if launch.initial_packages.len() > 128 {
                            return Err("initial package limit is 128".into());
                        }
                        launch.protected_paths.push(std::fs::canonicalize(state)?);
                        Session::new_in(launch, workspace.as_deref(), token.clone(), directory)?
                    }
                };
                if let Err(error) = session.start(Some(slave)) {
                    // Bubblewrap writes setup errors to the payload terminal,
                    // which is not handed to the event pump until Started.
                    // Preserve that diagnostic instead of losing it on failure.
                    use std::io::Read;
                    unix::nonblocking(master.as_raw_fd())?;
                    let mut output = Vec::new();
                    let _ = std::fs::File::from(master)
                        .take(4096)
                        .read_to_end(&mut output);
                    return Err(format!("{error}: {}", String::from_utf8_lossy(&output)).into());
                }
                results.try_send(Completed::Started {
                    docker_enabled: session.docker_enabled(),
                    docker_scope: session.docker_scope(),
                    initial_packages: session
                        .launch
                        .initial_packages
                        .iter()
                        .filter_map(|p| p.file_name()?.to_str()?.get(33..).map(str::to_string))
                        .collect(),
                    master,
                    listener: session.listener.take().unwrap(),
                    identity: session.identity.clone().unwrap(),
                    inheritance: session.inheritance(),
                    exit_watch: session.exit_watch()?,
                })?;
                loop {
                    if token.check().is_err() || !session.alive() {
                        break;
                    }
                    match commands.recv_timeout(WORKER_TICK) {
                        Ok(Work::Preview {
                            approval,
                            package,
                            cancel,
                        }) => {
                            let result = crate::catalog::preview(
                                &session.launch.flake,
                                &package,
                                &session.directory,
                                &cancel,
                            )
                            .map_err(|e| e.to_string());
                            if results
                                .try_send(Completed::Preview { approval, result })
                                .is_err()
                            {
                                break;
                            }
                        }
                        Ok(Work::Decide {
                            request: req,
                            approved,
                            output,
                        }) => {
                            session.event(
                                "decision",
                                serde_json::json!({"request":req,"approved":approved}),
                            );
                            let result = if approved {
                                if req.kind == "docker" {
                                    session.enable_docker(req.scope.as_deref(), req.anonymous)
                                } else {
                                    session.grant_observed(
                                        &req.package,
                                        output.as_deref(),
                                        |_, _| Ok(()),
                                        |state| {
                                            results.try_send(Completed::Progress {
                                                request: req.id.clone(),
                                                state,
                                            })?;
                                            Ok(())
                                        },
                                    )
                                }
                            } else {
                                Ok(())
                            };
                            let output = if approved && result.is_ok() {
                                session.packages.get(&req.package).cloned()
                            } else {
                                None
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
                                    let category = if req.kind == "docker" {
                                        if detail.starts_with("Docker unavailable in this session")
                                        {
                                            "Docker unavailable; fix host prerequisites and start a new sandbox; see host diagnostics".into()
                                        } else {
                                            "Docker activation failed; see host diagnostics".into()
                                        }
                                    } else if detail.starts_with("cannot resolve package") {
                                        format!(
                                            "cannot resolve package '{}' from pinned nixpkgs; see tui terminal",
                                            req.package
                                        )
                                    } else if detail.starts_with("could not build package") {
                                        format!(
                                            "could not build package '{}'; see tui terminal",
                                            req.package
                                        )
                                    } else {
                                        "grant failed; see tui terminal".into()
                                    };
                                    (
                                        Reply::new(Some(req.id), "error", Some(category)),
                                        Some(detail),
                                    )
                                }
                            };
                            if results
                                .try_send(Completed::Granted {
                                    docker_scope: session.docker_scope(),
                                    reply,
                                    detail,
                                    output,
                                })
                                .is_err()
                            {
                                break;
                            }
                        }
                        Err(mpsc::RecvTimeoutError::Timeout) => (),
                        Err(mpsc::RecvTimeoutError::Disconnected) => break,
                    }
                }
                // Cancellation may have come from the controller observing a
                // normal payload exit while this worker was resolving a package.
                session.alive();
                Ok(session.exit_code)
            };
            match run() {
                Ok(exit_code) => {
                    let _ = results.try_send(Completed::Stopped { exit_code });
                }
                Err(e) => {
                    if token.check().is_ok() {
                        let _ = results.try_send(Completed::Failed(e.to_string()));
                    }
                    let _ = results.try_send(Completed::Stopped { exit_code: None });
                }
            }
        });
        Self {
            cancel,
            commands: tx,
            results: rx,
            thread: Some(thread),
        }
    }
}
impl Worker {
    /// Completion is usable only after observing the channel disconnected and
    /// empty. Checking thread completion after an empty read could otherwise
    /// discard a final result sent between those two observations.
    pub fn poll(&self) -> (Vec<Completed>, bool) {
        let mut results = Vec::new();
        for _ in 0..16 {
            match self.results.try_recv() {
                Ok(result) => results.push(result),
                Err(mpsc::TryRecvError::Empty) => return (results, false),
                Err(mpsc::TryRecvError::Disconnected) => {
                    return (results, self.finished());
                }
            }
        }
        (results, false)
    }
    #[cfg(test)]
    pub fn completed(results: Vec<Completed>) -> Self {
        let (commands, _) = mpsc::sync_channel(4);
        let (sender, receiver) = mpsc::sync_channel(16);
        for result in results {
            sender
                .try_send(result)
                .unwrap_or_else(|_| panic!("test result queue full"));
        }
        drop(sender);
        Self {
            cancel: Cancel::default(),
            commands,
            results: receiver,
            thread: None,
        }
    }
    fn finished(&self) -> bool {
        self.thread.as_ref().is_none_or(|t| t.is_finished())
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn final_result_after_empty_poll_is_not_discarded() {
        let (commands, _) = mpsc::sync_channel(4);
        let (sender, results) = mpsc::sync_channel(16);
        let worker = Worker {
            cancel: Cancel::default(),
            commands,
            results,
            thread: None,
        };
        // Even a finished producer observation cannot make an empty (still
        // connected) channel final. A last message may arrive after this poll.
        let (results, finished) = worker.poll();
        assert!(results.is_empty());
        assert!(!finished);
        sender
            .try_send(Completed::Stopped { exit_code: Some(7) })
            .unwrap_or_else(|_| panic!("send"));
        drop(sender);
        let (results, finished) = worker.poll();
        assert!(finished);
        assert!(matches!(
            results.as_slice(),
            [Completed::Stopped { exit_code: Some(7) }]
        ));
    }
}
