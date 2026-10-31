//! Logical daemon commands remain mutable here. Only one frame is committed
//! to Channel at a time; Channel's framing, limits and buffers are unchanged.
use super::{Encoder, PendingTransfer, Prepared, channel, invalid};
use crate::{Command, Event, HostOutcome};
use std::{
    collections::{BTreeMap, BTreeSet, VecDeque},
    io,
};

pub const REQUESTS: usize = 4;
const SIGNALS: usize = 16;
const SANDBOX_BYTES: usize = 2 * 1024 * 1024;

/// Four unsent logical requests, not receiver admission credits. Controls
/// are bounded by the calls, processes and connections that produced them.
#[derive(Default)]
pub struct Commands {
    ordered: VecDeque<Command>,
    cancellations: VecDeque<Command>,
    cancelled: VecDeque<Event>,
    stop: bool,
    stop_sent: bool,
    closed_connections: BTreeSet<u64>,
    in_flight_sandbox: BTreeMap<u64, usize>,
}
fn request(command: &Command) -> bool {
    matches!(
        command,
        Command::Preview { .. }
            | Command::Decide { .. }
            | Command::Refresh { .. }
            | Command::Run { .. }
            | Command::Diff { .. }
            | Command::Spawn { .. }
            | Command::Attach { .. }
            | Command::PrepareHostJob { .. }
            | Command::RunHostJob { .. }
            | Command::Export { .. }
    )
}
fn cancelled(command: &Command) -> Event {
    match command {
        Command::Spawn { id, .. } => Event::SpawnFailed {
            id: *id,
            code: None,
            message: "process cancelled before it started".into(),
        },
        Command::PrepareHostJob { request, .. } => Event::HostJobPrepared {
            request: request.clone(),
            result: Err("session cancelled".into()),
        },
        Command::RunHostJob { request, .. } => Event::HostJobFinished {
            request: request.clone(),
            outcome: Ok(HostOutcome {
                cancelled: true,
                ..Default::default()
            }),
        },
        _ => unreachable!("only accepted spawn/host requests need local completion"),
    }
}
impl Commands {
    pub fn unsent_requests(&self) -> usize {
        self.ordered.iter().filter(|c| request(c)).count()
    }
    pub fn is_empty(&self) -> bool {
        self.ordered.is_empty()
            && self.cancellations.is_empty()
            && self.cancelled.is_empty()
            && !self.stop
    }
    pub fn push(&mut self, command: Command) -> Result<(), Command> {
        // A locally synthesized outcome replaces the removed request in
        // its original slot until the actor has room to report completion.
        if request(&command) && self.unsent_requests() + self.cancelled.len() == REQUESTS {
            return Err(command);
        }
        match command {
            Command::Cancel(id) => {
                let before = self.ordered.len();
                self.ordered.retain(|c| c.cancel_id() != Some(id));
                if before == self.ordered.len()
                    && !self
                        .cancellations
                        .iter()
                        .any(|c| matches!(c,Command::Cancel(other) if *other == id))
                {
                    self.cancellations.push_back(Command::Cancel(id));
                }
            }
            Command::CancelProcess { id, everything } => {
                let unsent = self
                    .ordered
                    .iter()
                    .any(|c| matches!(c,Command::Spawn { id:other,.. } if *other == id));
                if unsent {
                    let outcomes = &mut self.cancelled;
                    self.ordered.retain(|c| match c {
                        Command::Spawn { id: other, .. } if *other == id => {
                            outcomes.push_back(cancelled(c));
                            false
                        }
                        Command::Signal { id: other, .. } if *other == id => false,
                        _ => true,
                    });
                } else if let Some(Command::CancelProcess {
                    everything: old, ..
                }) = self
                    .cancellations
                    .iter_mut()
                    .find(|c| matches!(c,Command::CancelProcess { id:other,.. } if *other == id))
                {
                    *old |= everything;
                } else {
                    self.cancellations
                        .push_back(Command::CancelProcess { id, everything });
                }
            }
            Command::CancelHostJob { request } => {
                let unsent_prepare = self.ordered.iter().any(|c| matches!(c,Command::PrepareHostJob { request:other,.. } if *other == request));
                let outcomes = &mut self.cancelled;
                self.ordered.retain(|c| {
                    if matches!(c,Command::PrepareHostJob {request:other,..}|Command::RunHostJob {request:other,..} if *other==request) {
                        outcomes.push_back(cancelled(c));false
                    } else {true}
                });
                // A sent Prepare may still own its pinned directory even when
                // this cancellation removed an unsent Run.
                if !unsent_prepare && !self.cancellations.iter().any(|c| matches!(c,Command::CancelHostJob { request:other } if *other == request)) {
                    self.cancellations.push_back(Command::CancelHostJob { request });
                }
            }
            Command::Stop => {
                if !self.stop_sent {
                    self.stop = true;
                }
            }
            Command::Signal { id, signal } => {
                if self
                    .ordered
                    .iter()
                    .filter(|c| matches!(c,Command::Signal { id:other,.. } if *other == id))
                    .count()
                    < SIGNALS
                {
                    self.ordered.push_back(Command::Signal { id, signal });
                }
            }
            Command::Resize {
                terminal,
                rows,
                cols,
            } => {
                // An intervening Attach/Detach is an ordering barrier. Never
                // move a newer resize back across that terminal's handoff.
                for queued in self.ordered.iter_mut().rev() {
                    match queued {
                        Command::Attach {
                            terminal: other, ..
                        }
                        | Command::Detach { terminal: other }
                            if *other == terminal =>
                        {
                            break;
                        }
                        Command::Resize {
                            terminal: other,
                            rows: old_rows,
                            cols: old_cols,
                        } if *other == terminal => {
                            *old_rows = rows;
                            *old_cols = cols;
                            return Ok(());
                        }
                        _ => (),
                    }
                }
                self.ordered.push_back(Command::Resize {
                    terminal,
                    rows,
                    cols,
                });
            }
            Command::SandboxSend {
                connection,
                message,
            } => {
                if self.closed_connections.contains(&connection) {
                    return Ok(());
                }
                let bytes = goblins_jsonrpc::encode(&message).map(|b| b.len());
                let queued: usize = self
                    .ordered
                    .iter()
                    .filter_map(|c| match c {
                        Command::SandboxSend {
                            connection: other,
                            message,
                        } if *other == connection => {
                            goblins_jsonrpc::encode(message).ok().map(|b| b.len())
                        }
                        _ => None,
                    })
                    .sum::<usize>()
                    .saturating_add(
                        self.in_flight_sandbox
                            .get(&connection)
                            .copied()
                            .unwrap_or(0),
                    );
                if bytes.is_err()
                    || bytes.is_ok_and(|bytes| queued.saturating_add(bytes) > SANDBOX_BYTES)
                {
                    self.ordered.retain(|c| !matches!(c,Command::SandboxSend { connection:other,.. } if *other == connection));
                    self.closed_connections.insert(connection);
                    self.ordered.push_back(Command::SandboxClose { connection });
                } else {
                    self.ordered.push_back(Command::SandboxSend {
                        connection,
                        message,
                    });
                }
            }
            command => self.ordered.push_back(command),
        }
        Ok(())
    }
    /// Actor-local outcomes for requests removed before transmission. This
    /// bounded producer prevents an accepted spawn/host caller being stranded.
    /// Drain into the actor's event output; never send these back as commands.
    pub fn pop_cancelled(&mut self) -> Option<Event> {
        self.cancelled.pop_front()
    }
    pub fn pop_cancellation(&mut self) -> Option<Command> {
        self.cancellations.pop_front()
    }
    /// Retry a dispatcher refusal without reordering any accepted command.
    pub fn restore_front(&mut self, command: Command) {
        self.ordered.push_front(command);
    }
    pub fn export_members(&self) -> impl Iterator<Item = &str> {
        self.ordered.iter().filter_map(|c| {
            if let Command::Export { member } = c {
                Some(member.as_str())
            } else {
                None
            }
        })
    }
    /// Take only when the frame writer is idle. Once taken for Channel, a
    /// frame is committed even if its first nonblocking write would block.
    pub fn pop(&mut self) -> Option<Command> {
        let command = self
            .cancellations
            .pop_front()
            .or_else(|| self.ordered.pop_front())
            .or_else(|| {
                if std::mem::take(&mut self.stop) {
                    self.stop_sent = true;
                    Some(Command::Stop)
                } else {
                    None
                }
            });
        if let Some((connection, bytes)) = command.as_ref().and_then(sandbox_size) {
            *self.in_flight_sandbox.entry(connection).or_default() += bytes;
        }
        command
    }
    fn sandbox_written(&mut self, connection: u64, bytes: usize) {
        if let Some(queued) = self.in_flight_sandbox.get_mut(&connection) {
            *queued = queued.saturating_sub(bytes);
            if *queued == 0 {
                self.in_flight_sandbox.remove(&connection);
            }
        }
    }
    /// A closed sandbox connection's ID is never reused by that supervisor.
    /// Its lifecycle result releases the small overflow tombstone.
    pub fn sandbox_closed(&mut self, connection: u64) {
        self.closed_connections.remove(&connection);
        self.in_flight_sandbox.remove(&connection);
    }
}

