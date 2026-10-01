//! Serialized mailbox and audit IO worker. The controller never waits on disk.
use crate::mailbox::{self, Audit, AuditError, Directory, Event, Limits, Mailbox};
use goblins_protocol::messages::Mutation;
use serde::Deserialize;
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    fs::{File, OpenOptions},
    io::Write,
    os::unix::fs::OpenOptionsExt,
    path::Path,
    sync::mpsc::{self, Receiver, SyncSender},
    thread,
    time::{SystemTime, UNIX_EPOCH},
};

const LOG_LIMIT: usize = 128 * 1024 * 1024;

struct Journal {
    file: File,
    bytes: usize,
    events: Vec<Event>,
}
impl Audit for Journal {
    fn append(
        &mut self,
        event: &Event,
        reserved_bytes: usize,
    ) -> std::result::Result<(), AuditError> {
        let mut line = serde_json::to_vec(event).map_err(|_| AuditError::Unavailable)?;
        line.push(b'\n');
        if self.bytes + line.len() + reserved_bytes > LOG_LIMIT {
            return Err(AuditError::Capacity);
        }
        self.file
            .write_all(&line)
            .map_err(|_| AuditError::Unavailable)?;
        self.bytes += line.len();
        self.events.push(event.clone());
        Ok(())
    }
}

pub enum Work {
    Call {
        connection: u64,
        id: Value,
        actor: String,
        method: String,
        params: Value,
        directory: Directory,
        view: crate::integration::View,
        raids: Vec<crate::raid::Event>,
        operations: Vec<crate::operation::Event>,
    },
    /// Session metadata, raid and operation events queued since the last
    /// batch. An accepted batch is never resent.
    Sync {
        directory: Directory,
        raids: Vec<crate::raid::Event>,
        operations: Vec<crate::operation::Event>,
    },
    Delivery {
        actor: String,
        revision: u64,
        submitted: bool,
    },
}
pub struct Completed {
    pub connection: u64,
    pub id: Value,
    pub result: mailbox::Result<Value>,
    pub actor: String,
}
pub struct Service {
    pub commands: SyncSender<Work>,
    pub results: Receiver<Completed>,
    worker: Option<thread::JoinHandle<()>>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Get {
    message: String,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Empty {}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct List {
    #[serde(default)]
    after: u64,
    #[serde(default = "page_size")]
    limit: usize,
    session: Option<String>,
}
fn page_size() -> usize {
    100
}
fn parse<T: serde::de::DeserializeOwned>(value: Value) -> mailbox::Result<T> {
    serde_json::from_value(value).map_err(|e| (-32602, e.to_string()))
}
fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

impl Service {
    pub fn new(state: &Path, instance: &str) -> crate::Result<Self> {
        let file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW)
            .open(state.join(format!("communications-{instance}.jsonl")))?;
        let mailbox = Mailbox::new(instance.into(), Limits::default()).map_err(|(_, e)| e)?;
        let (commands, input) = mpsc::sync_channel(64);
        let (output, results) = mpsc::sync_channel(64);
        let worker = thread::Builder::new()
            .name("goblins-mailbox".into())
            .spawn(move || {
                let mut worker = State::new(
                    mailbox,
                    Journal {
                        file,
                        bytes: 0,
                        events: vec![],
                    },
                );
                while let Ok(work) = input.recv() {
                    if let Some(completed) = worker.handle(work)
                        && output.send(completed).is_err()
                    {
                        break;
                    }
                }
                worker.finish();
            })?;
        Ok(Self {
            commands,
            results,
            worker: Some(worker),
        })
    }
    /// A service whose work nobody consumes, for controller tests that
    /// inspect or fill the queue.
    #[cfg(test)]
    pub fn detached(capacity: usize) -> (Self, Receiver<Work>) {
        let (commands, input) = mpsc::sync_channel(capacity);
        let (_, results) = mpsc::sync_channel(1);
        (
            Self {
                commands,
                results,
                worker: None,
            },
            input,
        )
    }
}

/// The audit sink plus the events it has accepted, for log pages.
trait Log: Audit {
    fn events(&self) -> &[Event];
}
impl Log for Journal {
    fn events(&self) -> &[Event] {
        &self.events
    }
}

/// The worker's serialized state: the mailbox and its logged raid view, the
/// integrations, and the sessions it has seen.
struct State<L> {
    mailbox: Mailbox,
    journal: L,
    integrations: crate::integration::Integrations,
    known: Directory,
}
impl<L: Log> State<L> {
    fn new(mailbox: Mailbox, journal: L) -> Self {
        Self {
            mailbox,
            journal,
            integrations: crate::integration::Integrations::default(),
            known: BTreeMap::new(),
        }
    }

