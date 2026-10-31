//! Nonblocking daemon client and member placeholder for one supervisor.
use std::path::{Path, PathBuf};
mod child;
pub(crate) use child::Child;
use goblins_runtime::{
    Command, Event, Runtime, Source, TerminalId, TerminalStatus,
    channel::Channel,
    supervisor::{
        self, Decoder, Encoder, Message, SupervisorStart,
        outbound::{Commands, Writer},
    },
};
use std::{
    collections::{BTreeMap, BTreeSet, VecDeque},
    time::{Duration, Instant},
};

const EVENTS: usize = 16;
pub(crate) const ACK_TIMEOUT: Duration = Duration::from_secs(2);
pub(crate) const CLEANUP_TIMEOUT: Duration = Duration::from_secs(15);
pub(crate) const SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(17);
const EXIT_GRACE: Duration = Duration::from_secs(2);

struct Process {
    child: Child,
    channel: Channel,
}
/// The record/receipt/manifest stay in Active; this owns the waiting Source,
/// including scope membership. A member does not spawn until Handoff arrives.
pub(crate) struct Supervised {
    executable: Executable,
    legacy_path: Option<PathBuf>,
    bootstrap: Option<SupervisorStart>,
    process: Option<Process>,
    decoder: Decoder,
    encoder: Encoder,
    writer: Writer,
    commands: Commands,
    /// Ownership control tokens, coalesced by transfer like cancellations.
    /// They never wait behind new commands or prevent reading peer events.
    adoptions: BTreeSet<u64>,
    events: VecDeque<Event>,
    terminals: BTreeMap<TerminalId, TerminalStatus>,
    stop: Option<Instant>,
    stopping: Option<Instant>,
    stopped: bool,
    failed: bool,
    release_seen: bool,
    released: bool,
    failure: Option<String>,
    write_failure: bool,
    exit_grace: Option<Instant>,
    death: Option<Death>,
    polling: bool,
    round: usize,
}
struct Death {
    initial: VecDeque<Event>,
    terminals: std::collections::btree_map::IntoIter<TerminalId, TerminalStatus>,
}
impl Supervised {
    pub fn start(
        executable: Executable,
        source: Source,
        directory: PathBuf,
        dimensions: (u16, u16),
        detached: bool,
    ) -> crate::Result<Self> {
        executable.path()?;
        if !directory.is_absolute() || !matches!(dimensions, (1..=1000, 1..=1000)) {
            return Err("invalid supervisor startup parameters".into());
        }
        let legacy_path = directory
            .parent()
            .ok_or("missing goblin directory")?
            .join("terminal.sock");
        let host = matches!(source, Source::Host { .. });
        let mut this = Self {
            executable,
            legacy_path: None,
            bootstrap: Some(SupervisorStart {
                supervisor_api: supervisor::API,
                source,
                directory,
                dimensions,
                detached,
                legacy_listener: None,
                handoff: None,
            }),
            process: None,
            decoder: Decoder::default(),
            encoder: Encoder::default(),
            writer: Writer::default(),
            commands: Commands::default(),
            adoptions: BTreeSet::new(),
            events: VecDeque::new(),
            terminals: [(TerminalId::Main, TerminalStatus::default())].into(),
            stop: None,
            stopping: None,
            stopped: false,
            failed: false,
            release_seen: false,
            released: false,
            failure: None,
            write_failure: false,
            exit_grace: None,
            death: None,
            polling: false,
            round: 0,
        };
        // Synchronous launch acceptance exposes this path. Only bind/transfer
        // happen here; all acceptance, framing and terminal I/O stay in Engine.
        use std::os::unix::{fs::PermissionsExt, net::UnixListener};
        let listener = UnixListener::bind(&legacy_path)?;
        this.legacy_path = Some(legacy_path.clone());
        listener.set_nonblocking(true)?;
        std::fs::set_permissions(&legacy_path, std::fs::Permissions::from_mode(0o600))?;
        this.bootstrap.as_mut().unwrap().legacy_listener =
            Some(std::os::fd::OwnedFd::from(listener).into());
        if host {
            this.spawn()?;
        }
        Ok(this)
    }
    fn spawn(&mut self) -> crate::Result<()> {
        let (child, socket) = Child::start(self.executable.path()?)?;
        let channel = Channel::new(socket)?;
        self.process = Some(Process { child, channel });
        let start = self.bootstrap.take().ok_or("supervisor already started")?;
        self.writer
            .begin(self.encoder.prepare(Message::SupervisorStart(start))?)
            .map_err(|_| "supervisor bootstrap writer busy")?;
        Ok(())
    }
    fn fail(&mut self, message: String) {
        self.write_failure = false;
        self.exit_grace = None;
        if self.failure.is_none() {
            self.failure = Some(message);
        }
        if let Some(process) = &self.process {
            let _ = process.child.kill();
        }
    }
    fn terminal_outcomes_known(&self) -> bool {
        self.terminals
            .values()
            .all(|s| s.complete || s.interrupted || s.detached)
    }
    fn fail_write(&mut self, error: Box<dyn std::error::Error + Send + Sync>, now: Instant) {
        // Encoding/protocol errors are never forgiven. Only a transport write
        // racing a verified final lifecycle and successful reap can be clean.
        let transport = error.downcast_ref::<std::io::Error>().is_some_and(|e| {
            matches!(
                e.kind(),
                std::io::ErrorKind::BrokenPipe
                    | std::io::ErrorKind::ConnectionReset
                    | std::io::ErrorKind::ConnectionAborted
                    | std::io::ErrorKind::NotConnected
            )
        });
        if transport
            && (self.failure.is_none() || self.write_failure)
            && self.stopped
            && self.release_seen
            && self.terminal_outcomes_known()
        {
            // Control closes before the child returns from its destructors.
            // Let that already settled child return naturally, preserving the
            // first deadline even if another write observes the same closure.
            self.failure
                .get_or_insert_with(|| format!("supervisor control: {error}"));
            self.write_failure = true;
            self.exit_grace.get_or_insert(now + EXIT_GRACE);
        } else {
            self.fail(format!("supervisor control: {error}"));
        }
    }
    fn receive(
        &mut self,
        message: Message,
        adopted: Option<u64>,
        now: Instant,
    ) -> crate::Result<()> {
        match message {
            Message::Event(event) => {
                match &event {
                    Event::Stopped { .. } => self.stopped = true,
                    Event::Failed(_) => self.failed = true,
                    Event::SandboxClosed { connection } => {
                        self.commands.sandbox_closed(*connection)
                    }
                    Event::Terminal {
                        terminal, status, ..
                    } => {
                        if matches!(terminal, TerminalId::Process(_))
                            && !status.connected
                            && (status.complete || status.interrupted || status.detached)
                        {
                            self.terminals.remove(terminal);
                        } else {
                            self.terminals.insert(*terminal, *status);
                        }
                    }
                    _ => (),
                }
                self.events.push_back(event);
            }
            Message::Stopping => {
                let requested = self.stop.ok_or("unsolicited supervisor Stopping")?;
                if self.stopping.is_none() && now.saturating_duration_since(requested) > ACK_TIMEOUT
                {
                    return Err("supervisor did not acknowledge Stop within 2 seconds".into());
                }
                self.stopping.get_or_insert(now);
            }
            Message::Released => {
                if !self.stopped {
                    return Err("supervisor Released before Stopped".into());
                }
                self.release_seen = true;
            }
            Message::Adopted { transfer } => self.writer.adopted(transfer)?,
            _ => return Err("invalid supervisor-to-daemon message".into()),
        }
        // A typed event is owned by our queue before its token is acknowledged.
        if let Some(transfer) = adopted {
            self.adoptions.insert(transfer);
        }
        Ok(())
    }
    fn write_control(&mut self) -> crate::Result<()> {
        if self.failure.is_none()
            && let Some(process) = &mut self.process
            && !process.channel.closed()
        {
            // Finish an indivisible committed frame before an ACK. At a
            // free frame boundary ownership control precedes new requests.
            self.writer.pump(&mut process.channel)?;
            if self.writer.idle(&process.channel)
                && let Some(transfer) = self.adoptions.pop_first()
            {
                self.writer
                    .begin(self.encoder.prepare(Message::Adopted { transfer })?)
                    .map_err(|_| "supervisor writer busy")?;
            }
            self.writer.pump_commands(
                &mut self.commands,
                &mut self.encoder,
                &mut process.channel,
            )?;
        }
        Ok(())
    }
    fn step(&mut self, now: Instant) {
        if self.failure.is_none() {
            // Receive FIRST, regardless of Channel.wants_write or request
            // admission. A blocked outbound frame must not block peer events.
            for _ in 0..EVENTS {
                if self.events.len() == EVENTS {
                    break;
                }
                let Some(process) = &mut self.process else {
                    break;
                };
                match self.decoder.receive_channel(&mut process.channel) {
                    Ok(Some(received)) => {
                        if let Err(e) = self.receive(received.message, received.adopted, now) {
                            self.fail(e.to_string());
                            break;
                        }
                    }
                    Ok(None) => break,
                    Err(e) => {
                        self.fail(format!("supervisor control: {e}"));
                        break;
                    }
                }
            }
        }
        if let Some(start) = self.stop
            && !self.stopped
        {
            let elapsed = now.saturating_duration_since(start);
            if self.stopping.is_none() && elapsed >= ACK_TIMEOUT {
                self.fail("supervisor did not acknowledge Stop within 2 seconds".into());
            } else if self.stopping.is_some_and(|acknowledged| {
                now.saturating_duration_since(acknowledged) >= CLEANUP_TIMEOUT
            }) {
                self.fail(
                    "supervisor did not finish cleanup within 15 seconds after acknowledging Stop"
                        .into(),
                );
            }
        }
        // Stopped ended the Stop deadlines; only this separate first-error
        // grace and the unchanged daemon-wide abort bound apply after it.
        if self.exit_grace.is_some_and(|deadline| now >= deadline)
            && self
                .process
                .as_ref()
                .is_some_and(|p| !p.child.exited().unwrap_or(false))
        {
            self.fail(
                "supervisor did not exit within 2 seconds after final control closure".into(),
            );
        }
        if let Err(e) = self.write_control() {
            self.fail_write(e, now);
        }
        if self.failure.is_none()
            && !self.stopped
            && self.process.as_ref().is_some_and(|p| p.channel.closed())
        {
            self.fail("supervisor control closed before Stopped".into());
        }
        // Sender typed guards survive EOF/error/kill until the actual exit
        // notification and reap, even if the control channel already closed.
        let exited = self.process.as_mut().map(|p| p.child.reap());
        match exited {
            Some(Ok(true))
                if self.failure.is_some() || self.process.as_ref().unwrap().channel.closed() =>
            {
                self.finish_exit()
            }
            Some(Err(e)) => self.fail(format!("supervisor exit watch: {e}")),
            _ => (),
        }
        while self.events.len() < EVENTS {
            if let Some(event) = self.commands.pop_cancelled() {
                self.events.push_back(event);
                continue;
            }
            let Some(death) = &mut self.death else {
                break;
            };
            if let Some(event) = death.initial.pop_front() {
                self.events.push_back(event);
                continue;
            }
            if let Some((terminal, mut status)) = death.terminals.next() {
                status.eof = true;
                status.interrupted = true;
                status.attached = false;
                status.connected = false;
                status.detached = false;
                self.events.push_back(Event::Terminal {
                    terminal,
                    status,
                    error: None,
                });
            } else {
                self.death = None;
                break;
            }
        }
    }
    fn finish_exit(&mut self) {
        let mut process = self.process.take().unwrap(); // already reaped
        self.decoder.close(&mut process.channel);
        self.writer.close(&mut process.channel);
        self.commands = Commands::default();
        self.adoptions.clear();
        self.bootstrap.take();
        self.unlink_legacy();
        self.exit_grace = None;
        let clean = (self.failure.is_none()
            || (self.write_failure && self.terminal_outcomes_known()))
            && self.stopped
            && self.release_seen
            && process.child.successful();
        self.release_seen = true;
        if clean {
            self.terminals.clear();
            return;
        }
        let mut initial = VecDeque::new();
        initial.push_back(Event::InitLost);
        if !self.stopped {
            if !self.failed {
                initial.push_back(Event::Failed(self.failure.take().unwrap_or_else(|| {
                    "supervisor exited without a final Stopped result".into()
                })));
                self.failed = true;
            }
            initial.push_back(Event::Stopped { exit_code: None });
            self.stopped = true;
        }
        self.death = Some(Death {
            initial,
            terminals: std::mem::take(&mut self.terminals).into_iter(),
        });
    }
    fn release(&mut self) {
        if self.release_seen
            && self.events.is_empty()
            && self.death.is_none()
            && self.commands.is_empty()
            && self.adoptions.is_empty()
            && self
                .process
                .as_ref()
                .is_none_or(|p| self.writer.drained(&p.channel))
        {
            self.released = true;
        }
    }
    fn unlink_legacy(&mut self) {
        if let Some(path) = self.legacy_path.take() {
            let _ = std::fs::remove_file(path);
        }
    }
}
impl Runtime for Supervised {
    fn send(&mut self, command: Command) -> Result<(), Command> {
        if matches!(command, Command::Stop | Command::ScopeFailed) && self.stop.is_none() {
            self.stop = Some(Instant::now());
        }
        if self.failure.is_some()
            || self.death.is_some()
            || (self.bootstrap.is_none() && self.process.is_none())
        {
            return Ok(());
        }
        if self.process.is_none() {
            match command {
                Command::Start(handoff) => {
                    self.bootstrap.as_mut().unwrap().handoff = Some(handoff);
                    if let Err(e) = self.spawn() {
                        if self.process.is_some() {
                            self.fail(format!("cannot start supervisor: {e}"));
                        } else {
                            self.bootstrap.take();
                            self.unlink_legacy();
                            self.failed = true;
                            self.stopped = true;
                            self.release_seen = true;
                            self.events
                                .push_back(Event::Failed(format!("cannot start supervisor: {e}")));
                            self.events.push_back(Event::Stopped { exit_code: None });
                        }
                    }
                }
                command @ (Command::Stop | Command::ScopeFailed) => {
                    let exit_code = matches!(command, Command::ScopeFailed).then_some(1);
                    self.bootstrap.take();
                    self.unlink_legacy();
                    self.stopped = true;
                    self.release_seen = true;
                    self.events.push_back(Event::Stopped { exit_code });
                }
                Command::Cancel(_)
                | Command::CancelProcess { .. }
                | Command::CancelHostJob { .. } => (),
                other => return Err(other),
            }
            return Ok(());
        }
        if matches!(command, Command::Start(_)) {
            return Err(command);
        }
        let scope_failure = matches!(command, Command::ScopeFailed);
        self.commands.push(command)?;
        if scope_failure {
            self.commands
                .push(Command::Stop)
                .expect("Stop is never refused");
        }
        Ok(())
    }
    fn poll(&mut self) -> Option<Event> {
        if !self.polling {
            self.step(Instant::now());
            self.polling = true;
            self.round = 0;
        }
        let event = if self.round == EVENTS {
            None
        } else {
            self.events.pop_front()
        };
        self.polling = event.is_some();
        self.round += usize::from(event.is_some());
        if event.is_none() {
            self.release();
        }
        event
    }
    fn released(&self) -> bool {
        self.released
    }
    fn ended(&self) -> bool {
        self.released
            && self.process.is_none()
            && self.bootstrap.is_none()
            && self.events.is_empty()
            && self.death.is_none()
    }
    fn abort(&mut self) {
        self.fail("supervisor terminated during daemon shutdown".into());
    }
}
impl Drop for Supervised {
    fn drop(&mut self) {
        if let Some(mut process) = self.process.take() {
            let _ = process.child.kill();
            drop(process.child); // wait for confirmed exit before sender owners
            self.decoder.close(&mut process.channel);
            self.writer.close(&mut process.channel);
        }
        self.bootstrap.take();
        self.unlink_legacy();
    }
}

