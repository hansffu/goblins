//! The daemon's handle to one goblin's runtime: commands go in, events come
//! out, and neither side waits for the other.
use crate::session::Inheritance;
use crate::worker::Endpoints;
use crate::worker::Worker;
use goblins_init as init;
use goblins_runtime::Source;
use goblins_runtime::policy::Capabilities;
use goblins_unix as unix;
use std::{
    collections::{BTreeMap, VecDeque},
    fs::{self, File},
    io,
    os::{
        fd::{AsRawFd, OwnedFd, RawFd},
        unix::process::CommandExt,
    },
    path::{Path, PathBuf},
    process::{Child, Stdio as ChildStdio},
    sync::mpsc,
};

/// Output kept per host job; the rest is discarded with a note.
const LOG_LIMIT: u64 = 16 * 1024 * 1024;
/// Finished host jobs whose output stays readable per sandbox.
const KEPT: usize = 8;

use goblins_runtime::{Command, Event, Fd, Runtime, Stdio};

/// The goblin's current capability, replaced under the lock whenever a dev
/// shell refresh publishes, so an export never waits for Nix work.
pub(crate) type Published = std::sync::Arc<std::sync::Mutex<Option<Inheritance>>>;

/// A running host job.
pub(crate) struct HostJob {
    request: String,
    child: Child,
    /// Dropping it stops the job.
    lifetime: Option<OwnedFd>,
    directory: PathBuf,
}

use crate::Local;

impl Local {
    pub fn start(source: Source, directory: PathBuf, dimensions: (u16, u16)) -> Self {
        let host_jobs = directory.join("host-jobs");
        let published = Published::default();
        // A member's worker waits for the handoff before it builds anything.
        let (start, handoff) = match &source {
            Source::Member { .. } => {
                let (start, handoff) = mpsc::sync_channel(1);
                (Some(start), Some(handoff))
            }
            Source::Host { .. } => (None, None),
        };
        let worker = Worker::start(source, directory, dimensions, published.clone(), handoff);
        let mut local = Self::new(worker, host_jobs);
        local.published = published;
        local.start = start;
        local
    }
    fn new(worker: Worker, host_jobs: PathBuf) -> Self {
        Self {
            worker,
            published: Published::default(),
            start: None,
            capabilities: None,
            host_jobs,
            prepared: BTreeMap::new(),
            jobs: vec![],
            cancels: BTreeMap::new(),
            init: None,
            listener: None,
            relay: Default::default(),
            exit_watch: None,
            masters: BTreeMap::new(),
            buffer: vec![],
            stopping: false,
            events: VecDeque::new(),
            polling: false,
            ended: false,
        }
    }
    /// The goblin's current capability, once it has started.
    fn capability(&self) -> Option<Inheritance> {
        self.published.lock().ok()?.clone()
    }
    /// Report an event in the current round, or the next.
    fn report(&mut self, event: Event) {
        self.events.push_back(event);
    }
    /// Collect a round: worker results, init's events, new request
    /// connections and the payload's exit.
    fn collect(&mut self) {
        for _ in 0..16 {
            match self.worker.results.try_recv() {
                Ok(event) => {
                    match &event {
                        // Finished work no longer needs its token.
                        Event::Preview { approval: id, .. }
                        | Event::Refresh { approval: id, .. }
                        | Event::Call { serial: id, .. } => {
                            self.cancels.remove(id);
                        }
                        // The worker hands the endpoints over just before.
                        Event::Started {
                            policy, support, ..
                        } => {
                            self.capabilities = Some(((**policy).clone(), support.clone()));
                            if let Ok(endpoints) = self.worker.endpoints.try_recv() {
                                let Endpoints {
                                    control,
                                    listener,
                                    exit_watch,
                                } = *endpoints;
                                self.init = Some(control);
                                self.exit_watch = Some(exit_watch);
                                self.listener = Some(listener);
                            }
                        }
                        Event::Stopped { .. } => {
                            self.listener = None;
                            self.relay.end(&mut self.events);
                        }
                        _ => (),
                    }
                    self.report(event);
                }
                Err(mpsc::TryRecvError::Empty) => break,
                // Completion is usable only after observing the channel
                // disconnected and empty. Checking thread completion after
                // an empty read could otherwise discard a final result sent
                // between those two observations.
                Err(mpsc::TryRecvError::Disconnected) => {
                    self.ended = self.worker.finished();
                    if self.ended {
                        self.listener = None;
                        self.relay.end(&mut self.events);
                    }
                    break;
                }
            }
        }
        // Read after results too: Stopped must not hide a failure that the
        // worker published immediately before it, even with a full result queue.
        if let Ok(name) = self.worker.scope_failures.try_recv() {
            self.report(Event::ScopeFailed { name });
        }
        self.receive_init();
        for _ in 0..8 {
            let Some(listener) = &self.listener else {
                break;
            };
            match listener.accept() {
                Ok((stream, _)) => {
                    self.relay.open(stream, &mut self.events);
                }
                Err(e) if e.kind() == io::ErrorKind::Interrupted => (),
                Err(_) => break,
            }
        }
        if let Some((policy, support)) = &self.capabilities {
            self.relay.tick(
                &Capabilities { policy, support },
                self.stopping,
                &mut self.events,
            );
        }
        self.finish_host_jobs();
        // Observe process exit independently of a worker blocked on Nix.
        // A pidfd pins the process identity, avoiding PID-reuse races.
        if self
            .exit_watch
            .as_ref()
            .is_some_and(|fd| unix::readable(fd.as_raw_fd(), 0).unwrap_or(true))
        {
            self.exit_watch = None;
            self.report(Event::PayloadExited);
        }
    }
    /// Read init's events without blocking.
    fn receive_init(&mut self) {
        if self.init.is_some() && self.buffer.is_empty() {
            self.buffer = vec![0; init::MAX_MESSAGE];
        }
        for _ in 0..256 {
            let Some(socket) = &self.init else {
                return;
            };
            match init::receive(socket.as_raw_fd(), &mut self.buffer, libc::MSG_DONTWAIT) {
                // Init never sends descriptors; any that arrive are closed.
                Ok((n, _)) if n > 0 => {
                    let Ok(event) = serde_json::from_slice::<init::Event>(&self.buffer[..n]) else {
                        continue;
                    };
                    let event = match event {
                        init::Event::Spawned { id, pid } => Event::Spawned {
                            id,
                            pid,
                            master: self.masters.remove(&id).map(Fd::from),
                        },
                        init::Event::Failed { id, code, message } => {
                            self.masters.remove(&id);
                            Event::SpawnFailed {
                                id,
                                code: Some(code),
                                message,
                            }
                        }
                        init::Event::Exited { id, code, signal } => {
                            Event::Exited { id, code, signal }
                        }
                    };
                    self.report(event);
                }
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => return,
                Ok(_) | Err(_) => {
                    self.init = None;
                    self.masters.clear();
                    self.report(Event::InitLost);
                    return;
                }
            }
        }
    }
    /// Send `command` to init with `fds` attached.
    fn to_init(&self, command: &init::Command, fds: &[RawFd]) -> Result<(), String> {
        let socket = self.init.as_ref().ok_or("the goblin has no init channel")?;
        let bytes = serde_json::to_vec(command).map_err(|e| e.to_string())?;
        init::send(socket.as_raw_fd(), &bytes, fds)
            .map_err(|e| format!("cannot reach the sandbox's init: {e}"))
    }
    fn spawn(
        &mut self,
        id: u64,
        argv: Option<Vec<String>>,
        cwd: Option<String>,
        env: Vec<(String, String)>,
        stdio: Stdio,
    ) {
        let (tty, fds, master): (_, Vec<OwnedFd>, _) = match stdio {
            Stdio::Fds(fds) => (false, fds.into_iter().map(OwnedFd::from).collect(), None),
            Stdio::Tty(slave) => (true, vec![slave.into()], None),
            Stdio::NewPty { rows, cols } => match unix::pty(rows, cols) {
                Ok((master, slave)) => (true, vec![slave], Some(master)),
                Err(error) => {
                    return self.report(Event::SpawnFailed {
                        id,
                        code: None,
                        message: error.to_string(),
                    });
                }
            },
        };
        let command = init::Command::Spawn {
            id,
            argv,
            cwd,
            env,
            tty,
        };
        let raw: Vec<_> = fds.iter().map(AsRawFd::as_raw_fd).collect();
        match self.to_init(&command, &raw) {
            Ok(()) => {
                if let Some(master) = master {
                    self.masters.insert(id, master);
                }
            }
            Err(message) => self.report(Event::SpawnFailed {
                id,
                code: None,
                message,
            }),
        }
    }
}