fn sandbox_size(command: &Command) -> Option<(u64, usize)> {
    if let Command::SandboxSend {
        connection,
        message,
    } = command
    {
        goblins_jsonrpc::encode(message)
            .ok()
            .map(|bytes| (*connection, bytes.len()))
    } else {
        None
    }
}

/// One committed frame and full typed ownership for transmitted transfers.
/// The actor drops this state only after receiver/channel teardown (or pidfd
/// exit for supervisor death), as specified by PendingTransfer.
#[derive(Default)]
pub struct Writer {
    current: Option<Prepared>,
    priority: Option<Prepared>,
    current_output: Option<(u64, usize)>,
    completed_output: Option<(u64, usize)>,
    pending: BTreeMap<u64, PendingTransfer>,
}
impl Writer {
    pub fn idle(&self, channel: &channel::Channel) -> bool {
        self.current.is_none() && self.priority.is_none() && !channel.wants_write()
    }
    /// All transmitted owners were adopted, as well as all bytes flushed.
    pub fn drained(&self, channel: &channel::Channel) -> bool {
        self.idle(channel) && self.pending.is_empty()
    }
    pub fn begin(&mut self, prepared: Prepared) -> Result<(), Prepared> {
        if self.current.is_some() || self.priority.is_some() {
            return Err(prepared);
        }
        self.current = Some(prepared);
        Ok(())
    }
    fn completed(&mut self) -> io::Result<()> {
        for main in [false, true] {
            let prepared = if main {
                &mut self.current
            } else {
                &mut self.priority
            };
            if prepared.as_ref().is_some_and(|p| p.frames.is_empty()) {
                let prepared = prepared.take().unwrap();
                if main && self.current_output.is_some() {
                    assert!(
                        self.completed_output.is_none(),
                        "account a completed sandbox frame before committing another"
                    );
                    self.completed_output = self.current_output.take();
                }
                if let Some(pending) = prepared.into_pending()? {
                    let transfer = pending.transfer();
                    if self.pending.contains_key(&transfer) {
                        return Err(invalid("duplicate ownership transfer id"));
                    }
                    self.pending.insert(transfer, pending);
                }
            }
        }
        Ok(())
    }
    fn account_completed(&mut self, commands: &mut Commands) {
        if let Some((connection, bytes)) = self.completed_output.take() {
            commands.sandbox_written(connection, bytes);
        }
    }
    /// A bounded turn commits at most one additional frame. All other
    /// logical commands stay in Commands, available for cancellation/resize.
    pub fn pump(&mut self, channel: &mut channel::Channel) -> io::Result<()> {
        channel.flush()?;
        if channel.wants_write() {
            return Ok(());
        }
        self.completed()?;
        let prepared = if self.priority.is_some() {
            &mut self.priority
        } else {
            &mut self.current
        };
        if let Some(prepared) = prepared {
            if let Some(mut frame) = prepared.frames.pop_front() {
                channel.queue(&frame.value, &mut frame.fds)?;
                channel.flush()?;
            }
            if !channel.wants_write() {
                self.completed()?;
            }
        }
        Ok(())
    }
    pub fn adopted(&mut self, transfer: u64) -> io::Result<()> {
        self.pending
            .get_mut(&transfer)
            .ok_or_else(|| invalid("unexpected ownership acknowledgement"))?
            .adopted(transfer)?;
        self.pending.remove(&transfer);
        Ok(())
    }
    pub fn close(&mut self, channel: &mut channel::Channel) {
        channel.close();
        self.priority.take();
        self.current.take();
        self.current_output.take();
        self.completed_output.take();
        self.pending.clear();
    }
    /// Encode only at commitment, so cancellation has not left stale frames
    /// or duplicate capabilities in Channel's own output queue.
    pub fn pump_commands(
        &mut self,
        commands: &mut Commands,
        encoder: &mut Encoder,
        channel: &mut channel::Channel,
    ) -> io::Result<()> {
        channel.flush()?;
        if channel.wants_write() {
            return Ok(());
        }
        self.completed()?;
        self.account_completed(commands);
        if self.priority.is_none()
            && let Some(command) = commands.cancellations.pop_front()
        {
            // Cancellation may pass between Handoff lease batches; it never
            // overtakes an indivisible frame already inside Channel.
            self.priority = Some(encoder.prepare(super::Message::Command(command))?);
        }
        if self.current.is_none()
            && self.priority.is_none()
            && let Some(command) = commands.pop()
        {
            self.current_output = sandbox_size(&command);
            self.current = Some(encoder.prepare(super::Message::Command(command))?);
        }
        self.pump(channel)?;
        self.account_completed(commands);
        Ok(())
    }
}