/// Captured once per controller, shared by every inherited-socket mode.
#[derive(Clone)]
pub(crate) struct Executable(Option<PathBuf>);
impl Default for Executable {
    fn default() -> Self {
        Self::from_path(std::env::var_os("GOBLINS_SUPERVISOR_RUNTIME").map(PathBuf::from))
    }
}
impl Executable {
    pub(crate) fn from_path(path: Option<PathBuf>) -> Self {
        Self(path)
    }
    pub(crate) fn path(&self) -> crate::Result<&Path> {
        let path = self
            .0
            .as_deref()
            .filter(|p| !p.as_os_str().is_empty())
            .ok_or("supervisor executable not configured (GOBLINS_SUPERVISOR_RUNTIME)")?;
        if !path.is_absolute() {
            return Err(
                "supervisor executable must be an absolute path (GOBLINS_SUPERVISOR_RUNTIME)"
                    .into(),
            );
        }
        Ok(path)
    }
}

#[cfg(test)]
pub(crate) mod tests;
#[cfg(test)]
mod pin_tests {
    use super::*;
    #[test]
    fn executable_requires_an_explicit_absolute_pin_without_path_search() {
        for path in [None, Some(PathBuf::new())] {
            assert!(
                Executable::from_path(path)
                    .path()
                    .unwrap_err()
                    .to_string()
                    .contains("supervisor executable not configured")
            );
        }
        assert!(
            Executable::from_path(Some("goblins-supervisor-bwrap".into()))
                .path()
                .is_err()
        );
        let pin = Executable::from_path(Some("/immutable/supervisor".into()));
        assert_eq!(pin.path().unwrap(), Path::new("/immutable/supervisor"));
    }
}