impl Local {
    /// Start an approved host job under the helper's `--host-job`
    /// supervisor, in the directory its request pinned.
    fn run_host_job(
        &mut self,
        request: &str,
        argv: &[String],
        env: &[(String, String)],
        timeout: Option<u64>,
    ) -> crate::Result<()> {
        let cwd = self
            .prepared
            .remove(request)
            .ok_or("the host command's directory was not prepared")?;
        let helper = self
            .capability()
            .ok_or("the goblin has stopped")?
            .helper()
            .to_path_buf();
        prune(&self.host_jobs);
        let directory = self.host_jobs.join(request);
        fs::create_dir(&directory)?;
        let (lifetime_read, lifetime_write) = unix::pipe()?;
        let mut command = std::process::Command::new(helper);
        command
            .arg("--host-job")
            .arg(directory.join("output.log"))
            .arg(directory.join("status.json"))
            .arg(LOG_LIMIT.to_string())
            .arg(timeout.unwrap_or(0).to_string());
        // The request's variables reach the command only, not the supervisor.
        for (key, value) in env {
            command.args(["--env", key, value]);
        }
        command
            .arg("--")
            .args(argv)
            // Temporary files go straight to /tmp unless the request says
            // otherwise: nested daemons and nix develop put sockets beneath
            // TMPDIR, and Unix socket paths are limited to 107 bytes.
            .env_remove("TMPDIR")
            .env("GOBLINS_HOST_JOB", request)
            .stdin(ChildStdio::from(lifetime_read))
            .stdout(ChildStdio::null())
            .stderr(File::create(directory.join("supervisor.log"))?);
        let cwd = cwd.as_raw_fd();
        let parent = unsafe { libc::getpid() };
        unsafe {
            command.pre_exec(move || {
                // Lifetime: the pipe closes with the daemon, but also stop
                // promptly if the daemon dies before reaching it.
                unix::cvt(libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGTERM))?;
                if libc::getppid() != parent {
                    return Err(std::io::Error::from_raw_os_error(libc::ESRCH));
                }
                unix::cvt(libc::fchdir(cwd))?;
                // Only stdio crosses: no daemon socket or session descriptor.
                unix::cvt(libc::syscall(libc::SYS_close_range, 3u32, u32::MAX, 4u32) as i32)?;
                Ok(())
            });
        }
        let child = command.spawn();
        drop(command);
        self.jobs.push(HostJob {
            request: request.to_string(),
            child: child?,
            lifetime: Some(lifetime_write),
            directory,
        });
        Ok(())
    }
    /// Report jobs whose supervisor has exited.
    fn finish_host_jobs(&mut self) {
        let mut index = 0;
        while index < self.jobs.len() {
            if matches!(self.jobs[index].child.try_wait(), Ok(None)) {
                index += 1;
                continue;
            }
            let mut job = self.jobs.remove(index);
            drop(job.lifetime.take());
            let _ = job.child.wait();
            let outcome = fs::read(job.directory.join("status.json"))
                .ok()
                .and_then(|bytes| serde_json::from_slice(&bytes).ok())
                .ok_or_else(|| crate::process::diagnostic(&job.directory.join("supervisor.log")));
            self.report(Event::HostJobFinished {
                request: job.request,
                outcome,
            });
        }
    }
}

