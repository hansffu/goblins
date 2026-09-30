//! Host-owned mailbox state machine, independent of native agent and transport.
//!
//! The caller supplies authenticated identity and trusted session metadata.
//! Execute this core on a serialized host worker: its audit sink may block.
//! A transition is published only after the sink accepts its audit record.
use goblins_protocol::messages::{Message, Mutation, State};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::BTreeMap;

pub type Fault = (i32, String);
pub type Result<T> = std::result::Result<T, Fault>;

fn absent() -> Fault {
    (-32004, "record absent or unavailable".into())
}
fn conflict() -> Fault {
    (-32009, "stale or conflicting operation".into())
}
fn capacity() -> Fault {
    (-32010, "mailbox capacity reached".into())
}

/// Host metadata, never deserialized from an agent request.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Session {
    pub id: String,
    pub parent: Option<String>,
    pub path: String,
    /// True while starting/running and eligible to accept messages.
    pub running: bool,
}
pub type Directory = BTreeMap<String, Session>;

/// A session's raid membership as the communications log has recorded it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RaidView {
    pub id: String,
    pub name: String,
    pub role: Option<String>,
    pub owner: String,
}
/// Session ID -> logged membership. Built only from recorded raid events,
/// never from the controller's copy, so a join authorizes nothing until it
/// is in the log.
pub type Logged = BTreeMap<String, RaidView>;

