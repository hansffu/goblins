//! Raid membership rules. Pure state: the controller resolves targets,
//! enforces ownership-tree restrictions and forwards the returned events.
//!
//! A raid only helps members find and message each other. It grants no
//! files, terminals or lifecycle control, and dissolving one stops nobody.
use serde_json::{Value, json};
use std::collections::BTreeMap;

/// Invite lists are bounded like live sessions; a session joins one raid.
pub const INVITE_LIMIT: usize = 16;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Raid {
    /// "{instance}-raid{n}"; a reused name gets a new ID.
    pub id: String,
    pub name: String,
    /// Session ID. Only the first joiner owns the raid; roles never move it.
    pub owner: String,
    pub members: BTreeMap<String, Member>,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Member {
    /// None only until a role is first set; a role is never cleared.
    pub role: Option<String>,
    /// Join order, for listings.
    pub joined: u64,
}
#[derive(Clone, Debug, Default)]
pub struct Raids {
    raids: BTreeMap<String, Raid>,
    member_of: BTreeMap<String, String>,
    joined: u64,
}

/// Why a member left, as recorded in `raid.left`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Reason {
    Left,
    Stopped,
    LaunchFailed,
}
impl Reason {
    fn as_str(self) -> &'static str {
        match self {
            Self::Left => "left",
            Self::Stopped => "stopped",
            Self::LaunchFailed => "launch-failed",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Change {
    Created {
        raid: String,
        name: String,
        owner: String,
    },
    Joined {
        raid: String,
        session: String,
        role: Option<String>,
    },
    Role {
        raid: String,
        session: String,
        role: String,
    },
    Left {
        raid: String,
        session: String,
        reason: Reason,
    },
    Dissolved {
        raid: String,
        /// Everyone who was a member, the owner included.
        members: Vec<String>,
        destroyed: bool,
    },
}
/// One membership change, attributed to the session (or `host`) that caused it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Event {
    pub actor: String,
    pub change: Change,
}
impl Event {
    pub fn kind(&self) -> &'static str {
        match self.change {
            Change::Created { .. } => "raid.created",
            Change::Joined { .. } => "raid.joined",
            Change::Role { .. } => "raid.role",
            Change::Left { .. } => "raid.left",
            Change::Dissolved { .. } => "raid.dissolved",
        }
    }
    /// Additions can authorize messages; removals only take access away.
    pub fn addition(&self) -> bool {
        !matches!(self.change, Change::Left { .. } | Change::Dissolved { .. })
    }
    /// The sessions a filtered communications log should show this event to.
    pub fn sessions(&self) -> Vec<String> {
        match &self.change {
            Change::Created { owner, .. } => vec![owner.clone()],
            Change::Joined { session, .. }
            | Change::Role { session, .. }
            | Change::Left { session, .. } => vec![session.clone()],
            Change::Dissolved { members, .. } => members.clone(),
        }
    }
    pub fn data(&self) -> Value {
        match &self.change {
            Change::Created { raid, name, owner } => {
                json!({"raid":raid,"name":name,"owner":owner})
            }
            Change::Joined {
                raid,
                session,
                role,
            } => json!({"raid":raid,"session":session,"role":role}),
            Change::Role {
                raid,
                session,
                role,
            } => json!({"raid":raid,"session":session,"role":role}),
            Change::Left {
                raid,
                session,
                reason,
            } => json!({"raid":raid,"session":session,"reason":reason.as_str()}),
            Change::Dissolved {
                raid, destroyed, ..
            } => {
                json!({"raid":raid,"reason":if *destroyed {"destroyed"} else {"owner-left"}})
            }
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Error {
    InvalidName,
    InvalidRole,
    InvalidList,
    /// The raid name is taken.
    Exists(String),
    /// No raid has this name.
    Absent(String),
    /// The session is in the named raid, and cannot create or join another.
    Member {
        session: String,
        raid: String,
    },
    /// The session is in no raid, or not in the one named.
    NotMember(String),
}
pub type Result<T> = std::result::Result<T, Error>;

/// Raid names and roles use the agent-name grammar.
pub fn valid_name(name: &str) -> bool {
    crate::controller::valid_agent_name(name)
}

impl Raids {
    pub fn get(&self, name: &str) -> Option<&Raid> {
        self.raids.get(name)
    }
    pub fn of(&self, session: &str) -> Option<&Raid> {
        self.member_of
            .get(session)
            .and_then(|name| self.raids.get(name))
    }
    pub fn iter(&self) -> impl Iterator<Item = &Raid> {
        self.raids.values()
    }
    pub fn members(&self) -> impl Iterator<Item = &String> {
        self.member_of.keys()
    }

    pub fn create(
        &mut self,
        id: String,
        name: &str,
        owner: &str,
        actor: &str,
    ) -> Result<Vec<Event>> {
        if !valid_name(name) {
            return Err(Error::InvalidName);
        }
        if self.raids.contains_key(name) {
            return Err(Error::Exists(name.into()));
        }
        if let Some(raid) = self.member_of.get(owner) {
            return Err(Error::Member {
                session: owner.into(),
                raid: raid.clone(),
            });
        }
        self.joined += 1;
        self.raids.insert(
            name.into(),
            Raid {
                id: id.clone(),
                name: name.into(),
                owner: owner.into(),
                members: BTreeMap::from([(
                    owner.into(),
                    Member {
                        role: None,
                        joined: self.joined,
                    },
                )]),
            },
        );
        self.member_of.insert(owner.into(), name.into());
        Ok(vec![Event {
            actor: actor.into(),
            change: Change::Created {
                raid: id,
                name: name.into(),
                owner: owner.into(),
            },
        }])
    }

    /// Validates every target before changing anything, so a failed invite
    /// creates no raid and picks no owner. Current members are a no-op; a
    /// member of another raid fails rather than moving. An absent raid is
    /// created with the first target as owner; an existing owner is kept.
    pub fn invite(
        &mut self,
        id: String,
        name: &str,
        sessions: &[String],
        actor: &str,
    ) -> Result<Vec<Event>> {
        if !valid_name(name) {
            return Err(Error::InvalidName);
        }
        if sessions.is_empty() || sessions.len() > INVITE_LIMIT {
            return Err(Error::InvalidList);
        }
        for session in sessions {
            if let Some(raid) = self.member_of.get(session).filter(|r| *r != name) {
                return Err(Error::Member {
                    session: session.clone(),
                    raid: raid.clone(),
                });
            }
        }
        let mut events = vec![];
        if !self.raids.contains_key(name) {
            events = self.create(id, name, &sessions[0], actor)?;
        }
        for session in sessions {
            if self.member_of.contains_key(session) {
                continue;
            }
            self.joined += 1;
            let raid = self.raids.get_mut(name).expect("raid exists");
            raid.members.insert(
                session.clone(),
                Member {
                    role: None,
                    joined: self.joined,
                },
            );
            self.member_of.insert(session.clone(), name.into());
            events.push(Event {
                actor: actor.into(),
                change: Change::Joined {
                    raid: raid.id.clone(),
                    session: session.clone(),
                    role: None,
                },
            });
        }
        Ok(events)
    }

    /// A launch join. `--inherit-raid` never creates: the raid must exist.
    pub fn join(
        &mut self,
        id: String,
        name: &str,
        session: &str,
        create: bool,
        actor: &str,
    ) -> Result<Vec<Event>> {
        if !create && !self.raids.contains_key(name) {
            return Err(Error::Absent(name.into()));
        }
        self.invite(id, name, &[session.into()], actor)
    }

    /// Leave, exit, stop or failed launch. Removing the owner dissolves the
    /// raid and clears every member; it never stops anyone.
    pub fn remove(&mut self, session: &str, reason: Reason) -> Vec<Event> {
        let Some(name) = self.member_of.remove(session) else {
            return vec![];
        };
        let raid = self.raids.get_mut(&name).expect("member of a live raid");
        raid.members.remove(session);
        let mut events = vec![Event {
            actor: session.into(),
            change: Change::Left {
                raid: raid.id.clone(),
                session: session.into(),
                reason,
            },
        }];
        if raid.owner == session {
            events.extend(self.dissolve(&name, session, false));
        }
        events
    }

    pub fn destroy(&mut self, name: &str, actor: &str) -> Result<Vec<Event>> {
        if !self.raids.contains_key(name) {
            return Err(Error::Absent(name.into()));
        }
        Ok(self.dissolve(name, actor, true))
    }

    fn dissolve(&mut self, name: &str, actor: &str, destroyed: bool) -> Vec<Event> {
        let raid = self.raids.remove(name).expect("raid exists");
        let mut members: Vec<_> = raid.members.keys().cloned().collect();
        for member in &members {
            self.member_of.remove(member);
        }
        if !members.contains(&raid.owner) {
            members.insert(0, raid.owner.clone());
        }
        vec![Event {
            actor: actor.into(),
            change: Change::Dissolved {
                raid: raid.id,
                members,
                destroyed,
            },
        }]
    }

    /// Roles describe work only. Setting the current role again is a no-op.
    pub fn set_role(&mut self, session: &str, role: &str, actor: &str) -> Result<Vec<Event>> {
        if !valid_name(role) {
            return Err(Error::InvalidRole);
        }
        let name = self
            .member_of
            .get(session)
            .ok_or_else(|| Error::NotMember(session.into()))?;
        let raid = self.raids.get_mut(name).expect("member of a live raid");
        let member = raid.members.get_mut(session).expect("listed member");
        if member.role.as_deref() == Some(role) {
            return Ok(vec![]);
        }
        member.role = Some(role.into());
        Ok(vec![Event {
            actor: actor.into(),
            change: Change::Role {
                raid: raid.id.clone(),
                session: session.into(),
                role: role.into(),
            },
        }])
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn ids(values: &[&str]) -> Vec<String> {
        values.iter().map(|v| v.to_string()).collect()
    }
    fn kinds(events: &[Event]) -> Vec<&'static str> {
        events.iter().map(Event::kind).collect()
    }

    #[test]
    fn create_join_and_leave() {
        let mut r = Raids::default();
        let events = r.create("r1".into(), "review", "a", "a").unwrap();
        assert_eq!(kinds(&events), ["raid.created"]);
        assert_eq!(events[0].sessions(), ["a"]);
        assert_eq!(
            events[0].data(),
            json!({"raid":"r1","name":"review","owner":"a"})
        );
        let events = r.join("r2".into(), "review", "b", false, "a").unwrap();
        assert_eq!(kinds(&events), ["raid.joined"]);
        assert_eq!(
            (events[0].actor.as_str(), events[0].sessions()),
            ("a", ids(&["b"]))
        );
        assert_eq!(r.of("b").unwrap().id, "r1");
        assert_eq!(r.of("b").unwrap().owner, "a");
        let events = r.remove("b", Reason::Left);
        assert_eq!(kinds(&events), ["raid.left"]);
        assert_eq!(events[0].data()["reason"], "left");
        assert!(r.of("b").is_none());
        assert_eq!(r.get("review").unwrap().members.len(), 1);
        assert!(r.remove("b", Reason::Left).is_empty());
        // Launch joins never create for --inherit-raid.
        assert_eq!(
            r.join("r3".into(), "absent", "c", false, "a").unwrap_err(),
            Error::Absent("absent".into())
        );
        assert!(r.get("absent").is_none());
        assert_eq!(
            r.create("r4".into(), "review", "c", "c").unwrap_err(),
            Error::Exists("review".into())
        );
        assert_eq!(
            r.create("r5".into(), "other", "a", "a").unwrap_err(),
            Error::Member {
                session: "a".into(),
                raid: "review".into()
            }
        );
        for name in ["", "Review", "two words", &"x".repeat(33)] {
            assert_eq!(
                r.create("r6".into(), name, "c", "c").unwrap_err(),
                Error::InvalidName
            );
        }
    }

    #[test]
    fn owner_removal_dissolves_and_clears_every_member() {
        let mut r = Raids::default();
        r.invite("r1".into(), "review", &ids(&["a", "b", "c"]), "host")
            .unwrap();
        let events = r.remove("a", Reason::Stopped);
        assert_eq!(kinds(&events), ["raid.left", "raid.dissolved"]);
        assert_eq!(events[0].data()["reason"], "stopped");
        assert_eq!(events[1].actor, "a");
        assert_eq!(events[1].data(), json!({"raid":"r1","reason":"owner-left"}));
        assert_eq!(events[1].sessions(), ids(&["a", "b", "c"]));
        assert!(r.get("review").is_none());
        assert!(r.members().next().is_none());
    }

    #[test]
    fn invites_are_idempotent_validated_first_and_keep_the_owner() {
        let mut r = Raids::default();
        let events = r
            .invite("r1".into(), "review", &ids(&["a", "b"]), "host")
            .unwrap();
        assert_eq!(kinds(&events), ["raid.created", "raid.joined"]);
        assert!(events.iter().all(|e| e.actor == "host"));
        assert_eq!(r.get("review").unwrap().owner, "a");
        // Current members are a no-op, and inviting keeps the owner.
        let events = r
            .invite("r2".into(), "review", &ids(&["b", "a", "c"]), "host")
            .unwrap();
        assert_eq!(kinds(&events), ["raid.joined"]);
        assert_eq!(events[0].sessions(), ["c"]);
        assert_eq!(r.get("review").unwrap().owner, "a");
        assert_eq!(r.get("review").unwrap().id, "r1");
        // A member of another raid fails the whole invite: no raid, no owner.
        r.create("r3".into(), "build", "x", "x").unwrap();
        let before = r.clone();
        assert_eq!(
            r.invite("r4".into(), "fresh", &ids(&["d", "x"]), "host")
                .unwrap_err(),
            Error::Member {
                session: "x".into(),
                raid: "build".into()
            }
        );
        assert!(r.get("fresh").is_none());
        assert!(r.of("d").is_none());
        assert_eq!(
            r.invite("r4".into(), "review", &ids(&["x"]), "a")
                .unwrap_err(),
            Error::Member {
                session: "x".into(),
                raid: "build".into()
            }
        );
        assert_eq!(r.of("x").unwrap().name, "build");
        assert_eq!(
            r.invite("r4".into(), "fresh", &[], "host").unwrap_err(),
            Error::InvalidList
        );
        let many: Vec<_> = (0..=INVITE_LIMIT).map(|i| format!("s{i}")).collect();
        assert_eq!(
            r.invite("r4".into(), "fresh", &many, "host").unwrap_err(),
            Error::InvalidList
        );
        assert_eq!(r.raids, before.raids);
        assert_eq!(r.member_of, before.member_of);
    }

    #[test]
    fn a_reused_name_gets_a_new_id_without_former_members() {
        let mut r = Raids::default();
        r.invite("r1".into(), "review", &ids(&["a", "b"]), "host")
            .unwrap();
        let events = r.destroy("review", "host").unwrap();
        assert_eq!(kinds(&events), ["raid.dissolved"]);
        assert_eq!(events[0].actor, "host");
        assert_eq!(events[0].data()["reason"], "destroyed");
        assert_eq!(events[0].sessions(), ids(&["a", "b"]));
        assert_eq!(
            r.destroy("review", "host").unwrap_err(),
            Error::Absent("review".into())
        );
        r.create("r2".into(), "review", "c", "c").unwrap();
        let raid = r.get("review").unwrap();
        assert_eq!(raid.id, "r2");
        assert_eq!(raid.members.keys().collect::<Vec<_>>(), ["c"]);
        assert!(r.of("a").is_none() && r.of("b").is_none());
    }

    #[test]
    fn roles_describe_work_and_never_move_ownership() {
        let mut r = Raids::default();
        r.invite("r1".into(), "review", &ids(&["a", "b"]), "host")
            .unwrap();
        let events = r.set_role("b", "owner", "b").unwrap();
        assert_eq!(kinds(&events), ["raid.role"]);
        assert_eq!(
            events[0].data(),
            json!({"raid":"r1","session":"b","role":"owner"})
        );
        assert_eq!(r.get("review").unwrap().owner, "a");
        // Several members may share a role; setting the same role is a no-op.
        r.set_role("a", "owner", "host").unwrap();
        assert!(r.set_role("a", "owner", "a").unwrap().is_empty());
        for role in ["", "Owner", "two words"] {
            assert_eq!(r.set_role("a", role, "a").unwrap_err(), Error::InvalidRole);
        }
        assert_eq!(
            r.set_role("z", "builder", "z").unwrap_err(),
            Error::NotMember("z".into())
        );
        // A non-owner leaving leaves the raid and its owner in place.
        r.remove("b", Reason::Left);
        assert_eq!(r.get("review").unwrap().owner, "a");
    }

    #[test]
    fn mutations_return_ordered_events_including_undone_changes() {
        let mut r = Raids::default();
        let mut log = vec![];
        log.extend(r.create("r1".into(), "quick", "a", "a").unwrap());
        log.extend(r.join("r1".into(), "quick", "b", false, "a").unwrap());
        log.extend(r.set_role("b", "builder", "b").unwrap());
        log.extend(r.set_role("b", "reviewer", "host").unwrap());
        log.extend(r.set_role("b", "builder", "b").unwrap());
        log.extend(r.remove("a", Reason::Left));
        assert_eq!(
            kinds(&log),
            [
                "raid.created",
                "raid.joined",
                "raid.role",
                "raid.role",
                "raid.role",
                "raid.left",
                "raid.dissolved"
            ]
        );
        assert_eq!(
            log.iter().map(|e| e.actor.as_str()).collect::<Vec<_>>(),
            ["a", "a", "b", "host", "b", "a", "a"]
        );
        assert_eq!(
            log.iter().map(|e| e.addition()).collect::<Vec<_>>(),
            [true, true, true, true, true, false, false]
        );
        assert!(r.raids.is_empty());
    }
}