    fn handle(&mut self, work: Work) -> Option<Completed> {
        let (mailbox, journal, integrations, known) = (
            &mut self.mailbox,
            &mut self.journal,
            &mut self.integrations,
            &mut self.known,
        );
        if let Work::Delivery {
            actor,
            revision,
            submitted,
        } = work
        {
            let _ = mailbox.record(
                &actor,
                "integration.wake_result",
                json!({"terminal_revision":revision,"submitted":submitted}),
                now(),
                journal,
            );
            return None;
        }
        let (directory, raids, operations) = match &work {
            Work::Call {
                directory,
                raids,
                operations,
                ..
            }
            | Work::Sync {
                directory,
                raids,
                operations,
            } => (directory, raids, operations),
            Work::Delivery { .. } => unreachable!(),
        };
        // Session metadata is supplied only by the host controller. Retain
        // ancestry for log filtering after its bounded UI records expire.
        let stopped: Vec<_> = known
            .values()
            .filter(|s| s.running && !directory.get(&s.id).is_some_and(|s| s.running))
            .map(|s| s.id.clone())
            .collect();
        for (id, session) in directory.iter().filter(|(id, _)| !known.contains_key(*id)) {
            let _ = mailbox.record(
                id,
                "session.started",
                json!({"parent":session.parent,"path":session.path}),
                now(),
                journal,
            );
        }
        known.extend(directory.iter().map(|(id, s)| (id.clone(), s.clone())));
        for session in known.values_mut() {
            if !directory.contains_key(&session.id) {
                session.running = false;
            }
        }
        // Membership is recorded before the call it arrived with, so a
        // message is always logged after the membership that allowed it.
        mailbox.record_raid(raids, now(), journal);
        // Outcomes are recorded before the call too: an inbox fetch that
        // arrives with a completion sees its notice.
        mailbox.record_operations(operations, directory, now(), journal);
        for id in stopped {
            let _ = mailbox.record(&id, "session.exited", json!({}), now(), journal);
            let _ = mailbox.stop_recipient(&id, now(), journal);
        }
        let Work::Call {
            connection,
            id,
            actor,
            method,
            params,
            directory,
            view,
            ..
        } = work
        else {
            return None;
        };
        let result = match method.as_str() {
            m if m.starts_with("integration.") => {
                integrations.call(&actor, m, params, (&view, now()), mailbox, journal)
            }
            "messages.get" => {
                parse::<Get>(params).and_then(|p| mailbox.get(&actor, &p.message).map(|m| json!(m)))
            }
            "inbox.status" => parse::<Empty>(params).map(|_| {
                let mut s = mailbox.status(&actor);
                s["integration"] = integrations.status(&actor, now());
                s
            }),
            "communications.list" if actor == "host" => {
                parse::<List>(params).and_then(|p| page(journal.events(), known, p))
            }
            _ => Mutation::parse(&method, params)
                .map_err(|e| (-32602, e))
                .and_then(|command| mailbox.execute(&actor, command, &directory, now(), journal)),
        };
        Some(Completed {
            connection,
            id,
            result,
            actor,
        })
    }

