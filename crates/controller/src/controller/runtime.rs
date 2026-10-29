//! The daemon's handle to one goblin's runtime: commands go in, events come
//! out, and neither side waits for the other.
use crate::{
    session::{Cancel, Identity, Inheritance},
    worker::{Auto, Recreated, Source, Worker},
};
use goblins_protocol::{Reply, Request};
use std::{
    collections::BTreeMap,
    os::{fd::OwnedFd, unix::net::UnixListener},
    path::PathBuf,
    sync::mpsc,
};

/// The daemon's view of one goblin's runtime. Neither method blocks.
pub(crate) trait Runtime: Send {
    /// Err gives the command back when the runtime's queue is full.
    /// Cancellations and `Stop` are never refused.
    fn send(&mut self, command: Command) -> Result<(), Command>;
    fn poll(&mut self) -> Option<Event>;
    /// True once every event has been polled and the runtime is gone.
    fn ended(&self) -> bool;
}

/// What the daemon asks of a runtime.
#[derive(Debug)]
pub(crate) enum Command {
    /// Resolve a package for its approval preview; cancelled by `approval`.
    Preview {
        approval: u64,
        package: String,
    },
    Decide {
        request: Request,
        approved: bool,
        output: Option<PathBuf>,
    },
    /// Evaluate a dev shell refresh candidate for approval; cancelled by
    /// `approval`.
    Refresh {
        approval: u64,
        changes: crate::devshell::Changes,
    },
    /// Build and mount a flake app from the trusted generation; cancelled by
    /// `serial`.
    Run {
        serial: u64,
        app: String,
    },
    /// Evaluate a refresh candidate for its preview only; nothing is stored.
    /// Cancelled by `serial`.
    Diff {
        serial: u64,
        changes: crate::devshell::Changes,
    },
    /// Cancel the work with this approval or serial, queued or running.
    Cancel(u64),
    Stop,
}

impl Command {
    /// The id a `Cancel` names this command by, when it can be cancelled.
    fn cancel_id(&self) -> Option<u64> {
        match self {
            Command::Preview { approval, .. } | Command::Refresh { approval, .. } => {
                Some(*approval)
            }
            Command::Run { serial, .. } | Command::Diff { serial, .. } => Some(*serial),
            _ => None,
        }
    }
}

/// What a runtime reports.
pub(crate) enum Event {
    Preview {
        approval: u64,
        result: std::result::Result<crate::catalog::Preview, String>,
    },
    Started {
        docker_enabled: bool,
        scope: Option<String>,
        dev_shell: Option<String>,
        dev_identity: Option<String>,
        initial_packages: Vec<String>,
        /// The agent's PTY, or with attach handlers its output pipe.
        master: OwnedFd,
        handlers: Option<Box<crate::config::Handlers>>,
        /// The private channel to the sandbox's goblins-init.
        control: OwnedFd,
        listener: UnixListener,
        identity: Identity,
        inheritance: Inheritance,
        exit_watch: OwnedFd,
        recreated: Option<Box<Recreated>>,
        auto: Box<Auto>,
    },
    Progress {
        request: String,
        state: &'static str,
    },
    /// Whether the agent will run without a terminal, known before the
    /// launch's slow work.
    Mode {
        server: bool,
    },
    /// Structured approval preview and candidate identity, or why it cannot
    /// refresh.
    Refresh {
        approval: u64,
        result: std::result::Result<(serde_json::Value, String), String>,
    },
    /// The reply to a sandbox call waiting on the runtime (`Command::Run`,
    /// `Command::Diff`), or why it failed.
    Call {
        serial: u64,
        result: std::result::Result<serde_json::Value, String>,
    },
    /// A refresh became the current generation; members it runs later start in it.
    DevShell {
        identity: String,
        inheritance: Inheritance,
    },
    Granted {
        reply: Reply,
        detail: Option<String>,
        output: Option<PathBuf>,
    },
    /// A host launch's manifest validated; `sequence` orders acceptances.
    Manifest {
        sequence: u64,
        lease: std::sync::Arc<crate::config::ManifestLease>,
    },
    Failed(String),
    Stopped {
        exit_code: Option<i32>,
    },
}

