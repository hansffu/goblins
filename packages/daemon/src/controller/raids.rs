//! Raid wiring for the event loop: target resolution, who may act, launch
//! joins, reconciliation, prompts and the bounded event outbox. The rules
//! themselves live in `crate::raid`.
use super::Controller;
use super::Fault;
use super::Start;
use crate::raid;
use goblins_protocol::contract::{self,Method};
use crate::raid::Event;
use crate::raid::Raids;
use crate::raid::Reason;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

/// Events waiting for the messaging worker. Additions are refused when full.
pub(super) const RAID_OUTBOX: usize = 256;
/// Daemon-wide raid events; additions are refused once they would exceed it.
const RAID_EVENTS: usize = 4096;
/// Stands in for a launch's session ID while validating its join.
const LAUNCHING: &str = "";
/// The permission kind asking the host to resolve a paused raid.
pub(super) const RAID_OWNER: &str = "raid-owner";

/// A session record's raid, as the controller currently has it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(transparent)]
pub struct RaidMembership(pub contract::SessionRaid);
impl std::ops::Deref for RaidMembership {
    type Target = contract::SessionRaid;
    fn deref(&self) -> &Self::Target {
        &self.0
    }
}
impl std::ops::DerefMut for RaidMembership {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.0
    }
}
impl<'de> Deserialize<'de> for RaidMembership {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        contract::decode_session_raid(Value::deserialize(deserializer)?)
            .map(Self)
            .map_err(serde::de::Error::custom)
    }
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct RaidRecord {
    pub id: String,
    pub name: String,
    /// None while paused.
    pub owner: Option<String>,
    /// Session ID of the owner whose leaving paused the raid.
    #[serde(default)]
    pub former_owner: Option<String>,
    #[serde(default)]
    pub paused: bool,
    /// Changes on every pause and owner change; a host decision names it.
    #[serde(default)]
    pub generation: u64,
    pub members: Vec<RaidMemberRecord>,
}
/// No host paths, process identities, terminal endpoints or diagnostics:
/// the same record is served inside sandboxes.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct RaidMemberRecord {
    pub id: String,
    /// Unique among live goblins, usable as a `send` recipient.
    pub agent_name: String,
    /// Configuration key.
    pub name: String,
    pub role: Option<String>,
    pub state: String,
    pub owner: bool,
    /// Ran by an owner of this raid with `goblin run`, which may stop it.
    #[serde(default)]
    pub created_by: Option<String>,
    /// The launch options it was run with.
    #[serde(default)]
    pub options: std::collections::BTreeMap<String, String>,
}

impl From<&RaidMemberRecord> for contract::RaidMemberRecord {
    fn from(r: &RaidMemberRecord) -> Self {
        Self {
            id: r.id.clone(),
            agent_name: r.agent_name.clone(),
            name: r.name.clone(),
            role: r.role.clone(),
            state: r.state.clone(),
            owner: r.owner,
            created_by: r.created_by.clone(),
            options: r.options.clone(),
        }
    }
}
impl From<&RaidRecord> for contract::RaidRecord {
    fn from(r: &RaidRecord) -> Self {
        Self {
            id: r.id.clone(),
            name: r.name.clone(),
            owner: r.owner.clone(),
            former_owner: r.former_owner.clone(),
            paused: r.paused,
            generation: r.generation,
            members: r.members.iter().map(Into::into).collect(),
        }
    }
}
impl From<contract::RaidRecordReply> for RaidRecord {
    fn from(r: contract::RaidRecordReply) -> Self {
        Self {
            id: r.id,
            name: r.name,
            owner: r.owner,
            former_owner: r.former_owner,
            paused: r.paused,
            generation: r.generation,
            members: r
                .members
                .into_iter()
                .map(|member| RaidMemberRecord {
                    id: member.id,
                    agent_name: member.agent_name,
                    name: member.name,
                    role: member.role,
                    state: member.state,
                    owner: member.owner,
                    created_by: member.created_by,
                    options: member.options,
                })
                .collect(),
        }
    }
}
/// A validated launch join, applied once the session is inserted: a host
/// `--raid` join, or a member the owner runs.
pub(super) enum Join {
    Host(String),
    Run(String),
}