    fn finish(&mut self) {
        for recipient in self
            .known
            .keys()
            .map(String::as_str)
            .chain(std::iter::once("host"))
        {
            let _ = self
                .mailbox
                .stop_recipient(recipient, now(), &mut self.journal);
        }
    }
}

impl Drop for Service {
    fn drop(&mut self) {
        // During orderly daemon teardown, close work and drain responses so a
        // bounded result queue cannot deadlock the final lifecycle audit writes.
        // Running control/terminal handling never joins or waits on this worker.
        let (closed, _) = mpsc::sync_channel(0);
        drop(std::mem::replace(&mut self.commands, closed));
        for _ in &self.results {}
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

fn page(journal: &[Event], directory: &Directory, p: List) -> mailbox::Result<Value> {
    if p.limit == 0 || p.limit > 100 {
        return Err((-32602, "limit must be 1..100".into()));
    }
    let scope = p
        .session
        .as_ref()
        .map(|target| {
            if directory.contains_key(target) {
                return Ok(target.clone());
            }
            let mut found = directory.values().filter(|s| {
                s.running
                    && (s.path == *target || s.path.rsplit('/').next() == Some(target.as_str()))
            });
            let first = found
                .next()
                .ok_or_else(|| (-32004, "session absent".into()))?;
            if found.next().is_some() {
                return Err((-32009, "ambiguous session; use path or ID".into()));
            }
            Ok(first.id.clone())
        })
        .transpose()?;
    let contains = |id: &str| {
        let Some(scope) = &scope else {
            return true;
        };
        let mut current = id;
        for _ in 0..=directory.len() {
            if current == scope {
                return true;
            }
            let Some(parent) = directory.get(current).and_then(|s| s.parent.as_deref()) else {
                break;
            };
            current = parent;
        }
        false
    };
    let mut events = vec![];
    let mut bytes = 0;
    let mut cursor = p.after;
    for event in journal.iter().filter(|e| e.sequence > p.after) {
        if contains(&event.actor) || event.sessions.iter().any(|id| contains(id)) {
            let size = serde_json::to_vec(event)
                .map_err(|e| (-32009, e.to_string()))?
                .len();
            if events.len() >= p.limit || bytes + size > 512 * 1024 {
                break;
            }
            bytes += size;
            events.push(event);
        }
        cursor = event.sequence;
    }
    Ok(json!({"events":events,"cursor":cursor,"session":scope,
        "latest":journal.last().map_or(0,|e|e.sequence)}))
}

/// Offline inspection uses the same cursor/subtree filter as the live endpoint.
pub fn offline_log(path: &Path, session: Option<String>) -> crate::Result<Vec<Value>> {
    use std::io::Read;
    let mut file = File::open(path)?;
    if !file.metadata()?.is_file() {
        return Err("communications log must be a regular file".into());
    }
    let mut bytes = Vec::new();
    (&mut file)
        .take((LOG_LIMIT + 1) as u64)
        .read_to_end(&mut bytes)?;
    if bytes.len() > LOG_LIMIT || (!bytes.is_empty() && bytes.last() != Some(&b'\n')) {
        return Err("oversized or incomplete communications log".into());
    }
    let mut events: Vec<Event> = Vec::new();
    let mut known = Directory::new();
    for line in bytes.split(|b| *b == b'\n').filter(|line| !line.is_empty()) {
        let event: Event = serde_json::from_slice(line)?;
        if event.sequence != events.len() as u64 + 1
            || events
                .first()
                .is_some_and(|first| first.instance != event.instance)
        {
            return Err("communications log has a sequence gap or mixed instances".into());
        }
        if event.kind == "session.started" {
            known.insert(
                event.actor.clone(),
                mailbox::Session {
                    id: event.actor.clone(),
                    path: event.data["path"].as_str().unwrap_or("").into(),
                    parent: event.data["parent"].as_str().map(String::from),
                    running: true,
                },
            );
        }
        events.push(event);
    }
    if session.as_ref().is_some_and(|id| !known.contains_key(id))
        && !known.values().any(|s| session.as_ref() == Some(&s.path))
    {
        return Err(
            "offline session selection requires a session ID or captured path from session.started"
                .into(),
        );
    }
    let mut after = 0;
    let mut result = Vec::new();
    loop {
        let p = page(
            &events,
            &known,
            List {
                after,
                limit: 100,
                session: session.clone(),
            },
        )
        .map_err(|(_, e)| e)?;
        result.extend(p["events"].as_array().unwrap().iter().cloned());
        after = p["cursor"].as_u64().unwrap();
        if after >= p["latest"].as_u64().unwrap() {
            break;
        }
    }
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::raid::Raids;

    const INSTANCE: &str = "0123456789abcdef0123456789abcdef";

    /// Accepts everything unless told to fail raid events.
    #[derive(Default)]
    struct Faulty {
        events: Vec<Event>,
        raid: Option<fn() -> AuditError>,
    }
    impl Audit for Faulty {
        fn append(&mut self, event: &Event, _: usize) -> std::result::Result<(), AuditError> {
            if let Some(error) = self.raid.filter(|_| event.kind.starts_with("raid.")) {
                return Err(error());
            }
            self.events.push(event.clone());
            Ok(())
        }
    }
    impl Log for Faulty {
        fn events(&self) -> &[Event] {
            &self.events
        }
    }

    fn state() -> State<Faulty> {
        State::new(
            Mailbox::new(INSTANCE.into(), Limits::default()).unwrap(),
            Faulty::default(),
        )
    }
    fn directory() -> Directory {
        [
            ("a", None, "chief"),
            ("b", Some("a"), "chief/scout"),
            ("x", None, "other"),
        ]
        .into_iter()
        .map(|(id, parent, path)| {
            (
                id.to_string(),
                mailbox::Session {
                    id: id.into(),
                    parent: parent.map(Into::into),
                    path: path.into(),
                    running: true,
                },
            )
        })
        .collect()
    }
    fn call(actor: &str, method: &str, params: Value, raids: Vec<crate::raid::Event>) -> Work {
        Work::Call {
            connection: 1,
            id: json!(1),
            actor: actor.into(),
            method: method.into(),
            params,
            directory: directory(),
            view: Default::default(),
            raids,
            operations: vec![],
        }
    }
    fn send(actor: &str, to: &str, key: &str, raids: Vec<crate::raid::Event>) -> Work {
        call(
            actor,
            "messages.send",
            json!({"key":key,"to":to,"body":"hello"}),
            raids,
        )
    }
    fn invite(raids: &mut Raids, members: &[&str]) -> Vec<crate::raid::Event> {
        let members: Vec<_> = members.iter().map(|m| m.to_string()).collect();
        raids
            .invite("r1".into(), "review", &members, "host")
            .unwrap()
    }

    #[test]
    fn a_batch_is_recorded_once_in_order_before_the_call_it_arrived_with() {
        let mut w = state();
        let mut raids = Raids::default();
        let batch = invite(&mut raids, &["b", "x"]);
        let done = w.handle(send("x", "chief/scout", "k", batch)).unwrap();
        assert_eq!(done.result.unwrap()["recipient"], "b");
        let kinds: Vec<_> = w
            .journal
            .events
            .iter()
            .map(|e| e.kind.as_str())
            .filter(|k| !k.starts_with("session."))
            .collect();
        assert_eq!(kinds, ["raid.created", "raid.joined", "message.accepted"]);
        let accepted = w.journal.events.last().unwrap();
        assert_eq!(accepted.data["change"]["route"], "raid");
        // A later sync without new events records nothing again.
        let count = w.journal.events.len();
        assert!(
            w.handle(Work::Sync {
                directory: directory(),
                raids: vec![],
                operations: vec![],
            })
            .is_none()
        );
        assert_eq!(w.journal.events.len(), count);
        // Events arriving on a sync are applied before the next call.
        let left = raids.remove("x", crate::raid::Reason::Stopped);
        w.handle(Work::Sync {
            directory: directory(),
            raids: left,
            operations: vec![],
        });
        assert_eq!(w.journal.events.last().unwrap().kind, "raid.left");
        let refused = w.handle(send("x", "chief/scout", "late", vec![])).unwrap();
        assert_eq!(refused.result.unwrap_err().0, -32004);
    }

    #[test]
    fn a_join_that_fails_to_write_authorizes_no_send() {
        let mut w = state();
        w.journal.raid = Some(|| AuditError::Unavailable);
        let batch = invite(&mut Raids::default(), &["b", "x"]);
        let done = w.handle(send("x", "chief/scout", "k", batch)).unwrap();
        // A write failure freezes messaging, tree routes included.
        assert_eq!(
            done.result.unwrap_err(),
            (-32009, "mailbox audit unavailable".into())
        );
        let done = w.handle(send("a", "scout", "tree", vec![])).unwrap();
        assert!(done.result.is_err());
    }

    #[test]
    fn capacity_failure_closes_raid_routes_but_not_tree_routes() {
        let mut w = state();
        let mut raids = Raids::default();
        let batch = invite(&mut raids, &["x", "b"]);
        w.handle(send("x", "chief/scout", "first", batch))
            .unwrap()
            .result
            .unwrap();
        w.journal.raid = Some(|| AuditError::Capacity);
        let role = raids.set_role("x", "builder", "x").unwrap();
        let done = w.handle(send("x", "chief/scout", "second", role)).unwrap();
        assert_eq!(
            done.result.unwrap_err(),
            (-32009, "raid membership could not be recorded".into())
        );
        assert_eq!(w.mailbox.raids()["x"].role, None);
        let status = w
            .handle(call("x", "inbox.status", json!({}), vec![]))
            .unwrap();
        assert_eq!(status.result.unwrap()["raid_audit_failed"], true);
        let tree = w.handle(send("a", "scout", "tree", vec![])).unwrap();
        assert_eq!(tree.result.unwrap()["recipient"], "b");
        // Removals are still applied to the logged view.
        let left = raids.remove("b", crate::raid::Reason::Left);
        w.handle(Work::Sync {
            directory: directory(),
            raids: left,
            operations: vec![],
        });
        assert!(!w.mailbox.raids().contains_key("b"));
        assert!(w.mailbox.raids().contains_key("x"));
    }

    #[test]
    fn a_fetch_sees_the_outcome_it_arrived_with_and_the_log_shows_each_wait() {
        let mut w = state();
        let event = |change| crate::operation::Event {
            request: "r1".into(),
            session: "b".into(),
            actor: "b".into(),
            change,
        };
        w.handle(Work::Sync {
            directory: directory(),
            raids: vec![],
            operations: vec![event(crate::operation::Change::Requested {
                kind: "docker".into(),
                subject: "Docker scope: work".into(),
                reason: "tests".into(),
                automatic: false,
            })],
        });
        let status = w
            .handle(call("b", "inbox.status", json!({}), vec![]))
            .unwrap();
        assert_eq!(status.result.unwrap()["operations"][0]["state"], "pending");
        let Work::Call {
            connection,
            id,
            actor,
            method,
            params,
            directory,
            view,
            raids,
            ..
        } = call("b", "inbox.next", json!({"key":"fetch"}), vec![])
        else {
            unreachable!()
        };
        let fetch = Work::Call {
            connection,
            id,
            actor,
            method,
            params,
            directory,
            view,
            raids,
            operations: vec![
                crate::operation::Event {
                    actor: "host".into(),
                    ..event(crate::operation::Change::Decided { approved: true })
                },
                crate::operation::Event {
                    actor: "goblins".into(),
                    ..event(crate::operation::Change::Completed {
                        kind: "docker".into(),
                        subject: "Docker scope: work".into(),
                        status: "ready".into(),
                        message: None,
                        notify: true,
                    })
                },
            ],
        };
        let claimed = w.handle(fetch).unwrap().result.unwrap();
        assert_eq!(claimed["message"]["operation"]["request"], "r1");
        // Approval wait, execution wait, notice and response are separate events.
        let kinds: Vec<_> = w
            .journal
            .events
            .iter()
            .map(|e| e.kind.as_str())
            .filter(|k| !k.starts_with("session."))
            .collect();
        assert_eq!(
            kinds,
            [
                "operation.requested",
                "operation.decided",
                "operation.completed",
                "message.claimed"
            ]
        );
        // The requester's filtered log shows its operation's lifecycle.
        let page = w
            .handle(call(
                "host",
                "communications.list",
                json!({"session":"chief/scout"}),
                vec![],
            ))
            .unwrap()
            .result
            .unwrap();
        let shown = page["events"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|e| e["kind"].as_str().unwrap().starts_with("operation."))
            .count();
        assert_eq!(shown, 3);
    }

    #[test]
    fn filtered_log_pages_show_raid_events_to_affected_sessions() {
        let mut w = state();
        let mut raids = Raids::default();
        let mut batch = invite(&mut raids, &["a", "x"]);
        batch.extend(raids.destroy("review", "host").unwrap());
        w.handle(Work::Sync {
            directory: directory(),
            raids: batch,
            operations: vec![],
        });
        for (session, expected) in [
            ("x", vec!["raid.joined", "raid.dissolved"]),
            ("chief", vec!["raid.created", "raid.dissolved"]),
        ] {
            let done = w
                .handle(call(
                    "host",
                    "communications.list",
                    json!({"session":session}),
                    vec![],
                ))
                .unwrap();
            let page = done.result.unwrap();
            let kinds: Vec<_> = page["events"]
                .as_array()
                .unwrap()
                .iter()
                .filter_map(|e| e["kind"].as_str().filter(|k| k.starts_with("raid.")))
                .map(String::from)
                .collect();
            assert_eq!(kinds, expected, "{session}");
        }
    }
}