/// Keep the output of the most recent finished jobs only.
fn prune(jobs: &Path) {
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

impl Runtime for Local {
    fn send(&mut self, command: Command) -> Result<(), Command> {
        // The daemon checks too; a command the goblin may not be sent fails
        // the goblin closed instead of running, and so does one that needs a
        // capability before `Started` has reported them.
        if let Some(operation) = command.operation() {
            let refused = match &self.capabilities {
                Some((policy, support)) => (Capabilities { policy, support })
                    .check(&operation)
                    .err()
                    .map(refusal_message),
                None => Some("the goblin has not started".to_string()),
            };
            if let Some(message) = refused {
                self.report(Event::Failed(message));
                return self.send(Command::Stop);
            }
        }
        match command {
            Command::SandboxSend {
                connection,
                message,
            } => self.relay.send(connection, message),
            Command::SandboxClose { connection } => self.relay.close(connection),
            Command::ScopeFailed => {
                self.worker
                    .failed_scope
                    .store(true, std::sync::atomic::Ordering::SeqCst);
                return self.send(Command::Stop);
            }
            Command::ScopeMembership { result } => {
                if !self.stopping {
                    if let Err(
                        mpsc::TrySendError::Full(result) | mpsc::TrySendError::Disconnected(result),
                    ) = self.worker.memberships.try_send(result)
                    {
                        return Err(Command::ScopeMembership { result });
                    }
                }
            }
            Command::Cancel(id) => {
                if let Some(cancel) = self.cancels.remove(&id) {
                    cancel.cancel();
                }
            }
            Command::Stop => {
                // Children of the worker's token see it cancelled too.
                self.worker.cancel.cancel();
                self.cancels.clear();
                self.stopping = true;
            }
            Command::Spawn {
                id,
                argv,
                cwd,
                env,
                stdio,
                ..
            } => self.spawn(id, argv, cwd, env, stdio),
            // Best effort, like init's own delivery.
            Command::Signal { id, signal } => {
                let _ = self.to_init(&init::Command::Signal { id, signal }, &[]);
            }
            Command::CancelProcess { id, everything } => {
                let _ = self.to_init(&init::Command::Cancel { id, everything }, &[]);
            }
            Command::KillAll => {
                let _ = self.to_init(&init::Command::KillAll {}, &[]);
            }
            Command::Export { member } => {
                let result = match self.capability() {
                    Some(inheritance) => inheritance
                        .handoff()
                        .map(Box::new)
                        .map_err(|e| e.to_string()),
                    None => Err("the goblin has not started".into()),
                };
                self.report(Event::Exported { member, result });
            }
            Command::Start(handoff) => {
                // A one-slot channel to a worker that waits: it only refuses
                // when the worker is gone or was already given one, and then
                // the launch cannot start.
                let delivered = self
                    .start
                    .as_ref()
                    .is_some_and(|start| start.try_send(*handoff).is_ok());
                if !delivered {
                    self.report(Event::Failed(
                        "the launch could not take its owner's handoff".into(),
                    ));
                    return self.send(Command::Stop);
                }
            }
            Command::PrepareHostJob { request, cwd } => {
                let result = match &self.capability() {
                    Some(inheritance) => inheritance
                        .host_directory(&cwd)
                        .map(|(file, path)| {
                            self.prepared.insert(request.clone(), file);
                            path
                        })
                        .map_err(|e| e.to_string()),
                    None => Err("the goblin has stopped".into()),
                };
                self.report(Event::HostJobPrepared { request, result });
            }
            Command::RunHostJob {
                request,
                argv,
                env,
                timeout,
            } => {
                if let Err(error) = self.run_host_job(&request, &argv, &env, timeout) {
                    self.report(Event::HostJobFailed {
                        request,
                        error: error.to_string(),
                    });
                }
            }
            Command::CancelHostJob { request } => {
                self.prepared.remove(&request);
                if let Some(job) = self.jobs.iter_mut().find(|j| j.request == request) {
                    job.lifetime.take();
                }
            }
            command => {
                let id = command.cancel_id();
                let cancel = self.worker.cancel.child();
                match self.worker.commands.try_send((command, cancel.clone())) {
                    Ok(()) => {
                        if let Some(id) = id {
                            self.cancels.insert(id, cancel);
                        }
                    }
                    Err(error) => return Err(unsent(error).0),
                }
            }
        }
        Ok(())
    }
    fn poll(&mut self) -> Option<Event> {
        // Every round collects every source, behind what `send` reported.
        if !self.polling {
            self.collect();
            self.polling = true;
        }
        let event = self.events.pop_front();
        self.polling = event.is_some();
        event
    }
    fn ended(&self) -> bool {
        self.ended && self.events.is_empty() && self.jobs.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::process::Cancellation as Cancel;
    use goblins_runtime::Identity;
    use goblins_runtime::SpawnKind;
    use goblins_runtime::policy::{Policy, Support};
    use std::os::unix::net::UnixListener;

    /// A worker without a thread: the test receives its commands and sends
    /// its results.
    fn worker() -> (
        Worker,
        mpsc::Receiver<crate::worker::Work>,
        mpsc::SyncSender<Event>,
    ) {
        let (commands, received) = mpsc::sync_channel(4);
        let (sender, results) = mpsc::sync_channel(16);
        let (_, endpoints) = mpsc::sync_channel(1);
        let worker = Worker {
            cancel: Cancel::default(),
            failed_scope: Default::default(),
            scope_failures: mpsc::sync_channel(1).1,
            commands,
            memberships: mpsc::sync_channel(1).0,
            results,
            endpoints,
            thread: None,
        };
        (worker, received, sender)
    }

    #[test]
    fn a_scope_allocation_error_reaches_the_worker_without_stopping_a_running_goblin() {
        let (mut worker, _, _sender) = worker();
        let (memberships, received) = mpsc::sync_channel(1);
        worker.memberships = memberships;
        let mut local = ready(worker);
        local
            .send(Command::ScopeMembership {
                result: Err("scope membership queue is full".into()),
            })
            .unwrap();
        assert!(
            matches!(received.try_recv(), Ok(Err(message)) if message == "scope membership queue is full")
        );
        assert!(!local.stopping);
        assert!(local.poll().is_none());
    }

    #[test]
    fn scope_failure_is_not_lost_behind_a_full_or_ended_result_queue() {
        let (mut worker, _, sender) = worker();
        let (failure, received) = mpsc::sync_channel(1);
        worker.scope_failures = received;
        for _ in 0..16 {
            sender.try_send(Event::Mode { server: false }).unwrap();
        }
        failure.try_send("work".into()).unwrap();
        drop(sender);
        let mut local = ready(worker);
        let events: Vec<_> = std::iter::from_fn(|| local.poll()).collect();
        assert_eq!(
            events
                .iter()
                .filter(|e| matches!(e, Event::ScopeFailed { name } if name == "work"))
                .count(),
            1
        );
        assert_eq!(
            events
                .iter()
                .filter(|e| matches!(e, Event::Mode { .. }))
                .count(),
            16
        );
    }

    #[test]
    fn final_result_after_empty_poll_is_not_discarded() {
        let (worker, _, sender) = worker();
        let mut local = ready(worker);
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
        assert!(local.poll().is_none());
        assert!(local.ended());
    }

    #[test]
    fn cancellation_reaches_queued_and_running_work_even_when_the_queue_is_full() {
        let (worker, received, _sender) = worker();
        let mut local = ready(worker);
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

    #[test]
    fn local_relays_init_the_payloads_exit_and_request_connections() {
        let directory = unix::temp_directory().unwrap();
        let (worker, _, _results) = worker();
        let mut local = ready(worker);
        let round = |local: &mut Local| std::iter::from_fn(|| local.poll()).collect::<Vec<_>>();
        // Before Started there is no init to ask.
        local
            .send(Command::Spawn {
                id: 1,
                kind: SpawnKind::Exec,
                argv: None,
                cwd: None,
                env: vec![],
                stdio: Stdio::NewPty { rows: 24, cols: 80 },
            })
            .unwrap();
        assert!(matches!(
            round(&mut local).as_slice(),
            [Event::SpawnFailed { id: 1, code: None, message }]
                if message == "the goblin has no init channel"
        ));
        let (control, sandbox) = init::pair().unwrap();
        let (exit, exited) = unix::pipe().unwrap();
        let listener = UnixListener::bind(directory.join("request.sock")).unwrap();
        listener.set_nonblocking(true).unwrap();
        local.init = Some(control);
        local.exit_watch = Some(exit);
        local.listener = Some(listener);
        let mut buffer = vec![0; init::MAX_MESSAGE];
        let mut sent = || {
            let (n, fds) = init::receive(sandbox.as_raw_fd(), &mut buffer, 0).unwrap();
            (
                serde_json::from_slice::<init::Command>(&buffer[..n]).unwrap(),
                fds.len(),
            )
        };
        let reply = |event: init::Event| {
            let bytes = serde_json::to_vec(&event).unwrap();
            init::send(sandbox.as_raw_fd(), &bytes, &[]).unwrap();
        };
        // A new PTY: its slave goes to init, its master comes back with
        // Spawned.
        local
            .send(Command::Spawn {
                id: 2,
                kind: SpawnKind::Exec,
                argv: Some(vec!["client".into()]),
                cwd: None,
                env: vec![],
                stdio: Stdio::NewPty { rows: 24, cols: 80 },
            })
            .unwrap();
        let (command, fds) = sent();
        assert!(matches!(
            command,
            init::Command::Spawn {
                id: 2,
                tty: true,
                ..
            }
        ));
        assert_eq!(fds, 1);
        reply(init::Event::Spawned { id: 2, pid: 42 });
        reply(init::Event::Failed {
            id: 3,
            code: 127,
            message: "not found".into(),
        });
        reply(init::Event::Exited {
            id: 2,
            code: Some(0),
            signal: None,
        });
        let events = round(&mut local);
        assert!(matches!(
            events.as_slice(),
            [
                Event::Spawned {
                    id: 2,
                    pid: 42,
                    master: Some(_)
                },
                Event::SpawnFailed {
                    id: 3,
                    code: Some(127),
                    ..
                },
                Event::Exited {
                    id: 2,
                    code: Some(0),
                    signal: None
                },
            ]
        ));
        // Signals and cancellations reach init in the order sent.
        local.send(Command::Signal { id: 2, signal: 2 }).unwrap();
        local
            .send(Command::CancelProcess {
                id: 2,
                everything: true,
            })
            .unwrap();
        local.send(Command::KillAll).unwrap();
        assert_eq!(sent().0, init::Command::Signal { id: 2, signal: 2 });
        assert_eq!(
            sent().0,
            init::Command::Cancel {
                id: 2,
                everything: true
            }
        );
        assert_eq!(sent().0, init::Command::KillAll {});
        // A request connection, then the payload's exit, reported once.
        let _client =
            std::os::unix::net::UnixStream::connect(directory.join("request.sock")).unwrap();
        // Data, not a closed end: a process another test forks may hold the
        // write end open for a moment, which would delay the end of file.
        assert_eq!(
            unsafe { libc::write(exited.as_raw_fd(), b"x".as_ptr().cast(), 1) },
            1
        );
        let events = round(&mut local);
        assert!(matches!(
            events.as_slice(),
            [Event::SandboxOpened { .. }, Event::PayloadExited]
        ));
        assert!(round(&mut local).is_empty());
        // Stop retains the request socket for initialize/messaging until Stopped;
        // init's end closes its channel.
        local.send(Command::Stop).unwrap();
        assert!(local.listener.is_some());
        // Shut the peer down, so init's end reads as closed at once, even
        // while a process another test forks holds a copy of the descriptor.
        assert_eq!(
            unsafe { libc::shutdown(sandbox.as_raw_fd(), libc::SHUT_RDWR) },
            0
        );
        drop(sandbox);
        assert!(matches!(round(&mut local).as_slice(), [Event::InitLost]));
        local.send(Command::KillAll).unwrap();
        assert!(round(&mut local).is_empty());
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn what_send_reports_never_delays_collecting_a_round() {
        let (worker, _, results) = worker();
        let mut local = ready(worker);
        let (exit, exited) = unix::pipe().unwrap();
        local.exit_watch = Some(exit);
        // A spawn without init fails between rounds...
        local
            .send(Command::Spawn {
                id: 1,
                kind: SpawnKind::Exec,
                argv: None,
                cwd: None,
                env: vec![],
                stdio: Stdio::NewPty { rows: 24, cols: 80 },
            })
            .unwrap();
        results
            .try_send(Event::Mode { server: false })
            .unwrap_or_else(|_| panic!());
        // Data, not a closed end: a process another test forks may hold the
        // write end open for a moment, which would delay the end of file.
        assert_eq!(
            unsafe { libc::write(exited.as_raw_fd(), b"x".as_ptr().cast(), 1) },
            1
        );
        // ...and the next round still collects every source.
        let round: Vec<_> = std::iter::from_fn(|| local.poll()).collect();
        assert!(matches!(
            round.as_slice(),
            [
                Event::SpawnFailed {
                    id: 1,
                    code: None,
                    ..
                },
                Event::Mode { server: false },
                Event::PayloadExited,
            ]
        ));
    }

    /// A `Started` for the fixture configuration on `support`, with attach
    /// handlers when asked.
    fn started(support: Support, handlers: bool) -> Event {
        let inheritance = Inheritance::fixture("shell", &[]);
        let mut policy = inheritance.policy();
        if handlers {
            policy.handlers = Some(goblins_runtime::config::Handlers {
                on_attach: goblins_runtime::config::OnAttach {
                    command: vec!["client".into()],
                    timeout: 30,
                    wait_for: vec![],
                },
                on_detach: None,
            });
        }
        Event::Started {
            docker_enabled: false,
            policy: Box::new(policy),
            support,
            dev_shell: None,
            master: unix::pty(24, 80).unwrap().0,
            identity: Identity {
                pid: 1,
                helper_userns: 0,
                mount_owner: 0,
                source_mountns: 0,
                mountns: 0,
            },
            recreated: None,
            auto: Box::default(),
        }
    }
    /// A local runtime that has started on a plain configuration.
    fn ready(worker: Worker) -> Local {
        let mut local = Local::new(worker, PathBuf::from("/nonexistent"));
        local.capabilities = Some((Policy::fixture("shell", &[]), Support::linux()));
        local
    }

    #[test]
    fn relay_endpoints_wait_for_started_and_survive_stop_until_stopped() {
        use std::io::Write;
        let directory = unix::temp_directory().unwrap();
        let listener = UnixListener::bind(directory.join("request.sock")).unwrap();
        listener.set_nonblocking(true).unwrap();
        let mut client =
            std::os::unix::net::UnixStream::connect(directory.join("request.sock")).unwrap();
        let (mut worker, _, results) = worker();
        let (sender, endpoints) = mpsc::sync_channel(1);
        worker.endpoints = endpoints;
        let (control, _sandbox) = init::pair().unwrap();
        let (exit_watch, _exit) = unix::pipe().unwrap();
        sender
            .send(Box::new(Endpoints {
                control,
                listener,
                exit_watch,
            }))
            .unwrap();
        let mut local = Local::new(worker, directory.join("host-jobs"));
        assert!(
            std::iter::from_fn(|| local.poll())
                .collect::<Vec<_>>()
                .is_empty()
        );
        results.send(started(Support::linux(), false)).unwrap();
        let events: Vec<_> = std::iter::from_fn(|| local.poll()).collect();
        assert!(matches!(
            events.as_slice(),
            [
                Event::Started { .. },
                Event::SandboxOpened { connection: 1 }
            ]
        ));
        local.send(Command::Stop).unwrap();
        client
            .write_all(
                &goblins_jsonrpc::encode(&goblins_jsonrpc::request(
                    serde_json::json!(1),
                    "initialize",
                    serde_json::json!({"api":1}),
                ))
                .unwrap(),
            )
            .unwrap();
        let events: Vec<_> = std::iter::from_fn(|| local.poll()).collect();
        assert!(
            matches!(events.as_slice(), [Event::SandboxCall { call, .. }] if call.method == "initialize")
        );
        local
            .send(Command::SandboxSend {
                connection: 1,
                message: goblins_jsonrpc::result(
                    serde_json::json!(1),
                    serde_json::json!({"api":1}),
                ),
            })
            .unwrap();
        assert!(
            std::iter::from_fn(|| local.poll())
                .collect::<Vec<_>>()
                .is_empty()
        );
        assert_eq!(
            goblins_jsonrpc::read_started(&mut client, Some(std::time::Instant::now())).unwrap()["result"]
                ["api"],
            1
        );
        client
            .write_all(
                &goblins_jsonrpc::encode(&goblins_jsonrpc::request(
                    serde_json::json!(2),
                    "inbox.status",
                    serde_json::json!({}),
                ))
                .unwrap(),
            )
            .unwrap();
        let events: Vec<_> = std::iter::from_fn(|| local.poll()).collect();
        assert!(
            matches!(events.as_slice(), [Event::SandboxCall { call, .. }] if call.method == "inbox.status")
        );
        results.send(Event::Stopped { exit_code: Some(0) }).unwrap();
        let events: Vec<_> = std::iter::from_fn(|| local.poll()).collect();
        assert!(matches!(
            events.as_slice(),
            [
                Event::SandboxClosed { connection: 1 },
                Event::Stopped { .. }
            ]
        ));
        assert!(local.listener.is_none());
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn relay_send_and_close_are_not_refused_by_a_full_worker_queue() {
        let (worker, _received, _results) = worker();
        let mut local = ready(worker);
        for approval in 0..4 {
            local
                .send(Command::Preview {
                    approval,
                    package: "hello".into(),
                })
                .unwrap();
        }
        assert!(
            local
                .send(Command::Preview {
                    approval: 5,
                    package: "hello".into()
                })
                .is_err()
        );
        local
            .send(Command::SandboxSend {
                connection: 1,
                message: serde_json::json!({}),
            })
            .unwrap();
        local.send(Command::SandboxClose { connection: 1 }).unwrap();
        assert!(!local.stopping);
    }
    fn spawn(kind: SpawnKind) -> Command {
        Command::Spawn {
            id: 1,
            kind,
            argv: None,
            cwd: None,
            env: vec![],
            stdio: Stdio::NewPty { rows: 24, cols: 80 },
        }
    }

    #[test]
    fn a_command_the_goblin_may_not_be_sent_fails_it_closed() {
        let (worker, received, results) = worker();
        let mut local = ready(worker);
        results
            .try_send(started(Support::linux(), false))
            .unwrap_or_else(|_| panic!());
        assert!(matches!(local.poll(), Some(Event::Started { .. })));
        // The fixture's configuration allows no host commands: the command
        // is not run, the goblin fails and stops.
        local
            .send(Command::RunHostJob {
                request: "r1".into(),
                argv: vec!["true".into()],
                env: vec![],
                timeout: None,
            })
            .unwrap();
        assert!(local.jobs.is_empty());
        let events: Vec<_> = std::iter::from_fn(|| local.poll()).collect();
        assert!(matches!(
            events.as_slice(),
            [Event::Failed(message)] if message.contains("host commands")
        ));
        assert!(local.worker.cancel.check().is_err());
        // What the policy allows still reaches the worker.
        local
            .send(Command::Preview {
                approval: 1,
                package: "hello".into(),
            })
            .unwrap();
        assert!(received.try_recv().is_ok());
    }

    #[test]
    fn spawns_are_checked_by_what_they_are_for_and_nothing_runs_before_started() {
        use goblins_runtime::policy::Feature;
        // Before Started the runtime knows nothing it could allow.
        let (idle, _received, _results) = worker();
        let mut local = Local::new(idle, PathBuf::from("/nonexistent"));
        local.send(spawn(SpawnKind::Exec)).unwrap();
        let events: Vec<_> = std::iter::from_fn(|| local.poll()).collect();
        assert!(matches!(
            events.as_slice(),
            [Event::Failed(message)] if message == "the goblin has not started"
        ));
        // Cancelling and stopping are never refused.
        local.send(Command::Cancel(1)).unwrap();
        local.send(Command::KillAll).unwrap();
        local.send(Command::Stop).unwrap();
        assert!(std::iter::from_fn(|| local.poll()).next().is_none());
        // Each kind needs its own feature; a client also needs handlers.
        let unsupported = |name: &str| format!("not supported by this sandbox: {name}");
        let mut cases = vec![];
        for (kind, missing, handlers, name) in [
            (SpawnKind::Exec, Some(Feature::Exec), false, Some("exec")),
            (SpawnKind::Exec, None, false, None),
            (SpawnKind::Attach, None, false, Some("no attach handlers")),
            (
                SpawnKind::Attach,
                Some(Feature::AttachClients),
                true,
                Some("attach-clients"),
            ),
            (SpawnKind::Attach, None, true, None),
        ] {
            let (idle, _received, results) = worker();
            let mut local = Local::new(idle, PathBuf::from("/nonexistent"));
            let mut support = Support::linux();
            if let Some(missing) = missing {
                support.0.remove(&missing);
            }
            results
                .try_send(started(support, handlers))
                .unwrap_or_else(|_| panic!());
            assert!(matches!(local.poll(), Some(Event::Started { .. })));
            local.send(spawn(kind)).unwrap();
            let events: Vec<_> = std::iter::from_fn(|| local.poll()).collect();
            cases.push(match (events.as_slice(), name) {
                ([Event::Failed(message)], Some(name)) => {
                    // The goblin fails closed with the refusal's message.
                    assert!(
                        *message == unsupported(name) || message == name,
                        "{message}"
                    );
                    "refused"
                }
                // Allowed through, so it reaches init, which is not there.
                ([Event::SpawnFailed { .. }], None) => "allowed",
                (events, name) => panic!("{kind:?} {name:?}: {} events", events.len()),
            });
        }
        assert_eq!(
            cases,
            ["refused", "allowed", "refused", "refused", "allowed"]
        );
    }

    #[test]
    fn an_export_is_answered_at_once_and_a_start_reaches_the_waiting_member() {
        let (idle, received, _results) = worker();
        let mut local = ready(idle);
        let round = |local: &mut Local| std::iter::from_fn(|| local.poll()).collect::<Vec<_>>();
        let export = |local: &mut Local, member: &str| {
            local
                .send(Command::Export {
                    member: member.into(),
                })
                .unwrap();
            round(local)
        };
        // Before the goblin started there is nothing to hand over.
        assert!(matches!(
            export(&mut local, "m0").as_slice(),
            [Event::Exported { member, result: Err(error) }]
                if member == "m0" && error == "the goblin has not started"
        ));
        // Once its worker published its capability, the answer comes from
        // that, whatever the worker is busy with.
        let directory = unix::temp_directory().unwrap();
        let fixture = || Inheritance::fixture_in("shell", &[], directory.clone());
        *local.published.lock().unwrap() = Some(fixture());
        let events = export(&mut local, "m1");
        let [
            Event::Exported {
                member,
                result: Ok(handoff),
            },
        ] = events.as_slice()
        else {
            panic!("expected the handoff: {}", events.len());
        };
        assert_eq!(member, "m1");
        assert_eq!(handoff.leases.len(), 1);
        assert!(received.try_recv().is_err(), "the worker was not asked");
        // Exports keep being answered while the goblin stops.
        local.send(Command::Stop).unwrap();
        assert!(matches!(
            export(&mut local, "m2").as_slice(),
            [Event::Exported { result: Ok(_), .. }]
        ));
        // A member's Start goes to the worker that waits for it.
        let (start, waiting) = mpsc::sync_channel(1);
        local.start = Some(start);
        local
            .send(Command::Start(Box::new(fixture().handoff().unwrap())))
            .unwrap();
        assert!(waiting.try_recv().is_ok());
    }

    #[test]
    fn an_export_made_while_a_refresh_publishes_is_one_whole_generation() {
        let _no_forks = unix::no_forks();
        let (idle, _received, _results) = worker();
        let mut local = ready(idle);
        let (one, two) = (
            unix::temp_directory().unwrap(),
            unix::temp_directory().unwrap(),
        );
        // Two capabilities that differ in everything an export carries: the
        // directory it leases and the Docker flag. A mixture of the two
        // would be a torn read.
        let first = Inheritance::fixture_in("shell", &[], one.clone());
        first.set_docker(true);
        let second = Inheritance::fixture_in("shell", &[], two.clone());
        *local.published.lock().unwrap() = Some(first.clone());
        let published = local.published.clone();
        let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let flipper = {
            let stop = stop.clone();
            std::thread::spawn(move || {
                let mut flip = false;
                while !stop.load(std::sync::atomic::Ordering::Relaxed) {
                    let next = if flip { first.clone() } else { second.clone() };
                    *published.lock().unwrap() = Some(next);
                    flip = !flip;
                }
            })
        };
        for _ in 0..500 {
            local.send(Command::Export { member: "m".into() }).unwrap();
            let Some(Event::Exported {
                result: Ok(handoff),
                ..
            }) = local.poll()
            else {
                panic!("expected a handoff");
            };
            // The Docker flag and the lease belong to the same generation.
            assert_eq!(handoff.docker, handoff.leases[0].directory() == one);
            while local.poll().is_some() {}
        }
        stop.store(true, std::sync::atomic::Ordering::Relaxed);
        flipper.join().unwrap();
    }

    #[test]
    fn a_start_a_worker_cannot_take_fails_the_launch_instead_of_leaving_it_waiting() {
        let _no_forks = unix::no_forks();
        // Each in a directory of its own: the last lease on one removes it.
        let handoff = || Box::new(Inheritance::fixture("shell", &[]).handoff().unwrap());
        let failed = |local: &mut Local| {
            let events: Vec<_> = std::iter::from_fn(|| local.poll()).collect();
            matches!(
                events.as_slice(),
                [Event::Failed(message)] if message == "the launch could not take its owner's handoff"
            )
        };
        // A worker that has gone.
        let (idle, _received, _results) = worker();
        let mut local = ready(idle);
        let (start, waiting) = mpsc::sync_channel(1);
        drop(waiting);
        local.start = Some(start);
        local.send(Command::Start(handoff())).unwrap();
        assert!(failed(&mut local));
        assert!(local.worker.cancel.check().is_err());
        // A worker that was already given one.
        let (idle, _received, _results) = worker();
        let mut local = ready(idle);
        let (start, _waiting) = mpsc::sync_channel(1);
        local.start = Some(start);
        local.send(Command::Start(handoff())).unwrap();
        assert!(!failed(&mut local));
        local.send(Command::Start(handoff())).unwrap();
        assert!(failed(&mut local));
        // A runtime that is not waiting for one at all.
        let (idle, _received, _results) = worker();
        let mut local = ready(idle);
        local.send(Command::Start(handoff())).unwrap();
        assert!(failed(&mut local));
    }

    #[test]
    fn a_manifest_root_nobody_adopts_goes_with_its_event() {
        let directory = unix::temp_directory().unwrap();
        let root = directory.join("root");
        std::fs::write(&root, "").unwrap();
        let event = |name: &str| Event::Manifest {
            sequence: 1,
            lease: goblins_runtime::config::ManifestLease::adopt(
                directory.join(name),
                Some(root.clone()),
            ),
        };
        // Queued and never read: the queue's end takes the event with it.
        let (sender, receiver) = mpsc::sync_channel(1);
        sender
            .try_send(event("queued.json"))
            .unwrap_or_else(|_| panic!());
        assert!(root.exists());
        drop(receiver);
        drop(sender);
        assert!(!root.exists());
        // Refused because the queue is full: the event comes back, and
        // dropping it removes the root.
        std::fs::write(&root, "").unwrap();
        let (sender, _receiver) = mpsc::sync_channel(1);
        sender
            .try_send(Event::Mode { server: false })
            .unwrap_or_else(|_| panic!());
        let refused = sender.try_send(event("refused.json"));
        assert!(refused.is_err());
        assert!(root.exists());
        drop(refused);
        assert!(!root.exists());
        std::fs::remove_dir_all(directory).unwrap();
    }
}

fn unsent<T>(error: std::sync::mpsc::TrySendError<T>) -> T {
    match error {
        std::sync::mpsc::TrySendError::Full(value)
        | std::sync::mpsc::TrySendError::Disconnected(value) => value,
    }
}
fn refusal_message(refusal: goblins_runtime::policy::Refusal) -> String {
    use goblins_runtime::policy::Refusal;
    match refusal {
        Refusal::Policy { message, .. } => message,
        Refusal::Unsupported(feature) => format!(
            "not supported by this sandbox: {}",
            serde_json::to_value(feature)
                .ok()
                .and_then(|v| v.as_str().map(str::to_string))
                .unwrap_or_default()
        ),
    }
}
