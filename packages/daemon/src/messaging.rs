//! Serialized mailbox and audit IO worker. The controller never waits on disk.
use crate::mailbox;
use crate::mailbox::Audit;
use crate::mailbox::AuditError;
use crate::mailbox::Directory;
use crate::mailbox::Event;
use crate::mailbox::Limits;
use crate::mailbox::Mailbox;
use goblins_protocol::messages::Mutation;
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
}
pub struct Completed {
    pub connection: u64,
    pub id: Value,
    pub result: mailbox::Result<Value>,
    pub actor: String,
}
#[derive(Clone, Debug, PartialEq)]
pub struct Report {
    pub queued: BTreeMap<String, u64>,
    pub health: mailbox::Health,
}
pub struct Service {
    pub commands: SyncSender<Work>,
    pub results: Receiver<Completed>,
    /// Queued inbox counts per recipient and the log's health, sent
    /// whenever either changes.
    pub inbox: Receiver<Report>,
    worker: Option<thread::JoinHandle<()>>,
}

use goblins_protocol::contract::{self, Method};
use goblins_protocol::contract::CommunicationsListParams as List;
fn parse<T: serde::de::DeserializeOwned>(value: Value) -> mailbox::Result<T> {
    serde_json::from_value(value).map_err(|e| (-32602, e.to_string()))
}
pub fn now() -> u64 {
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
        // Unbounded but sent only on change, and drained every tick.
        let (counts, inbox) = mpsc::channel();
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
                    let completed = worker.handle(work);
                    if let Some(report) = worker.changed_report() {
                        let _ = counts.send(report);
                    }
                    if let Some(completed) = completed
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
            inbox,
            worker: Some(worker),
        })
    }
    /// A service whose work nobody consumes, for controller tests that
    /// inspect or fill the queue.
    #[cfg(test)]
    pub fn detached(capacity: usize) -> (Self, Receiver<Work>) {
        let (commands, input) = mpsc::sync_channel(capacity);
        let (_, results) = mpsc::sync_channel(1);
        let (_, inbox) = mpsc::channel();
        (
            Self {
                commands,
                results,
                inbox,
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

/// The worker's serialized state: the mailbox and its logged raid view,
/// and the sessions it has seen.
struct State<L> {
    mailbox: Mailbox,
    journal: L,
    known: Directory,
    /// Queued inbox counts as last reported to the controller.
    reported: Report,
}
impl<L: Log> State<L> {
    fn new(mailbox: Mailbox, journal: L) -> Self {
        Self {
            mailbox,
            journal,
            known: BTreeMap::new(),
            reported: Report {
                queued: BTreeMap::new(),
                health: mailbox::Health::default(),
            },
        }
    }

    /// The queued inbox counts and health, when either differs from what
    /// was last reported.
    fn changed_report(&mut self) -> Option<Report> {
        let report = Report {
            queued: self.mailbox.queued(),
            health: self.mailbox.health(),
        };
        (report != self.reported).then(|| {
            self.reported = report.clone();
            report
        })
    }

    fn handle(&mut self, work: Work) -> Option<Completed> {
        let (mailbox, journal, known) = (&mut self.mailbox, &mut self.journal, &mut self.known);
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
        };
        // Session metadata is supplied only by the host controller. Retain
        // ancestry for log filtering after its bounded UI records expire.
        let stopped: Vec<_> = known
            .values()
            .filter(|s| s.running && !directory.get(&s.id).is_some_and(|s| s.running))
            .map(|s| s.id.clone())
            .collect();
        for (id, session) in directory.iter().filter(|(id, _)| !known.contains_key(*id)) {
            let _ = mailbox.record_typed::<contract::communication_events::SessionStarted>(
                id,
                contract::CommSessionStartedData {
                    agent_name: session.agent_name.clone(),
                },
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
        mailbox.record_raid(raids, directory, now(), journal);
        // Outcomes are recorded before the call too: an inbox fetch that
        // arrives with a completion sees its notice.
        mailbox.record_operations(operations, directory, now(), journal);
        for id in stopped {
            let reason = directory.get(&id).and_then(|s| s.stop_reason.clone());
            let _ = mailbox.record_typed::<contract::communication_events::SessionExited>(
                &id,
                contract::CommSessionExitedData { reason },
                now(),
                journal,
            );
            let _ = mailbox.stop_recipient(&id, now(), journal);
        }
        let Work::Call {
            connection,
            id,
            actor,
            method,
            params,
            directory,
            ..
        } = work
        else {
            return None;
        };
        let result = match method.as_str() {
            contract::host::MessagesGet::NAME => parse::<contract::MailboxGetParams>(params)
                .and_then(|p| mailbox.get(&actor, &p.message).map(|m| json!(m))),
            contract::host::InboxStatus::NAME => {
                parse::<contract::MailboxStatusParams>(params).map(|_| mailbox.status(&actor))
            }
            contract::host::CommunicationsList::NAME if actor == "host" => parse::<List>(params)
                .and_then(|p| page(journal.events(), known, p))
                .map(|p| json!(p)),
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

fn page(
    journal: &[Event],
    directory: &Directory,
    p: List,
) -> mailbox::Result<contract::CommunicationsPageResult> {
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
            let mut found = directory
                .values()
                .filter(|s| s.running && s.agent_name == *target);
            let first = found
                .next()
                .ok_or_else(|| (-32004, "session absent".into()))?;
            if found.next().is_some() {
                return Err((-32009, "ambiguous session; use its ID".into()));
            }
            Ok(first.id.clone())
        })
        .transpose()?;
    let contains = |id: &str| scope.as_ref().is_none_or(|scope| id == scope);
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
            events.push(event.clone());
        }
        cursor = event.sequence;
    }
    Ok(contract::CommunicationsPageResult {
        events,
        cursor,
        session: scope,
        latest: journal.last().map_or(0, |e| e.sequence),
    })
}

/// Offline inspection uses the same cursor and session filter as the live
/// endpoint.
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
        if let Some(session) = event.session_started() {
            // The compatible generated projection preserves legacy path and
            // empty-name precedence without changing the raw stored payload.
            let name = session.name();
            known.insert(
                event.actor.clone(),
                mailbox::Session {
                    id: event.actor.clone(),
                    agent_name: name.into(),
                    stop_reason: None,
                    running: true,
                },
            );
        }
        events.push(event);
    }
    if session.as_ref().is_some_and(|id| !known.contains_key(id))
        && !known
            .values()
            .any(|s| session.as_ref() == Some(&s.agent_name))
    {
        return Err(
            "offline session selection requires a session ID or agent name from session.started"
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
        result.extend(p.events.into_iter().map(|event|json!(event)));
        after = p.cursor;
        if after >= p.latest {
            break;
        }
    }
    Ok(result)
}

#[cfg(test)]
mod tests {
    fn original_page(journal: &[Event], directory: &Directory, p: List) -> mailbox::Result<Value> {
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
                let mut found = directory
                    .values()
                    .filter(|s| s.running && s.agent_name == *target);
                let first = found
                    .next()
                    .ok_or_else(|| (-32004, "session absent".into()))?;
                if found.next().is_some() {
                    return Err((-32009, "ambiguous session; use its ID".into()));
                }
                Ok(first.id.clone())
            })
            .transpose()?;
        let contains = |id: &str| scope.as_ref().is_none_or(|scope| id == scope);
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
    #[test]
    fn generated_pages_match_original_filters_cursor_order_and_byte_budget() {
        let directory: Directory = [
            ("a", "alpha", true),
            ("b", "beta", true),
            ("old", "alpha", false),
            ("d1", "duplicate", true),
            ("d2", "duplicate", true),
        ]
        .into_iter()
        .map(|(id, name, running)| {
            (
                id.into(),
                mailbox::Session {
                    id: id.into(),
                    agent_name: name.into(),
                    stop_reason: None,
                    running,
                },
            )
        })
        .collect();
        let event = |sequence, actor: &str, data| Event {
            instance: "i".into(),
            sequence,
            timestamp: 9,
            kind: "future.event".into(),
            actor: actor.into(),
            sessions: vec![actor.into()],
            data,
        };
        for journal in [
            vec![
                event(1, "a", json!({})),
                event(2, "b", json!({})),
                event(3, "old", json!({})),
                event(4, "a", json!({})),
            ],
            vec![
                event(1, "a", json!("x".repeat(300 * 1024))),
                event(2, "b", json!("x".repeat(300 * 1024))),
                event(3, "a", json!("x".repeat(300 * 1024))),
            ],
            vec![event(1, "a", json!("x".repeat(512 * 1024)))],
            vec![],
        ] {
            for after in [0, 1, 4, u64::MAX] {
                for limit in [0, 1, 2, 100, 101] {
                    for session in [
                        None,
                        Some("a"),
                        Some("alpha"),
                        Some("old"),
                        Some("absent"),
                        Some("duplicate"),
                    ] {
                        let params = List {
                            after,
                            limit,
                            session: session.map(str::to_owned),
                        };
                        let old = original_page(&journal, &directory, params.clone());
                        let new = page(&journal, &directory, params).map(|p| json!(p));
                        assert_eq!(
                            old, new,
                            "after={after}, limit={limit}, session={session:?}"
                        );
                    }
                }
            }
        }
    }

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
        [("a", "chief"), ("b", "scout"), ("x", "other")]
            .into_iter()
            .map(|(id, name)| {
                (
                    id.to_string(),
                    mailbox::Session {
                        id: id.into(),
                        agent_name: name.into(),
                        stop_reason: None,
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
        let done = w.handle(send("x", "scout", "k", batch)).unwrap();
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
        let refused = w.handle(send("x", "scout", "late", vec![])).unwrap();
        assert_eq!(refused.result.unwrap_err().0, -32004);
    }

    #[test]
    fn an_exit_is_logged_with_who_stopped_it() {
        let mut w = state();
        w.handle(Work::Sync {
            directory: directory(),
            raids: vec![],
            operations: vec![],
        });
        let mut stopped = directory();
        let b = stopped.get_mut("b").unwrap();
        b.running = false;
        b.stop_reason = Some("stopped by raid owner chief (a)".into());
        w.handle(Work::Sync {
            directory: stopped,
            raids: vec![],
            operations: vec![],
        });
        let exited = w.journal.events.last().unwrap();
        assert_eq!(
            (exited.kind.as_str(), exited.actor.as_str()),
            ("session.exited", "b")
        );
        assert_eq!(exited.data["reason"], "stopped by raid owner chief (a)");
    }

    #[test]
    fn queued_counts_are_reported_only_when_they_change() {
        let mut w = state();
        let queued = |w: &mut State<Faulty>| w.changed_report().map(|r| r.queued);
        assert_eq!(queued(&mut w), None);
        w.handle(send("host", "scout", "one", vec![])).unwrap();
        w.handle(send("host", "scout", "two", vec![])).unwrap();
        assert_eq!(queued(&mut w), Some([("b".into(), 2)].into()));
        assert_eq!(queued(&mut w), None);
        // A stopped recipient's queued messages fail, emptying its inbox.
        let mut running = directory();
        running.remove("b");
        w.handle(Work::Sync {
            directory: running,
            raids: vec![],
            operations: vec![],
        });
        assert_eq!(queued(&mut w), Some(BTreeMap::new()));
    }

    #[test]
    fn a_join_that_fails_to_write_authorizes_no_send() {
        let mut w = state();
        w.journal.raid = Some(|| AuditError::Unavailable);
        let batch = invite(&mut Raids::default(), &["b", "x"]);
        let done = w.handle(send("x", "scout", "k", batch)).unwrap();
        // A write failure freezes messaging, host routes included.
        assert_eq!(
            done.result.unwrap_err(),
            (-32009, "mailbox audit unavailable".into())
        );
        let done = w.handle(send("host", "scout", "host", vec![])).unwrap();
        assert!(done.result.is_err());
    }

    #[test]
    fn capacity_failure_closes_raid_routes_but_not_host_routes() {
        let mut w = state();
        let mut raids = Raids::default();
        let batch = invite(&mut raids, &["x", "b"]);
        w.handle(send("x", "scout", "first", batch))
            .unwrap()
            .result
            .unwrap();
        let before = w.changed_report().unwrap();
        assert_eq!(before.health, mailbox::Health::default());
        w.journal.raid = Some(|| AuditError::Capacity);
        let role = raids.set_role("x", "builder", "x").unwrap();
        let done = w.handle(send("x", "scout", "second", role)).unwrap();
        assert_eq!(
            done.result.unwrap_err(),
            (-32009, "raid membership could not be recorded".into())
        );
        // The host hears of it although no inbox count changed.
        let report = w.changed_report().unwrap();
        assert_eq!(report.queued, before.queued);
        assert!(report.health.raid_audit_failed && !report.health.audit_failed);
        assert_eq!(w.changed_report(), None);
        assert_eq!(w.mailbox.raids()["x"].role, None);
        let status = w
            .handle(call("x", "inbox.status", json!({}), vec![]))
            .unwrap();
        assert_eq!(status.result.unwrap()["raid_audit_failed"], true);
        let host = w.handle(send("host", "scout", "host", vec![])).unwrap();
        assert_eq!(host.result.unwrap()["recipient"], "b");
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
                json!({"session":"scout"}),
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

    #[test]
    fn c13b2_session_events_preserve_names_nullable_reasons_and_removed_sessions() {
        for reason in [
            None,
            Some(""),
            Some("stopped by owner chief (a)"),
            Some("héllo\0\n"),
        ] {
            for removed in [false, true] {
                let mut w = state();
                let mut initial = directory();
                initial.get_mut("b").unwrap().agent_name = "scout é\0\n".into();
                let sync = |directory| Work::Sync {
                    directory,
                    raids: vec![],
                    operations: vec![],
                };
                w.handle(sync(initial.clone()));
                assert_eq!(w.journal.events.len(), 3);
                for (event, (id, session)) in w.journal.events.iter().zip(initial.iter()) {
                    assert_eq!(
                        serde_json::to_value(event).unwrap(),
                        json!({
                            "instance":INSTANCE,"sequence":if id=="a" {1} else if id=="b" {2} else {3},
                            "timestamp":event.timestamp,"kind":"session.started","actor":id,
                            "sessions":[id],"data":{"agent_name":session.agent_name}})
                    );
                }
                w.handle(sync(initial.clone()));
                assert_eq!(w.journal.events.len(), 3);
                let mut stopped = initial;
                if removed {
                    stopped.remove("b");
                } else {
                    let session = stopped.get_mut("b").unwrap();
                    session.running = false;
                    session.stop_reason = reason.map(str::to_owned);
                }
                w.handle(sync(stopped.clone()));
                assert_eq!(w.journal.events.len(), 4);
                let exited = &w.journal.events[3];
                assert_eq!(
                    serde_json::to_value(exited).unwrap(),
                    json!({
                        "instance":INSTANCE,"sequence":4,"timestamp":exited.timestamp,
                        "kind":"session.exited","actor":"b","sessions":["b"],
                        "data":{"reason":if removed {None} else {reason}}})
                );
                w.handle(sync(stopped));
                assert_eq!(w.journal.events.len(), 4);
            }
        }
    }

    // Local retained files exercise the reader directly, without daemon startup.
    struct C13b3Log(std::path::PathBuf);
    impl C13b3Log {
        fn new(events: &[Value]) -> Self {
            let root = goblins_unix::temp_directory().unwrap();
            let log = Self(root.join("events.jsonl"));
            let mut bytes = Vec::new();
            for event in events {
                serde_json::to_writer(&mut bytes, event).unwrap();
                bytes.push(b'\n');
            }
            std::fs::write(&log.0, bytes).unwrap();
            log
        }
    }
    impl Drop for C13b3Log {
        fn drop(&mut self) {
            std::fs::remove_dir_all(self.0.parent().unwrap()).unwrap();
        }
    }
    fn c13b3_event(sequence: u64, kind: &str, actor: &str, data: Value) -> Value {
        json!({"instance":INSTANCE,"sequence":sequence,"timestamp":9,
                "kind":kind,"actor":actor,"sessions":[actor],"data":data})
    }

    #[test]
    fn c13b3_offline_session_name_fallbacks_preserve_original_events() {
        for (data, name) in [
            (
                json!({"agent_name":"current","path":"/tree/legacy","future":true}),
                "current",
            ),
            (json!({"path":"/tree/legacy"}), "legacy"),
            (json!({"agent_name":null,"path":"/tree/legacy"}), "legacy"),
            (json!({"agent_name":7,"path":"/tree/legacy"}), "legacy"),
            (json!({"agent_name":"","path":"/tree/legacy"}), ""),
            (json!({"path":"/tree/"}), ""),
            (json!({"path":""}), ""),
            (json!({"path":7}), ""),
            (json!({}), ""),
            (Value::Null, ""),
            (json!(["legacy"]), ""),
            (json!(7), ""),
        ] {
            let events = vec![
                c13b3_event(1, "session.started", "a", data),
                c13b3_event(2, "future.event", "a", json!({"anything":[null,1,"é\0"]})),
            ];
            let log = C13b3Log::new(&events);
            for target in [None, Some("a".into()), Some(name.into())] {
                assert_eq!(offline_log(&log.0, target).unwrap(), events);
            }
            assert_eq!(
                offline_log(&log.0, Some("absent".into()))
                    .unwrap_err()
                    .to_string(),
                "offline session selection requires a session ID or agent name from session.started"
            );
        }
        // An exact ID wins over a name; historical duplicate names remain ambiguous.
        let events = vec![
            c13b3_event(1, "session.started", "a", json!({"agent_name":"duplicate"})),
            c13b3_event(2, "session.started", "b", json!({"path":"/old/duplicate"})),
        ];
        let log = C13b3Log::new(&events);
        assert_eq!(
            offline_log(&log.0, Some("a".into())).unwrap(),
            vec![events[0].clone()]
        );
        assert_eq!(
            offline_log(&log.0, Some("duplicate".into()))
                .unwrap_err()
                .to_string(),
            "ambiguous session; use its ID"
        );
    }

    #[test]
    fn c13b3_offline_known_and_future_payloads_remain_opaque() {
        let kinds = [
            "message.accepted",
            "message.claimed",
            "message.replied",
            "message.completed",
            "message.requeued",
            "inbox.empty",
            "operation.retried",
            "recipient.stopped",
            "session.exited",
            "raid.created",
            "raid.joined",
            "raid.role",
            "raid.left",
            "raid.paused",
            "raid.owner",
            "raid.dissolved",
            "raid.notice",
            "operation.requested",
            "operation.decided",
            "operation.completed",
            "operation.duplicate",
            "future.event",
        ];
        let mut events = vec![c13b3_event(
            1,
            "session.started",
            "a",
            json!({"path":"/old/scout"}),
        )];
        for kind in kinds {
            for data in [
                json!({}),
                Value::Null,
                json!(7),
                json!(["future", null]),
                json!({"change":{"message":{"id":"m","sender_path":"/old/a","recipient_path":"/old/b",
                        "future":true},"claim_generation":"malformed"},"notice":{"id":"n","operation":{"status":"future"}}}),
            ] {
                events.push(c13b3_event(events.len() as u64 + 1, kind, "a", data));
            }
        }
        // More than one page; an unrelated last event advances the global cursor.
        events.push(c13b3_event(
            events.len() as u64 + 1,
            "future.event",
            "other",
            json!(false),
        ));
        let log = C13b3Log::new(&events);
        assert_eq!(offline_log(&log.0, None).unwrap(), events);
        assert_eq!(
            offline_log(&log.0, Some("scout".into())).unwrap(),
            events[..events.len() - 1]
        );
    }

    #[test]
    fn c13b3_offline_retains_envelope_and_file_errors() {
        let first = c13b3_event(1, "session.started", "a", json!({"agent_name":"scout"}));
        for field in [
            "instance",
            "sequence",
            "timestamp",
            "kind",
            "actor",
            "sessions",
            "data",
        ] {
            let mut broken = first.clone();
            broken.as_object_mut().unwrap().remove(field);
            let log = C13b3Log::new(&[broken]);
            assert!(
                offline_log(&log.0, None)
                    .unwrap_err()
                    .to_string()
                    .contains(&format!("missing field `{field}`")),
                "{field}"
            );
        }
        for mut second in [
            c13b3_event(3, "future.event", "a", json!({})),
            c13b3_event(2, "future.event", "a", json!({})),
        ] {
            if second["sequence"] == 2 {
                second["instance"] = json!("different");
            }
            let log = C13b3Log::new(&[first.clone(), second]);
            assert_eq!(
                offline_log(&log.0, None).unwrap_err().to_string(),
                "communications log has a sequence gap or mixed instances"
            );
        }
        let log = C13b3Log::new(&[first]);
        let mut bytes = std::fs::read(&log.0).unwrap();
        bytes.pop();
        std::fs::write(&log.0, bytes).unwrap();
        assert_eq!(
            offline_log(&log.0, None).unwrap_err().to_string(),
            "oversized or incomplete communications log"
        );
        std::fs::write(&log.0, b"\n\n").unwrap();
        assert!(offline_log(&log.0, None).unwrap().is_empty());
        assert_eq!(
            offline_log(log.0.parent().unwrap(), None)
                .unwrap_err()
                .to_string(),
            "communications log must be a regular file"
        );
    }

}