impl Controller {
    fn path_of(&self, session: &str) -> String {
        self.sessions
            .get(session)
            .map_or_else(|| session.to_owned(), |a| a.record.agent_name.clone())
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
            raid::Error::NotOwner(session) => (
                -32009,
                format!("goblin '{}' does not own its raid", self.path_of(&session)),
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
    pub(super) fn raid_commit(&mut self, next: Raids, events: Vec<Event>) -> Result<(), Fault> {
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
        self.raid_prompts(&events);
        self.raid_outbox.extend(events);
        self.sync_raid_records();
    }
    /// A pause asks the host to resolve the raid; a new owner or dissolution
    /// finishes that request. A request is never reused for a later pause.
    fn raid_prompts(&mut self, events: &[Event]) {
        for event in events {
            match &event.change {
                raid::Change::Paused { raid, reason, .. } => {
                    self.finish_raid_prompt(raid, "cancelled", "the raid paused again".into());
                    self.raid_prompt(raid, *reason);
                }
                raid::Change::Owner { raid, owner, .. } => {
                    let message = format!("{} is the new owner", self.path_of(owner));
                    self.finish_raid_prompt(raid, "approved", message);
                }
                raid::Change::Dissolved { raid, reason, .. } => match reason {
                    raid::Dissolution::Destroyed => self.finish_raid_prompt(
                        raid,
                        "denied",
                        "the host dissolved the raid".into(),
                    ),
                    raid::Dissolution::Empty => {
                        self.finish_raid_prompt(raid, "cancelled", "every member left".into())
                    }
                },
                _ => {}
            }
        }
    }
    fn raid_prompt(&mut self, id: &str, reason: Reason) {
        let Some(raid) = self.raids.iter().find(|r| r.id == id) else {
            return;
        };
        let Some(paused) = &raid.paused else {
            return;
        };
        let former = paused.former_owner.clone();
        let name = raid.name.clone();
        let preview = self.raid_prompt_preview(raid);
        let serial = self.number();
        let agent_name = self
            .sessions
            .get(&former)
            .map_or_else(|| former.clone(), |a| a.record.agent_name.clone());
        self.permissions.push_back(super::PermissionRecord {
            kind: RAID_OWNER.into(),
            id: format!("{}-r{serial}", self.instance),
            session: former,
            agent_name,
            approval: format!("{}-a{serial}", self.instance),
            package: name,
            reason: format!("the owner {}", reason.description()),
            state: "pending".into(),
            approved: None,
            preview: Some(preview),
            message: None,
            name: None,
            started_at: None,
            log: None,
        });
        self.dirty = true;
    }
    fn raid_prompt_preview(&self, raid: &raid::Raid) -> Value {
        let mut members: Vec<_> = raid.members.iter().collect();
        members.sort_by_key(|(_, m)| m.joined);
        let former = raid.paused.as_ref().map(|p| p.former_owner.as_str());
        json!(contract::RaidApprovalPreview {
            raid: raid.id.clone(),
            name: raid.name.clone(),
            generation: raid.generation,
            former_owner: former.map(|id| contract::CreatedBy {
                id: id.into(),
                agent_name: self.path_of(id)
            }),
            members: members
                .into_iter()
                .filter_map(|(id, m)| {
                    let record = &self.sessions.get(id)?.record;
                    Some(contract::RaidApprovalMember {
                        id: id.clone(),
                        agent_name: record.agent_name.clone(),
                        name: record.name.clone(),
                        role: m.role.clone(),
                        state: record.state.clone(),
                        created: m.created_by.is_some(),
                    })
                })
                .collect(),
        })
    }
    fn finish_raid_prompt(&mut self, raid: &str, state: &str, message: String) {
        for r in self.permissions.iter_mut().filter(|r| {
            r.kind == RAID_OWNER
                && r.state == "pending"
                && r.preview.as_ref().is_some_and(|p| p["raid"] == raid)
        }) {
            r.state = state.into();
            r.approved = match state {
                "approved" => Some(true),
                "denied" => Some(false),
                _ => None,
            };
            r.message = Some(message.clone());
            self.dirty = true;
        }
    }
    /// Keep pending raid-owner previews current as members join, leave and
    /// change state.
    pub(super) fn refresh_raid_prompts(&mut self) {
        let previews: Vec<_> = self
            .permissions
            .iter()
            .enumerate()
            .filter(|(_, r)| r.kind == RAID_OWNER && r.state == "pending")
            .filter_map(|(index, r)| {
                let id = r.preview.as_ref()?["raid"].as_str()?;
                let raid = self.raids.iter().find(|raid| raid.id == id)?;
                Some((index, self.raid_prompt_preview(raid)))
            })
            .collect();
        for (index, preview) in previews {
            let r = &mut self.permissions[index];
            if r.preview.as_ref() != Some(&preview) {
                r.preview = Some(preview);
                self.dirty = true;
            }
        }
    }
    /// A host decision taken from a prompt must still name the raid it saw.
    fn same_raid(
        &self,
        name: &str,
        id: Option<&str>,
        generation: Option<u64>,
    ) -> Result<(), Fault> {
        let raid = self
            .raids
            .get(name)
            .ok_or_else(|| self.raid_fault(raid::Error::Absent(name.into())))?;
        if id.is_some_and(|id| id != raid.id) || generation.is_some_and(|g| g != raid.generation) {
            return Err((-32009, "the raid changed; reload the prompt".into()));
        }
        Ok(())
    }
    fn sync_raid_records(&mut self) {
        let memberships: Vec<_> = self
            .sessions
            .keys()
            .map(|id| {
                let raid = self.raids.of(id).map(|r| {
                    RaidMembership(contract::SessionRaid {
                        id: r.id.clone(),
                        name: r.name.clone(),
                        role: r.members[id].role.clone(),
                        owner: r.owner.clone(),
                        paused: r.paused.is_some(),
                    })
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
    /// an owner leaving pauses its raid. Idempotent, and it never stops
    /// anyone, so state transitions need no raid code of their own.
    pub(super) fn reconcile_raids(&mut self) {
        let gone: Vec<_> = self
            .raids
            .members()
            .filter_map(|id| match self.sessions.get(id) {
                Some(a)
                    if a.live() && matches!(a.record.state.as_str(), "starting" | "running") =>
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
            former_owner: raid.paused.as_ref().map(|p| p.former_owner.clone()),
            paused: raid.paused.is_some(),
            generation: raid.generation,
            members: members
                .into_iter()
                .filter_map(|(id, member)| {
                    let record = &self.sessions.get(id)?.record;
                    Some(RaidMemberRecord {
                        id: id.clone(),
                        agent_name: record.agent_name.clone(),
                        name: record.name.clone(),
                        role: member.role.clone(),
                        state: record.state.clone(),
                        owner: raid.owner.as_ref() == Some(id),
                        created_by: member.created_by.clone(),
                        options: record.options.clone(),
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
        Ok(json!(contract::RaidRecord::from(&self.raid_record(raid))))
    }
    fn live(&self, session: &str) -> Result<(), Fault> {
        let a = &self.sessions[session];
        if a.live() && matches!(a.record.state.as_str(), "starting" | "running") {
            Ok(())
        } else {
            Err((
                -32009,
                format!("goblin '{}' is not running", a.record.agent_name),
            ))
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
    /// or join any raid; a raid owner's run joins the raid it owns.
    pub(super) fn launch_raid(
        &self,
        p: &Start,
        owner: Option<&str>,
    ) -> Result<Option<Join>, Fault> {
        let mut next = self.raids.clone();
        let (join, events) = match (owner, &p.raid) {
            (Some(owner), _) => (
                Join::Run(owner.into()),
                next.run(owner, LAUNCHING).map_err(|e| self.raid_fault(e))?,
            ),
            (None, Some(name)) => (
                Join::Host(name.clone()),
                next.join(String::new(), name, LAUNCHING, "host")
                    .map_err(|e| self.raid_fault(e))?,
            ),
            (None, None) => return Ok(None),
        };
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
        let events = match join {
            Join::Host(name) => {
                let id = self.raid_id(&name);
                next.join(id, &name, session, "host")
            }
            Join::Run(owner) => next.run(&owner, session),
        }
        .map_err(|e| self.raid_fault(e))?;
        self.raid_commit(next, events)
    }

    pub(super) fn host_raids(&mut self, method: &str, value: Value) -> Result<Value, Fault> {
        match method {
            contract::host::RaidsInvite::NAME => {
                let p = super::contract_params::<contract::host::RaidsInvite>(value)?;
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
            contract::host::RaidsDestroy::NAME => {
                let p = super::contract_params::<contract::host::RaidsDestroy>(value)?;
                self.same_raid(&p.raid, p.id.as_deref(), p.generation)?;
                let members: Vec<_> = self
                    .raids
                    .get(&p.raid)
                    .map_or_else(Vec::new, |r| r.members.keys().cloned().collect());
                let events = self
                    .raids
                    .destroy(&p.raid, "host")
                    .map_err(|e| self.raid_fault(e))?;
                self.raid_queue(events);
                let mut stopped = vec![];
                if p.stop_members {
                    let reason = format!("stopped because raid {} was closed by the host", p.raid);
                    for id in members {
                        if self.live(&id).is_ok() {
                            self.stop(&id, Some(reason.clone()))?;
                            stopped.push(id);
                        }
                    }
                }
                Ok(json!(contract::RaidDestroyResult {
                    accepted: true,
                    stopped
                }))
            }
            contract::host::RaidsSetOwner::NAME => {
                let p = super::contract_params::<contract::host::RaidsSetOwner>(value)?;
                self.same_raid(&p.raid, p.id.as_deref(), p.generation)?;
                let id = self.resolve_session(&p.session)?;
                if self.raids.of(&id).is_none_or(|r| r.name != p.raid) {
                    return Err((
                        -32009,
                        format!("goblin '{}' is not in raid '{}'", self.path_of(&id), p.raid),
                    ));
                }
                self.live(&id)?;
                let mut next = self.raids.clone();
                let events = next
                    .set_owner(&p.raid, &id, "host")
                    .map_err(|e| self.raid_fault(e))?;
                self.raid_commit(next, events)?;
                self.raid_value(&p.raid)
            }
            contract::host::RaidsMembers::NAME => {
                let p = super::contract_params::<contract::host::RaidsMembers>(value)?;
                self.raid_value(&p.raid)
            }
            contract::host::RaidsKick::NAME => {
                let p = super::contract_params::<contract::host::RaidsKick>(value)?;
                self.raid_value(&p.raid)?;
                let id = self.resolve_session(&p.session)?;
                if self.raids.of(&id).is_none_or(|r| r.name != p.raid) {
                    return Err((
                        -32009,
                        format!("goblin '{}' is not in raid '{}'", self.path_of(&id), p.raid),
                    ));
                }
                let mut next = self.raids.clone();
                let events = next.kick(&id, "host").map_err(|e| self.raid_fault(e))?;
                let (dissolved, paused) = outcome(&events);
                self.raid_commit(next, events)?;
                Ok(json!(contract::RaidChangeResult {
                    accepted: true,
                    dissolved,
                    paused
                }))
            }
            contract::host::RaidsSetRole::NAME => {
                let p = super::contract_params::<contract::host::RaidsSetRole>(value)?;
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

    /// The caller's identity selects its own raid. Membership grants no
    /// control over other goblins; owners run and stop members elsewhere.
    pub(super) fn sandbox_raids(
        &mut self,
        scope: &str,
        method: &str,
        value: Value,
    ) -> Result<Value, Fault> {
        let current = self.raids.of(scope).map(|r| r.name.clone());
        let not_member = || self.raid_fault(raid::Error::NotMember(scope.into()));
        match method {
            contract::sandbox::RaidsCreate::NAME => {
                let p = contract::decode_params::<contract::sandbox::RaidsCreate>(value)
                    .map_err(|e| (-32602, e.to_string()))?;
                let mut next = self.raids.clone();
                let id = self.raid_id(&p.raid);
                let events = next
                    .create(id, &p.raid, scope, scope)
                    .map_err(|e| self.raid_fault(e))?;
                self.raid_commit(next, events)?;
                self.raid_value(&p.raid)
            }
            contract::sandbox::RaidsLeave::NAME => {
                let _ = contract::decode_params::<contract::sandbox::RaidsLeave>(value)
                    .map_err(|e| (-32602, e.to_string()))?;
                let name = current.ok_or_else(not_member)?;
                let raid = self.raids.get(&name).expect("current raid");
                if raid.owner.as_deref() != Some(scope) && raid.members[scope].created_by.is_some()
                {
                    return Err((
                        -32009,
                        "goblins created in a raid stay until stopped, kicked by the host or the raid dissolves".into(),
                    ));
                }
                let events = self.raids.remove(scope, Reason::Left);
                let (dissolved, paused) = outcome(&events);
                self.raid_queue(events);
                serde_json::to_value(contract::RaidChangeResult {
                    accepted: true,
                    dissolved,
                    paused,
                })
                .map_err(|e| (-32603, e.to_string()))
            }
            contract::sandbox::RaidsMembers::NAME => {
                let _ = contract::decode_params::<contract::sandbox::RaidsMembers>(value)
                    .map_err(|e| (-32602, e.to_string()))?;
                let name = current.ok_or_else(not_member)?;
                self.raid_value(&name)
            }
            contract::sandbox::RaidsSetRole::NAME => {
                let p = contract::decode_params::<contract::sandbox::RaidsSetRole>(value)
                    .map_err(|e| (-32602, e.to_string()))?;
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

/// Whether removing a member dissolved or paused its raid.
fn outcome(events: &[Event]) -> (bool, bool) {
    let any = |f: fn(&raid::Change) -> bool| events.iter().any(|e| f(&e.change));
    (
        any(|c| matches!(c, raid::Change::Dissolved { .. })),
        any(|c| matches!(c, raid::Change::Paused { .. })),
    )
}

#[cfg(test)]
mod preview_tests {
    use super::super::tests::{daemon, live};
    use super::*;
    #[test]
    fn sandbox_raid_handlers_keep_domain_error_order_and_public_results() {
        let _no_forks = goblins_unix::no_forks();
        let mut d = daemon();
        let path = d.state.clone();
        let _events = live(&mut d, "a");
        // A structurally valid but invalid role still looks up caller membership first.
        assert_eq!(
            d.sandbox_raids("a", "raids.set_role", json!({"role":"Bad Role"}))
                .unwrap_err()
                .0,
            -32009
        );
        let created = d
            .sandbox_raids("a", "raids.create", json!(["crew"]))
            .unwrap();
        let old = serde_json::to_value(original_record(&d, d.raids.get("crew").unwrap())).unwrap();
        assert_eq!(created, old);
        let before = d.sessions["a"].record.raid.clone();
        assert_eq!(
            d.sandbox_raids("a", "raids.set_role", json!(["Bad Role"]))
                .unwrap_err()
                .0,
            -32602
        );
        assert_eq!(d.sessions["a"].record.raid, before);
        let role = d
            .sandbox_raids("a", "raids.set_role", json!(["reviewer"]))
            .unwrap();
        assert_eq!(role["owner"], "a");
        assert_eq!(role["members"][0]["role"], "reviewer");
        assert_eq!(
            d.sandbox_raids("a", "raids.members", json!([])).unwrap(),
            role
        );
        assert_eq!(
            d.sandbox_raids("a", "raids.leave", json!([])).unwrap(),
            json!({"accepted":true,"dissolved":true,"paused":false})
        );
        assert!(d.sessions["a"].record.raid.is_none());
        drop(d);
        std::fs::remove_dir_all(path).unwrap();
    }
    #[test]
    fn raid_membership_wrapper_preserves_original_wire_forms() {
        let fixtures: Value =
            serde_json::from_str(include_str!("../../../../libs/api/schema/fixtures.json"))
                .unwrap();
        for case in fixtures["session_raid_cases"].as_array().unwrap() {
            let result = serde_json::from_value::<RaidMembership>(case["result"].clone());
            assert_eq!(result.is_ok(), case["legacy"].as_bool().unwrap(), "{case}");
            if let Ok(result) = result {
                assert_eq!(
                    serde_json::to_value(&result).unwrap(),
                    serde_json::to_value(&result.0).unwrap()
                );
            }
        }
    }
    fn original_preview(controller: &Controller, raid: &raid::Raid) -> Value {
        let mut members: Vec<_> = raid.members.iter().collect();
        members.sort_by_key(|(_, m)| m.joined);
        let former = raid.paused.as_ref().map(|p| p.former_owner.as_str());
        json!({"raid":raid.id,"name":raid.name,"generation":raid.generation,
            "former_owner":former.map(|id| json!({"id":id,"agent_name":controller.path_of(id)})),
            "members":members.into_iter().filter_map(|(id, m)| {
                let record = &controller.sessions.get(id)?.record;
                Some(json!({"id":id,"agent_name":record.agent_name,
                    "name":record.name,"role":m.role,"state":record.state,
                    "created":m.created_by.is_some()}))
            }).collect::<Vec<_>>()})
    }

    fn original_record(controller: &Controller, raid: &raid::Raid) -> RaidRecord {
        let mut members: Vec<_> = raid.members.iter().collect();
        members.sort_by_key(|(_, m)| m.joined);
        RaidRecord {
            id: raid.id.clone(),
            name: raid.name.clone(),
            owner: raid.owner.clone(),
            former_owner: raid.paused.as_ref().map(|p| p.former_owner.clone()),
            paused: raid.paused.is_some(),
            generation: raid.generation,
            members: members
                .into_iter()
                .filter_map(|(id, member)| {
                    let record = &controller.sessions.get(id)?.record;
                    Some(RaidMemberRecord {
                        id: id.clone(),
                        agent_name: record.agent_name.clone(),
                        name: record.name.clone(),
                        role: member.role.clone(),
                        state: record.state.clone(),
                        owner: raid.owner.as_ref() == Some(id),
                        created_by: member.created_by.clone(),
                        options: record.options.clone(),
                    })
                })
                .collect(),
        }
    }
    #[test]
    fn raid_record_producer_matches_original_fields_order_nulls_and_private_exclusion() {
        let _no_forks = goblins_unix::no_forks();
        let mut controller = daemon();
        let path = controller.state.clone();
        let _a = live(&mut controller, "a");
        let _z = live(&mut controller, "z");
        let active = controller.sessions.get_mut("a").unwrap();
        active.record.agent_name = "alpha".into();
        active.record.configuration = "/host/private/config".into();
        active.record.terminal = "/host/private/terminal".into();
        active.record.detail = Some("/host/private/diagnostic".into());
        active.record.options = std::collections::BTreeMap::from([("model".into(), "fast".into())]);
        let mut raid = raid::Raid {
            id: "raid-fixed".into(),
            name: "crew".into(),
            owner: Some("a".into()),
            paused: None,
            generation: 0,
            members: std::collections::BTreeMap::from([
                (
                    "a".into(),
                    raid::Member {
                        role: None,
                        joined: 3,
                        created_by: None,
                    },
                ),
                (
                    "gone".into(),
                    raid::Member {
                        role: None,
                        joined: 2,
                        created_by: None,
                    },
                ),
                (
                    "z".into(),
                    raid::Member {
                        role: Some("reviewer".into()),
                        joined: 1,
                        created_by: Some("a".into()),
                    },
                ),
            ]),
        };
        for paused in [false, true] {
            raid.owner = if paused { None } else { Some("a".into()) };
            raid.paused = paused.then(|| raid::Paused {
                former_owner: "a".into(),
                reason: Reason::Left,
            });
            raid.generation = u64::from(paused);
            let produced = json!(contract::RaidRecord::from(&controller.raid_record(&raid)));
            assert_eq!(produced, json!(original_record(&controller, &raid)));
            assert_eq!(
                produced["members"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .map(|m| m["id"].as_str().unwrap())
                    .collect::<Vec<_>>(),
                ["z", "a"]
            );
            assert!(produced["members"][1]["role"].is_null());
            assert!(produced["members"][1]["created_by"].is_null());
            assert_eq!(produced["members"][1]["options"], json!({"model":"fast"}));
            assert_eq!(
                produced["owner"],
                if paused { Value::Null } else { json!("a") }
            );
            assert_eq!(
                produced["former_owner"],
                if paused { json!("a") } else { Value::Null }
            );
            assert!(
                !serde_json::to_string(&produced)
                    .unwrap()
                    .contains("/host/private/")
            );
            for member in produced["members"].as_array().unwrap() {
                assert_eq!(member.as_object().unwrap().len(), 8);
                for key in ["configuration", "terminal", "identity", "detail"] {
                    assert!(member.get(key).is_none());
                }
            }
        }
        drop(controller);
        std::fs::remove_dir_all(path).unwrap();
    }

    #[test]
    fn raid_approval_preview_producer_matches_original_wire_and_nullable_fields() {
        let _no_forks = goblins_unix::no_forks();
        let mut controller = daemon();
        let path = controller.state.clone();
        let _a = live(&mut controller, "a");
        let _z = live(&mut controller, "z");
        controller.sessions.get_mut("a").unwrap().record.agent_name = "alpha".into();
        let mut raid = raid::Raid {
            id: "raid-fixed".into(),
            name: "crew".into(),
            owner: None,
            generation: 2,
            paused: None,
            members: std::collections::BTreeMap::from([
                (
                    "a".into(),
                    raid::Member {
                        role: None,
                        joined: 3,
                        created_by: None,
                    },
                ),
                (
                    "gone".into(),
                    raid::Member {
                        role: None,
                        joined: 2,
                        created_by: None,
                    },
                ),
                (
                    "z".into(),
                    raid::Member {
                        role: Some("reviewer".into()),
                        joined: 1,
                        created_by: Some("a".into()),
                    },
                ),
            ]),
        };
        let members = json!([
            {"id":"z","agent_name":"z","name":"shell","role":"reviewer","state":"running","created":true},
            {"id":"a","agent_name":"alpha","name":"shell","role":null,"state":"running","created":false}
        ]);
        for former in [None, Some("a"), Some("missing")] {
            raid.paused = former.map(|id| raid::Paused {
                former_owner: id.into(),
                reason: Reason::Left,
            });
            let produced = controller.raid_prompt_preview(&raid);
            assert_eq!(produced, original_preview(&controller, &raid));
            let former =
                former.map(|id| json!({"id":id,"agent_name":if id=="a" {"alpha"} else {id}}));
            assert_eq!(
                produced,
                json!({"raid":"raid-fixed","name":"crew","generation":2,"former_owner":former,"members":members})
            );
        }
        drop(controller);
        std::fs::remove_dir_all(path).unwrap();
    }
}