/// The runtime inside the daemon: a worker thread for Nix-bound work, with
/// `send` as the dispatcher that never waits for it.
pub(crate) struct Local {
    worker: Worker,
    /// Tokens of cancellable work sent to the worker, by cancel id.
    cancels: BTreeMap<u64, Cancel>,
    ended: bool,
}

impl Local {
    pub fn start(source: Source, directory: PathBuf, dimensions: (u16, u16)) -> Self {
        Self {
            worker: Worker::start(source, directory, dimensions),
            cancels: BTreeMap::new(),
            ended: false,
        }
    }
}

impl Runtime for Local {
    fn send(&mut self, command: Command) -> Result<(), Command> {
        match command {
            Command::Cancel(id) => {
                if let Some(cancel) = self.cancels.remove(&id) {
                    cancel.cancel();
                }
                Ok(())
            }
            Command::Stop => {
                // Children of the worker's token see it cancelled too.
                self.worker.cancel.cancel();
                self.cancels.clear();
                Ok(())
            }
            command => {
                let id = command.cancel_id();
                let cancel = self.worker.cancel.child();
                match self.worker.commands.try_send((command, cancel.clone())) {
                    Ok(()) => {
                        if let Some(id) = id {
                            self.cancels.insert(id, cancel);
                        }
                        Ok(())
                    }
                    Err(error) => Err(super::unsent(error).0),
                }
            }
        }
    }
    /// Completion is usable only after observing the channel disconnected and
    /// empty. Checking thread completion after an empty read could otherwise
    /// discard a final result sent between those two observations.
    fn poll(&mut self) -> Option<Event> {
        match self.worker.results.try_recv() {
            Ok(event) => {
                // Finished work no longer needs its token.
                match &event {
                    Event::Preview { approval: id, .. }
                    | Event::Refresh { approval: id, .. }
                    | Event::Call { serial: id, .. } => {
                        self.cancels.remove(id);
                    }
                    _ => (),
                }
                Some(event)
            }
            Err(mpsc::TryRecvError::Empty) => None,
            Err(mpsc::TryRecvError::Disconnected) => {
                self.ended = self.worker.finished();
                None
            }
        }
    }
    fn ended(&self) -> bool {
        self.ended
    }
}

/// Commands a `Fake` runtime received, shared with the test.
#[cfg(test)]
pub(crate) type Received = std::sync::Arc<std::sync::Mutex<std::collections::VecDeque<Command>>>;

/// A runtime for tests: it records the commands it is sent and replays the
/// events the test supplies.
#[cfg(test)]
pub(crate) struct Fake {
    received: Received,
    events: mpsc::Receiver<Event>,
    ended: bool,
}

#[cfg(test)]
impl Fake {
    /// A runtime that reports `events` and then ends.
    pub fn ended(events: Vec<Event>) -> Self {
        let (fake, sender, _) = Self::new();
        for event in events {
            sender
                .try_send(event)
                .unwrap_or_else(|_| panic!("test event queue full"));
        }
        fake
    }
    /// A live runtime whose events the test supplies through the sender.
    pub fn new() -> (Self, mpsc::SyncSender<Event>, Received) {
        let (sender, events) = mpsc::sync_channel(16);
        let received = Received::default();
        let fake = Self {
            received: received.clone(),
            events,
            ended: false,
        };
        (fake, sender, received)
    }
}

