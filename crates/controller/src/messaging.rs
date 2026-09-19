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
    },
    Sync(Directory),
}
pub struct Completed {
    pub connection: u64,
    pub id: Value,
    pub result: mailbox::Result<Value>,
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
        let mut mailbox = Mailbox::new(instance.into(), Limits::default()).map_err(|(_, e)| e)?;
        let (commands, input) = mpsc::sync_channel(64);
        let (output, results) = mpsc::sync_channel(64);
        let worker = thread::Builder::new()
            .name("goblins-mailbox".into())
            .spawn(move || {
                let mut journal = Journal {
                    file,
                    bytes: 0,
                    events: vec![],
                };
                let mut known: Directory = BTreeMap::new();
                while let Ok(work) = input.recv() {
                    let directory = match &work {
                        Work::Call { directory, .. } | Work::Sync(directory) => directory,
                    };
                    // Session metadata is supplied only by the host controller. Retain
                    // ancestry for log filtering after its bounded UI records expire.
                    let stopped: Vec<_> = known
                        .values()
                        .filter(|s| s.running && !directory.get(&s.id).is_some_and(|s| s.running))
                        .map(|s| s.id.clone())
                        .collect();
                    known.extend(directory.iter().map(|(id, s)| (id.clone(), s.clone())));
                    for session in known.values_mut() {
                        if !directory.contains_key(&session.id) {
                            session.running = false;
                        }
                    }
                    for id in stopped {
                        let _ = mailbox.stop_recipient(&id, now(), &mut journal);
                    }
                    let Work::Call {
                        connection,
                        id,
                        actor,
                        method,
                        params,
                        directory,
                    } = work
                    else {
                        continue;
                    };
                    let result = match method.as_str() {
                        "messages.get" => parse::<Get>(params)
                            .and_then(|p| mailbox.get(&actor, &p.message).map(|m| json!(m))),
                        "inbox.status" => parse::<Empty>(params).map(|_| mailbox.status(&actor)),
                        "communications.list" if actor == "host" => {
                            parse::<List>(params).and_then(|p| page(&journal, &known, p))
                        }
                        _ => Mutation::parse(&method, params)
                            .map_err(|e| (-32602, e))
                            .and_then(|command| {
                                mailbox.execute(&actor, command, &directory, now(), &mut journal)
                            }),
                    };
                    if output
                        .send(Completed {
                            connection,
                            id,
                            result,
                        })
                        .is_err()
                    {
                        break;
                    }
                }
                for recipient in known
                    .keys()
                    .map(String::as_str)
                    .chain(std::iter::once("host"))
                {
                    let _ = mailbox.stop_recipient(recipient, now(), &mut journal);
                }
            })?;
        Ok(Self {
            commands,
            results,
            worker: Some(worker),
        })
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

fn page(journal: &Journal, directory: &Directory, p: List) -> mailbox::Result<Value> {
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
    for event in journal.events.iter().filter(|e| e.sequence > p.after) {
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
        "latest":journal.events.last().map_or(0,|e|e.sequence)}))
}