#[derive(Clone, Copy, Debug)]
pub struct Limits {
    pub messages: usize,
    pub operations: usize,
    pub inbox: usize,
}
impl Default for Limits {
    fn default() -> Self {
        Self {
            messages: 4096,
            operations: 16384,
            inbox: 256,
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Event {
    pub instance: String,
    pub sequence: u64,
    pub timestamp: u64,
    pub kind: String,
    pub actor: String,
    pub sessions: Vec<String>,
    pub data: Value,
}

#[derive(Debug)]
pub enum AuditError {
    Capacity,
    Unavailable,
}
/// An uncertain write returns Unavailable and disables subsequent mutations.
/// Capacity rejects only that operation, preserving room for terminal outcomes.
pub trait Audit {
    fn append(
        &mut self,
        event: &Event,
        reserved_bytes: usize,
    ) -> std::result::Result<(), AuditError>;
}

struct Receipt {
    command: Mutation,
    result: Value,
}

pub struct Mailbox {
    instance: String,
    limits: Limits,
    sequence: u64,
    messages: Vec<Message>,
    receipts: BTreeMap<(String, String, String), Receipt>,
    audit_failed: bool,
    auxiliary_events: usize,
    raids: Logged,
    /// A raid event could not be recorded for lack of log capacity. Raid
    /// routes stay closed from then on; tree routes are unaffected.
    raid_audit_failed: bool,
}

struct Change {
    messages: Vec<Message>,
    result: Value,
    kind: &'static str,
    data: Value,
}

impl Mailbox {
    pub fn new(instance: String, limits: Limits) -> Result<Self> {
        // Leave room in protocol identifiers for the event/message suffix.
        if instance.len() != 32 || !instance.bytes().all(|b| b.is_ascii_hexdigit()) {
            return Err((-32602, "invalid daemon instance".into()));
        }
        Ok(Self {
            instance,
            limits,
            sequence: 0,
            messages: vec![],
            receipts: BTreeMap::new(),
            audit_failed: false,
            auxiliary_events: 0,
            raids: Logged::new(),
            raid_audit_failed: false,
        })
    }

    pub fn get(&self, actor: &str, id: &str) -> Result<&Message> {
        self.messages
            .iter()
            .find(|m| m.id == id && (actor == "host" || m.sender == actor || m.recipient == actor))
            .ok_or_else(absent)
    }

    pub fn record(
        &mut self,
        actor: &str,
        kind: &str,
        data: Value,
        now: u64,
        audit: &mut impl Audit,
    ) -> Result<()> {
        if self.audit_failed {
            return Err((-32009, "mailbox audit unavailable".into()));
        }
        if self.auxiliary_events >= 16384 {
            return Err(capacity());
        }
        let event = Event {
            instance: self.instance.clone(),
            sequence: self.sequence + 1,
            timestamp: now,
            kind: kind.into(),
            actor: actor.into(),
            sessions: data["message"]
                .as_str()
                .and_then(|id| self.messages.iter().find(|m| m.id == id))
                .map_or_else(
                    || vec![actor.into()],
                    |m| vec![m.sender.clone(), m.recipient.clone()],
                ),
            data,
        };
        let reserved = self
            .messages
            .iter()
            .filter(|m| matches!(m.state, State::Queued | State::Claimed))
            .count()
            * 2048;
        if let Err(e) = audit.append(&event, reserved) {
            return Err(self.audit_error(e));
        }
        self.sequence += 1;
        self.auxiliary_events += 1;
        Ok(())
    }

    pub fn raids(&self) -> &Logged {
        &self.raids
    }

    /// Record a batch of controller raid events in order, applying each to
    /// the logged membership once it is written. Removals apply even when
    /// their event cannot be written, since they only take access away;
    /// additions apply only when recorded. Raid events do not count against
    /// the auxiliary-event limit: the controller bounds them.
    pub fn record_raid(&mut self, events: &[crate::raid::Event], now: u64, audit: &mut impl Audit) {
        for raid in events {
            let addition = raid.addition();
            if addition && (self.audit_failed || self.raid_audit_failed) {
                continue;
            }
            let recorded = !self.audit_failed && {
                let event = Event {
                    instance: self.instance.clone(),
                    sequence: self.sequence + 1,
                    timestamp: now,
                    kind: raid.kind().into(),
                    actor: raid.actor.clone(),
                    sessions: raid.sessions(),
                    data: raid.data(),
                };
                let reserved = self
                    .messages
                    .iter()
                    .filter(|m| matches!(m.state, State::Queued | State::Claimed))
                    .count()
                    * 2048;
                match audit.append(&event, reserved) {
                    Ok(()) => {
                        self.sequence += 1;
                        true
                    }
                    Err(AuditError::Capacity) => {
                        self.raid_audit_failed = true;
                        false
                    }
                    Err(AuditError::Unavailable) => {
                        self.audit_failed = true;
                        false
                    }
                }
            };
            if recorded || !addition {
                apply_raid(&mut self.raids, &raid.change);
            }
        }
    }

    pub fn status(&self, actor: &str) -> Value {
        let inbox = || self.messages.iter().filter(|m| m.recipient == actor);
        json!({
            "queued":inbox().filter(|m| m.state == State::Queued).count(),
            "claim":inbox().find(|m| m.state == State::Claimed).map(|m| &m.id),
            "head":inbox().find(|m|matches!(m.state,State::Queued|State::Claimed)).map(|m|json!([m.id,m.claim_generation])),
            "revision":self.sequence,
            "audit_failed":self.audit_failed,
            "raid_audit_failed":self.raid_audit_failed,
        })
    }

    /// Identical retries are recovered before target resolution or liveness
    /// checks. Reused names therefore cannot redirect a committed operation.
    pub fn execute(
        &mut self,
        actor: &str,
        command: Mutation,
        directory: &Directory,
        now: u64,
        audit: &mut impl Audit,
    ) -> Result<Value> {
        command.validate().map_err(|e| (-32602, e))?;
        let receipt_key = (
            actor.to_owned(),
            command.method().into(),
            command.key().into(),
        );
        if let Some(receipt) = self.receipts.get(&receipt_key) {
            if receipt.command != command {
                return Err(conflict());
            }
            let result = receipt.result.clone();
            // A previously committed receipt remains recoverable even when
            // audit IO has failed. No mailbox transition is repeated.
            let _ = self.record(
                actor,
                "operation.retried",
                json!({"key":command.key(),"method":command.method(),"message":result.get("id").or_else(||result.get("reply").and_then(|m|m.get("id"))).or_else(||result.get("message").and_then(|m|m.get("id"))).or_else(||result.get("completed"))}),
                now,
                audit,
            );
            return Ok(result);
        }
        if self.audit_failed {
            return Err((-32009, "mailbox audit unavailable".into()));
        }
        live(directory, actor)?;
        let sequence = self.sequence.checked_add(1).ok_or_else(capacity)?;
        let change = self.prepare(actor, &command, directory, sequence, now)?;
        let reserved = self.check_capacity(&change.messages)?;
        let event = Event {
            instance: self.instance.clone(),
            sequence,
            timestamp: now,
            kind: change.kind.into(),
            actor: actor.into(),
            sessions: change
                .messages
                .iter()
                .flat_map(|m| [m.sender.clone(), m.recipient.clone()])
                .collect(),
            data: json!({"key":command.key(),"method":command.method(),"change":change.data}),
        };
        if let Err(error) = audit.append(&event, reserved * 2048) {
            return Err(self.audit_error(error));
        }
        self.apply(change.messages);
        self.sequence = sequence;
        let result = change.result;
        self.receipts.insert(
            receipt_key,
            Receipt {
                command,
                result: result.clone(),
            },
        );
        Ok(result)
    }

    fn prepare(
        &self,
        actor: &str,
        command: &Mutation,
        directory: &Directory,
        sequence: u64,
        now: u64,
    ) -> Result<Change> {
        match command {
            Mutation::Send(p) => {
                let (recipient, raid) = resolve(directory, &self.raids, actor, &p.to)?;
                if raid.is_some() && self.raid_audit_failed {
                    return Err(raid_unrecorded());
                }
                let conversation = match &p.conversation {
                    Some(id)
                        if self.messages.iter().any(|m| {
                            m.conversation == *id && (m.sender == actor || m.recipient == actor)
                        }) =>
                    {
                        id.clone()
                    }
                    Some(_) => return Err(absent()),
                    None => format!("{}-c{sequence}", self.instance),
                };
                let message = self.new_message(
                    actor,
                    recipient,
                    &p.body,
                    conversation,
                    None,
                    directory,
                    sequence,
                    now,
                );
                // A raid route is one only logged membership authorized.
                Ok(Change {
                    result: json!(message),
                    data: json!({"message":message,"route":if raid.is_some() {"raid"} else {"tree"},"raid":raid}),
                    messages: vec![message],
                    kind: "message.accepted",
                })
            }
            Mutation::Next(_) => {
                let current = self
                    .messages
                    .iter()
                    .find(|m| m.recipient == actor && m.state == State::Claimed)
                    .or_else(|| {
                        self.messages
                            .iter()
                            .find(|m| m.recipient == actor && m.state == State::Queued)
                    });
                if let Some(current) = current {
                    let mut message = current.clone();
                    message.state = State::Claimed;
                    if message.claim_generation == 0 {
                        message.claim_generation = 1;
                    }
                    Ok(Change {
                        result: json!({"message":message,"claim_generation":message.claim_generation}),
                        data: json!({"message":message.id,"claim_generation":message.claim_generation}),
                        messages: vec![message],
                        kind: "message.claimed",
                    })
                } else {
                    Ok(Change {
                        messages: vec![],
                        result: json!({"message":null,"claim_generation":null}),
                        kind: "inbox.empty",
                        data: json!({}),
                    })
                }
            }
            Mutation::Reply(p) => {
                let mut original = self.claim(actor, &p.message, p.claim_generation)?.clone();
                live(directory, &original.sender)?;
                // A reply is authorized like a send. Tree relations never
                // change, so only replies that relied on a raid can fail;
                // the claim stays outstanding to be completed or requeued.
                let sender = original.sender.as_str();
                if !(actor == "host"
                    || sender == "host"
                    || descendant(directory, actor, sender)
                    || descendant(directory, sender, actor))
                {
                    if shared_raid(&self.raids, actor, sender).is_none() {
                        return Err((
                            -32009,
                            format!(
                                "not allowed: '{}' is no longer in this goblin's raid",
                                original.sender_path
                            ),
                        ));
                    }
                    if self.raid_audit_failed {
                        return Err(raid_unrecorded());
                    }
                }
                let reply = self.new_message(
                    actor,
                    &original.sender,
                    &p.body,
                    original.conversation.clone(),
                    Some(original.id.clone()),
                    directory,
                    sequence,
                    now,
                );
                original.state = State::Completed;
                Ok(Change {
                    result: json!({"reply":reply,"completed":original.id}),
                    data: json!({"message":reply,"completed":original.id}),
                    messages: vec![original, reply],
                    kind: "message.replied",
                })
            }
            Mutation::Complete(p) => {
                let mut message = self.claim(actor, &p.message, p.claim_generation)?.clone();
                message.state = State::Completed;
                Ok(Change {
                    result: json!({"message":message.id,"state":message.state}),
                    data: json!({"message":message.id,"claim_generation":message.claim_generation}),
                    messages: vec![message],
                    kind: "message.completed",
                })
            }
            Mutation::Requeue(p) => {
                let mut message = self.get(actor, &p.message)?.clone();
                if message.recipient != actor && actor != "host" {
                    return Err(absent());
                }
                if message.state != State::Claimed || message.claim_generation != p.claim_generation
                {
                    return Err(conflict());
                }
                live(directory, &message.recipient)?;
                message.state = State::Queued;
                message.claim_generation = message
                    .claim_generation
                    .checked_add(1)
                    .ok_or_else(capacity)?;
                Ok(Change {
                    result: json!({"message":message.id,"claim_generation":message.claim_generation}),
                    data: json!({"message":message.id,"claim_generation":message.claim_generation,"reason":p.reason}),
                    messages: vec![message],
                    kind: "message.requeued",
                })
            }
        }
    }

    fn claim(&self, actor: &str, id: &str, generation: u64) -> Result<&Message> {
        let message = self.get(actor, id)?;
        if message.recipient != actor {
            return Err(absent());
        }
        if message.state != State::Claimed || message.claim_generation != generation {
            return Err(conflict());
        }
        Ok(message)
    }

    #[allow(clippy::too_many_arguments)]
    fn new_message(
        &self,
        sender: &str,
        recipient: &str,
        body: &str,
        conversation: String,
        in_reply_to: Option<String>,
        directory: &Directory,
        sequence: u64,
        now: u64,
    ) -> Message {
        let path = |id: &str| {
            directory
                .get(id)
                .map_or_else(|| id.to_owned(), |s| s.path.clone())
        };
        Message {
            id: format!("{}-m{sequence}", self.instance),
            conversation,
            sender: sender.into(),
            recipient: recipient.into(),
            sender_path: path(sender),
            recipient_path: path(recipient),
            in_reply_to,
            body: body.into(),
            created_sequence: sequence,
            created_at: now,
            state: State::Queued,
            claim_generation: 0,
            failure: None,
        }
    }

    fn check_capacity(&self, changes: &[Message]) -> Result<usize> {
        let updated = || {
            self.messages
                .iter()
                .map(|old| changes.iter().find(|m| m.id == old.id).unwrap_or(old))
                .chain(
                    changes
                        .iter()
                        .filter(|m| !self.messages.iter().any(|old| old.id == m.id)),
                )
        };
        if updated().count() > self.limits.messages {
            return Err(capacity());
        }
        let mut counts = BTreeMap::<&str, usize>::new();
        let mut reserved_operations = 0;
        for message in updated().filter(|m| m.state.unresolved()) {
            let count = counts.entry(&message.recipient).or_default();
            *count += 1;
            if *count > self.limits.inbox {
                return Err(capacity());
            }
            // Preserve one fetch and one completion for queued items, and one
            // completion for an existing claim, even after ordinary key capacity.
            reserved_operations += if message.state == State::Queued { 2 } else { 1 };
        }
        if self.receipts.len() + 1 + reserved_operations > self.limits.operations {
            return Err(capacity());
        }
        Ok(reserved_operations)
    }

    fn audit_error(&mut self, error: AuditError) -> Fault {
        match error {
            AuditError::Capacity => capacity(),
            AuditError::Unavailable => {
                self.audit_failed = true;
                (-32009, "mailbox audit unavailable".into())
            }
        }
    }

    fn apply(&mut self, messages: Vec<Message>) {
        for message in messages {
            if let Some(existing) = self.messages.iter_mut().find(|m| m.id == message.id) {
                *existing = message;
            } else {
                self.messages.push(message);
            }
        }
    }

    /// Lifecycle input from the daemon. Fail unresolved messages without
    /// consuming retry capacity needed to recover previously accepted calls.
    pub fn stop_recipient(
        &mut self,
        recipient: &str,
        now: u64,
        audit: &mut impl Audit,
    ) -> Result<()> {
        let mut messages: Vec<_> = self
            .messages
            .iter()
            .filter(|m| m.recipient == recipient && m.state.unresolved())
            .cloned()
            .collect();
        if messages.is_empty() {
            return Ok(());
        }
        if self.audit_failed {
            return Err((-32009, "mailbox audit unavailable".into()));
        }
        let sequence = self.sequence.checked_add(1).ok_or_else(capacity)?;
        for message in &mut messages {
            message.state = State::Failed;
            message.failure = Some("recipient stopped".into());
        }
        let event = Event {
            instance: self.instance.clone(),
            sequence,
            timestamp: now,
            kind: "recipient.stopped".into(),
            actor: "host".into(),
            sessions: messages
                .iter()
                .flat_map(|m| [m.sender.clone(), m.recipient.clone()])
                .collect(),
            data: json!({"recipient":recipient,"messages":messages.iter().map(|m| &m.id).collect::<Vec<_>>() }),
        };
        let reserved: usize = self
            .messages
            .iter()
            .filter(|m| m.recipient != recipient && m.state.unresolved())
            .map(|m| if m.state == State::Queued { 2 } else { 1 })
            .sum();
        if let Err(error) = audit.append(&event, reserved * 2048) {
            return Err(self.audit_error(error));
        }
        self.apply(messages);
        self.sequence = sequence;
        Ok(())
    }
}

fn live<'a>(directory: &'a Directory, id: &'a str) -> Result<&'a str> {
    if id == "host" || directory.get(id).is_some_and(|s| s.running) {
        Ok(id)
    } else {
        Err(absent())
    }
}

fn descendant(directory: &Directory, child: &str, ancestor: &str) -> bool {
    let mut current = child;
    // Trusted directory corruption must not produce an infinite parent walk.
    for _ in 0..directory.len() {
        let Some(parent) = directory.get(current).and_then(|s| s.parent.as_deref()) else {
            return false;
        };
        if parent == ancestor {
            return true;
        }
        current = parent;
    }
    false
}

fn raid_unrecorded() -> Fault {
    (-32009, "raid membership could not be recorded".into())
}

fn apply_raid(logged: &mut Logged, change: &crate::raid::Change) {
    use crate::raid::Change;
    match change {
        Change::Created { raid, name, owner } => {
            logged.insert(
                owner.clone(),
                RaidView {
                    id: raid.clone(),
                    name: name.clone(),
                    role: None,
                    owner: owner.clone(),
                },
            );
        }
        Change::Joined {
            raid,
            session,
            role,
        } => {
            // The raid is known once its creation is logged.
            if let Some(view) = logged.values().find(|v| v.id == *raid).cloned() {
                logged.insert(
                    session.clone(),
                    RaidView {
                        role: role.clone(),
                        ..view
                    },
                );
            }
        }
        Change::Role {
            raid,
            session,
            role,
        } => {
            if let Some(view) = logged.get_mut(session).filter(|v| v.id == *raid) {
                view.role = Some(role.clone());
            }
        }
        Change::Left { raid, session, .. } => {
            if logged.get(session).is_some_and(|v| v.id == *raid) {
                logged.remove(session);
            }
        }
        Change::Dissolved { raid, .. } => logged.retain(|_, v| v.id != *raid),
    }
}

fn shared_raid<'a>(logged: &'a Logged, a: &str, b: &str) -> Option<&'a str> {
    logged
        .get(a)
        .zip(logged.get(b))
        .filter(|(a, b)| a.id == b.id)
        .map(|(a, _)| a.id.as_str())
}

/// The recipient, and the raid when logged raid membership was the only
/// authorization. Roles are never matched.
fn resolve<'a>(
    directory: &'a Directory,
    raids: &'a Logged,
    actor: &str,
    target: &str,
) -> Result<(&'a str, Option<&'a str>)> {
    let parent = directory.get(actor).and_then(|s| s.parent.as_deref());
    if actor != "host" && target == "parent" {
        return Ok((live(directory, parent.ok_or_else(absent)?)?, None));
    }
    let tree = |s: &Session| {
        actor == "host" || parent == Some(s.id.as_str()) || descendant(directory, &s.id, actor)
    };
    let allowed = |s: &&Session| {
        s.running && s.id != actor && (tree(s) || shared_raid(raids, actor, &s.id).is_some())
    };
    let mut candidates = directory.values().filter(allowed).filter(|s| {
        s.id == target
            || s.path == target
            || s.path.rsplit('/').next() == Some(target)
            || directory
                .get(actor)
                .is_some_and(|a| s.path.strip_prefix(&format!("{}/", a.path)) == Some(target))
    });
    let first = candidates.next().ok_or_else(absent)?;
    if candidates.next().is_some() {
        return Err((-32009, "ambiguous recipient; use a path or ID".into()));
    }
    let raid = if tree(first) {
        None
    } else {
        shared_raid(raids, actor, &first.id)
    };
    Ok((&first.id, raid))
}

#[cfg(test)]
mod tests;