#[cfg(test)]
impl Runtime for Fake {
    fn send(&mut self, command: Command) -> Result<(), Command> {
        let mut received = self.received.lock().unwrap();
        // Requests are bounded like a worker's queue; nothing else is.
        let requests = |c: &Command| !matches!(c, Command::Cancel(_) | Command::Stop);
        if requests(&command) && received.iter().filter(|c| requests(c)).count() >= 4 {
            return Err(command);
        }
        received.push_back(command);
        Ok(())
    }
    fn poll(&mut self) -> Option<Event> {
        match self.events.try_recv() {
            Ok(event) => Some(event),
            Err(mpsc::TryRecvError::Empty) => None,
            Err(mpsc::TryRecvError::Disconnected) => {
                self.ended = true;
                None
            }
        }
    }
    fn ended(&self) -> bool {
        self.ended
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn final_result_after_empty_poll_is_not_discarded() {
        let (commands, _) = mpsc::sync_channel(4);
        let (sender, results) = mpsc::sync_channel(16);
        let mut local = Local {
            worker: Worker {
                cancel: Cancel::default(),
                commands,
                results,
                thread: None,
            },
            cancels: BTreeMap::new(),
            ended: false,
        };
        // Even a finished producer observation cannot make an empty (still
        // connected) channel final. A last message may arrive after this poll.
        assert!(local.poll().is_none());
        assert!(!local.ended());
        sender
            .try_send(Event::Stopped { exit_code: Some(7) })
            .unwrap_or_else(|_| panic!("send"));
        drop(sender);
        assert!(matches!(
            local.poll(),
            Some(Event::Stopped { exit_code: Some(7) })
        ));
        assert!(!local.ended());
        assert!(local.poll().is_none());
        assert!(local.ended());
    }

    #[test]
    fn cancellation_reaches_queued_and_running_work_even_when_the_queue_is_full() {
        let (commands, received) = mpsc::sync_channel(4);
        let (_sender, results) = mpsc::sync_channel(16);
        let mut local = Local {
            worker: Worker {
                cancel: Cancel::default(),
                commands,
                results,
                thread: None,
            },
            cancels: BTreeMap::new(),
            ended: false,
        };
        let preview = |approval| Command::Preview {
            approval,
            package: "hello".into(),
        };
        let drain = || -> Vec<(u64, Cancel)> {
            received
                .try_iter()
                .map(|(command, cancel)| (command.cancel_id().unwrap(), cancel))
                .collect()
        };
        // A cancel reaches work still in the queue, and only that work.
        local.send(preview(1)).unwrap();
        local.send(preview(2)).unwrap();
        let (_, running) = received.try_recv().unwrap();
        local.send(Command::Cancel(2)).unwrap();
        let queued = drain();
        assert_eq!(queued.len(), 1);
        assert_eq!(queued[0].0, 2);
        assert!(queued[0].1.check().is_err());
        assert!(running.check().is_ok());
        // And work the worker is running.
        local.send(Command::Cancel(1)).unwrap();
        assert!(running.check().is_err());
        // Unknown and repeated cancellations are harmless.
        local.send(Command::Cancel(1)).unwrap();
        local.send(Command::Cancel(9)).unwrap();
        // A full queue gives a request back, keeping no token for it, but
        // never refuses a cancellation.
        for approval in 3..7 {
            local.send(preview(approval)).unwrap();
        }
        assert!(matches!(
            local.send(preview(7)),
            Err(Command::Preview { approval: 7, .. })
        ));
        assert!(!local.cancels.contains_key(&7));
        local.send(Command::Cancel(4)).unwrap();
        let cancelled: Vec<_> = drain()
            .into_iter()
            .map(|(id, cancel)| (id, cancel.check().is_err()))
            .collect();
        assert_eq!(cancelled, [(3, false), (4, true), (5, false), (6, false)]);
        // Nor Stop, which reaches every queued request and the worker.
        for approval in 8..12 {
            local.send(preview(approval)).unwrap();
        }
        assert!(local.send(preview(12)).is_err());
        local.send(Command::Stop).unwrap();
        assert!(drain().iter().all(|(_, cancel)| cancel.check().is_err()));
        assert!(local.worker.cancel.check().is_err());
    }
}
