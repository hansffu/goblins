//! Raid wiring for the event loop: target resolution, tree restrictions,
//! launch joins, reconciliation and the bounded event outbox. The rules
//! themselves live in `crate::raid`.
use super::{Controller, Fault, Start, params};
use crate::raid::{self, Event, Raids, Reason};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

/// Events waiting for the messaging worker. Additions are refused when full.
pub(super) const RAID_OUTBOX: usize = 256;
/// Daemon-wide raid events; additions are refused once they would exceed it.
const RAID_EVENTS: usize = 4096;
/// Stands in for a launch's session ID while validating its join.
const LAUNCHING: &str = "";

/// A session record's raid, as the controller currently has it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RaidMembership {
    pub id: String,
    pub name: String,
    pub role: Option<String>,
    /// Session ID of the owner.
    pub owner: String,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct RaidRecord {
    pub id: String,
    pub name: String,
    pub owner: String,
    pub members: Vec<RaidMemberRecord>,
}
/// No host paths, process identities, terminal endpoints or diagnostics:
/// the same record is served inside sandboxes.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct RaidMemberRecord {
    pub id: String,
    pub agent_name: String,
    /// Full tree path, usable as a `send` recipient by other members.
    pub path: String,
    /// Configuration key.
    pub name: String,
    pub role: Option<String>,
    pub state: String,
    pub owner: bool,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Invite {
    raid: String,
    sessions: Vec<String>,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Name {
    raid: String,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct HostRole {
    raid: String,
    session: String,
    role: String,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Children {
    sessions: Vec<String>,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Role {
    role: String,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Empty {}

/// A validated launch join, applied once the session is inserted.
pub(super) struct Join {
    name: String,
    create: bool,
    actor: String,
}

impl Controller {
    fn path_of(&self, session: &str) -> String {
        self.sessions
            .get(session)
            .map_or_else(|| session.to_owned(), |a| a.record.path.clone())
    }
    fn raid_fault(&self, error: raid::Error) -> Fault {
        match error {
            raid::Error::InvalidName => (
                -32602,
                "raid names must be 1..32 ASCII lowercase letters, digits or hyphens, starting with a letter".into(),
            ),
            raid::Error::InvalidRole => (
                -32602,
                "roles must be 1..32 ASCII lowercase letters, digits or hyphens, starting with a letter; a role can be changed but not cleared".into(),
            ),
            raid::Error::InvalidList => (
                -32602,
                format!("invite 1..{} goblins", raid::INVITE_LIMIT),
            ),
            raid::Error::Exists(name) => (-32009, format!("raid '{name}' already exists")),
            raid::Error::Absent(name) => (-32004, format!("raid '{name}' does not exist")),
            raid::Error::Member { session, raid } => (
                -32009,
                format!("goblin '{}' is already in raid '{raid}'", self.path_of(&session)),
            ),
            raid::Error::NotMember(session) => (
                -32009,
                format!("goblin '{}' is not in a raid", self.path_of(&session)),
            ),
        }
    }
    /// A new raid needs an ID; joining an existing one does not.
    fn raid_id(&mut self, name: &str) -> String {
        if self.raids.get(name).is_some() {
            String::new()
        } else {
            self.id("raid")
        }
    }
    /// Commit a mutation made on a copy. Additions are refused when the
    /// outbox or the daemon-wide event budget would overflow.
    fn raid_commit(&mut self, next: Raids, events: Vec<Event>) -> Result<(), Fault> {
        if events.iter().any(Event::addition)
            && (self.raid_events + events.len() > RAID_EVENTS
                || self.raid_outbox.len() + events.len() > RAID_OUTBOX)
        {
            return Err((-32010, "raid event capacity reached".into()));
        }
        self.raids = next;
        self.raid_queue(events);
        Ok(())
    }
    /// Removals always proceed, beyond the bounds if need be: there are at
    /// most 16 members.
    fn raid_queue(&mut self, events: Vec<Event>) {
        if events.is_empty() {
            return;
        }
        self.raid_events += events.len();
        self.raid_outbox.extend(events);
        self.sync_raid_records();
    }
    fn sync_raid_records(&mut self) {
        let memberships: Vec<_> = self
            .sessions
            .keys()
            .map(|id| {
                let raid = self.raids.of(id).map(|r| RaidMembership {
                    id: r.id.clone(),
                    name: r.name.clone(),
                    role: r.members[id].role.clone(),
                    owner: r.owner.clone(),
                });
                (id.clone(), raid)
            })
            .collect();
        for (id, raid) in memberships {
            let record = &mut self.sessions.get_mut(&id).unwrap().record;
            if record.raid != raid {
                record.raid = raid;
                self.dirty = true;
            }
        }
    }
    /// Remove every member that is absent or no longer starting/running;
    /// an owner leaving dissolves its raid. Idempotent, and it never stops
    /// anyone, so state transitions need no raid code of their own.
    pub(super) fn reconcile_raids(&mut self) {
        let gone: Vec<_> = self
            .raids
            .members()
            .filter_map(|id| match self.sessions.get(id) {
                Some(a)
                    if a.worker.is_some()
                        && matches!(a.record.state.as_str(), "starting" | "running") =>
                {
                    None
                }
                Some(a) if a.record.state == "failed" && a.record.identity.is_none() => {
                    Some((id.clone(), Reason::LaunchFailed))
                }
                _ => Some((id.clone(), Reason::Stopped)),
            })
            .collect();
        for (id, reason) in gone {
            let events = self.raids.remove(&id, reason);
            self.raid_queue(events);
        }
    }
    pub(super) fn raid_records(&self) -> Vec<RaidRecord> {
        self.raids.iter().map(|r| self.raid_record(r)).collect()
    }
    fn raid_record(&self, raid: &raid::Raid) -> RaidRecord {
        let mut members: Vec<_> = raid.members.iter().collect();
        members.sort_by_key(|(_, m)| m.joined);
        RaidRecord {
            id: raid.id.clone(),
            name: raid.name.clone(),
            owner: raid.owner.clone(),
            members: members
                .into_iter()
                .filter_map(|(id, member)| {
                    let record = &self.sessions.get(id)?.record;
                    Some(RaidMemberRecord {
                        id: id.clone(),
                        agent_name: record.agent_name.clone(),
                        path: record.path.clone(),
                        name: record.name.clone(),
                        role: member.role.clone(),
                        state: record.state.clone(),
                        owner: *id == raid.owner,
                    })
                })
                .collect(),
        }
    }
    fn raid_value(&self, name: &str) -> Result<Value, Fault> {
        let raid = self
            .raids
            .get(name)
            .ok_or_else(|| self.raid_fault(raid::Error::Absent(name.into())))?;
        Ok(json!(self.raid_record(raid)))
    }
    fn live(&self, session: &str) -> Result<(), Fault> {
        let a = &self.sessions[session];
        if a.worker.is_some() && matches!(a.record.state.as_str(), "starting" | "running") {
            Ok(())
        } else {
            Err((-32009, format!("goblin '{}' is not running", a.record.path)))
        }
    }
    fn bounded_list(sessions: &[String]) -> Result<(), Fault> {
        if sessions.is_empty() || sessions.len() > raid::INVITE_LIMIT {
            return Err((-32602, format!("invite 1..{} goblins", raid::INVITE_LIMIT)));
        }
        Ok(())
    }
    /// Validate a launch's join after the launch-key lookup and before name
    /// allocation, so a refused join allocates nothing. The host may create
    /// or join any raid; `inherit_raid` joins the selected parent's.
    pub(super) fn launch_raid(
        &self,
        p: &Start,
        parent: Option<&str>,
        scope: Option<&str>,
    ) -> Result<Option<Join>, Fault> {
        let join = match (&p.raid, p.inherit_raid) {
            (Some(name), _) => Join {
                name: name.clone(),
                create: true,
                actor: "host".into(),
            },
            (None, true) => Join {
                name: parent
                    .and_then(|id| self.raids.of(id))
                    .ok_or((-32009, "parent is not in a raid".to_string()))?
                    .name
                    .clone(),
                create: false,
                actor: scope.unwrap_or("host").into(),
            },
            (None, false) => return Ok(None),
        };
        let mut next = self.raids.clone();
        let events = next
            .join(
                String::new(),
                &join.name,
                LAUNCHING,
                join.create,
                &join.actor,
            )
            .map_err(|e| self.raid_fault(e))?;
        if self.raid_events + events.len() > RAID_EVENTS
            || self.raid_outbox.len() + events.len() > RAID_OUTBOX
        {
            return Err((-32010, "raid event capacity reached".into()));
        }
        Ok(Some(join))
    }
    /// Join in the same event-loop operation that inserted the session, so
    /// it is a member while starting.
    pub(super) fn apply_launch_raid(&mut self, session: &str, join: Join) -> Result<(), Fault> {
        let mut next = self.raids.clone();
        let id = self.raid_id(&join.name);
        let events = next
            .join(id, &join.name, session, join.create, &join.actor)
            .map_err(|e| self.raid_fault(e))?;
        self.raid_commit(next, events)
    }

    pub(super) fn host_raids(&mut self, method: &str, value: Value) -> Result<Value, Fault> {
        match method {
            "raids.invite" => {
                let p: Invite = params(value)?;
                if !raid::valid_name(&p.raid) {
                    return Err(self.raid_fault(raid::Error::InvalidName));
                }
                Self::bounded_list(&p.sessions)?;
                // Resolve and check every target before changing anything.
                let mut targets = vec![];
                for target in &p.sessions {
                    let id = self.resolve_session(target)?;
                    self.live(&id)?;
                    targets.push(id);
                }
                let mut next = self.raids.clone();
                let id = self.raid_id(&p.raid);
                let events = next
                    .invite(id, &p.raid, &targets, "host")
                    .map_err(|e| self.raid_fault(e))?;
                self.raid_commit(next, events)?;
                self.raid_value(&p.raid)
            }
            "raids.destroy" => {
                let p: Name = params(value)?;
                let events = self
                    .raids
                    .destroy(&p.raid, "host")
                    .map_err(|e| self.raid_fault(e))?;
                self.raid_queue(events);
                Ok(json!({"accepted":true}))
            }
            "raids.members" => {
                let p: Name = params(value)?;
                self.raid_value(&p.raid)
            }
            "raids.set_role" => {
                let p: HostRole = params(value)?;
                self.raid_value(&p.raid)?;
                let id = self.resolve_session(&p.session)?;
                if self.raids.of(&id).is_none_or(|r| r.name != p.raid) {
                    return Err((
                        -32009,
                        format!("goblin '{}' is not in raid '{}'", self.path_of(&id), p.raid),
                    ));
                }
                let mut next = self.raids.clone();
                let events = next
                    .set_role(&id, &p.role, "host")
                    .map_err(|e| self.raid_fault(e))?;
                self.raid_commit(next, events)?;
                self.raid_value(&p.raid)
            }
            _ => Err((-32601, "method unavailable on host endpoint".into())),
        }
    }

    /// The caller's identity selects its own raid. Invites reach only direct
    /// children; membership grants no control over other branches.
    pub(super) fn sandbox_raids(
        &mut self,
        scope: &str,
        method: &str,
        value: Value,
    ) -> Result<Value, Fault> {
        let current = self.raids.of(scope).map(|r| r.name.clone());
        let not_member = || self.raid_fault(raid::Error::NotMember(scope.into()));
        match method {
            "raids.create" => {
                let p: Name = params(value)?;
                let mut next = self.raids.clone();
                let id = self.raid_id(&p.raid);
                let events = next
                    .create(id, &p.raid, scope, scope)
                    .map_err(|e| self.raid_fault(e))?;
                self.raid_commit(next, events)?;
                self.raid_value(&p.raid)
            }
            "raids.invite" => {
                let p: Children = params(value)?;
                let name = current.ok_or_else(not_member)?;
                Self::bounded_list(&p.sessions)?;
                let mut targets = vec![];
                for target in &p.sessions {
                    let id = self.resolve_scoped(target, Some(scope))?;
                    if self.sessions[&id].record.parent.as_deref() != Some(scope) {
                        return Err((-32602, "only direct children can be invited".into()));
                    }
                    self.live(&id)?;
                    targets.push(id);
                }
                let mut next = self.raids.clone();
                let events = next
                    .invite(String::new(), &name, &targets, scope)
                    .map_err(|e| self.raid_fault(e))?;
                self.raid_commit(next, events)?;
                self.raid_value(&name)
            }
            "raids.leave" => {
                let _: Empty = params(value)?;
                current.ok_or_else(not_member)?;
                let events = self.raids.remove(scope, Reason::Left);
                let dissolved = events
                    .iter()
                    .any(|e| matches!(e.change, raid::Change::Dissolved { .. }));
                self.raid_queue(events);
                Ok(json!({"accepted":true,"dissolved":dissolved}))
            }
            "raids.members" => {
                let _: Empty = params(value)?;
                let name = current.ok_or_else(not_member)?;
                self.raid_value(&name)
            }
            "raids.set_role" => {
                let p: Role = params(value)?;
                let name = current.ok_or_else(not_member)?;
                let mut next = self.raids.clone();
                let events = next
                    .set_role(scope, &p.role, scope)
                    .map_err(|e| self.raid_fault(e))?;
                self.raid_commit(next, events)?;
                self.raid_value(&name)
            }
            _ => Err((-32601, "method unavailable on sandbox endpoint".into())),
        }
    }
}
