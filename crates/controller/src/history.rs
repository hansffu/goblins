//! Recreate records: one small JSON file per root goblin, enough to start it
//! again from the current configuration of the same name with the packages it
//! was granted. Nothing here pins store paths. The owning daemon keeps the
//! records in memory and a writer thread persists them, so disk I/O never
//! runs on the event loop and every write, retention and prune is ordered.
use crate::{Result, unix};
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs::{self, File, OpenOptions},
    io::{self, Read, Write},
    os::unix::fs::OpenOptionsExt,
    path::{Path, PathBuf},
    sync::mpsc,
    thread::{self, JoinHandle},
};

pub const VERSION: u32 = 2;
/// Older versions read as the current one, which they are a subset of.
const READABLE: &[u32] = &[1, VERSION];
/// Records kept by default; `goblins server start --keep-sessions N`.
pub const DEFAULT_KEEP: usize = 100;
/// Names per package list.
pub const MAX_NAMES: usize = 256;
/// Bytes per serialized record.
pub const MAX_RECORD: usize = 64 * 1024;

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Packages {
    /// Names whose grant reached `ready`, in grant order (PATH priority).
    pub approved: Vec<String>,
    /// Names the host explicitly denied; information only.
    pub denied: Vec<String>,
    /// Names granted on request without asking, copied from the goblin that
    /// started this one. Disjoint from the other lists.
    #[serde(default)]
    pub auto_approve: Vec<String>,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Record {
    pub version: u32,
    /// Short and random (`new_id`); the record's goblins are in `sessions`.
    pub id: String,
    /// The configuration name, launched from the current manifest.
    pub configuration: String,
    pub agent_name: String,
    pub cwd: Option<PathBuf>,
    pub scope: Option<String>,
    pub dev_shell: Option<String>,
    pub docker_enabled: bool,
    pub packages: Packages,
    /// Written only by the goblin itself (`sessions.set-variables`). The
    /// agent's conversation id, `session_id`, keeps one record per
    /// conversation.
    pub variables: BTreeMap<String, String>,
    /// Every goblin this record ran as, oldest first; a recreate adds the
    /// next instead of a new record. Any of them finds the record.
    pub sessions: Vec<String>,
    /// What could not be restored when this goblin was last recreated, and why.
    /// Skipped packages stay in `approved`, so the next recreate tries again.
    pub skipped: Vec<Skipped>,
    /// Unix milliseconds.
    pub created: u64,
    pub updated: u64,
    /// Informational: a goblin that died with its daemon never gets it.
    pub stopped: Option<u64>,
}
/// A recorded package (or `docker`) a recreate could not restore.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Skipped {
    pub name: String,
    /// One line of at most 256 bytes: whitespace runs become one space,
    /// other control characters are escaped.
    pub reason: String,
}
impl Skipped {
    pub fn new(name: &str, reason: &str) -> Self {
        let text = |s: &str| {
            let mut bounded = String::new();
            let line = s.split_whitespace().collect::<Vec<_>>().join(" ");
            for c in goblins_protocol::plain_text(&line).chars() {
                if bounded.len() + c.len_utf8() > 256 {
                    break;
                }
                bounded.push(c);
            }
            bounded
        };
        Self {
            name: text(name),
            reason: text(reason),
        }
    }
}
/// A short random record id: 8 characters of lowercase Crockford base32
/// (40 bits), easy to read and type. `taken` rejects ids in use.
pub fn new_id(taken: impl Fn(&str) -> bool) -> Result<String> {
    const ALPHABET: &[u8] = b"0123456789abcdefghjkmnpqrstvwxyz";
    let mut random = File::open("/dev/urandom")?;
    for _ in 0..16 {
        let mut bytes = [0; 8];
        random.read_exact(&mut bytes)?;
        let id: String = bytes
            .iter()
            .map(|b| ALPHABET[usize::from(b & 31)] as char)
            .collect();
        // Never a goblin session id's shape, and never one in use.
        if !taken(&id) {
            return Ok(id);
        }
    }
    Err("cannot choose a free record id".into())
}
/// Goblin session ids a record keeps, the newest.
pub const MAX_SESSIONS: usize = 256;
impl Record {
    /// The goblin this record last ran as.
    pub fn latest(&self) -> &str {
        self.sessions.last().map_or("", String::as_str)
    }
    /// The agent's conversation, as the goblin reported it.
    pub fn conversation(&self) -> Option<&str> {
        self.variables.get("session_id").map(String::as_str)
    }
    /// Record that the line now runs as goblin `session`.
    pub fn ran_as(&mut self, session: &str) {
        if !self.sessions.iter().any(|s| s == session) {
            self.sessions.push(session.into());
        }
        let excess = self.sessions.len().saturating_sub(MAX_SESSIONS);
        self.sessions.drain(..excess);
    }
    /// Bytes as written: pretty-printed, with a final newline.
    fn size(&self) -> usize {
        serde_json::to_vec_pretty(self).map_or(usize::MAX, |v| v.len() + 1)
    }
    /// Shrink the record until it fits the size a reader accepts, giving up
    /// what matters least first: why packages were skipped, then which
    /// were, then the oldest goblin ids beyond the latest. False when it
    /// still does not fit.
    pub fn fit(&mut self) -> bool {
        if self.size() <= MAX_RECORD {
            return true;
        }
        for i in (0..self.skipped.len()).rev() {
            self.skipped[i].reason.clear();
            if self.size() <= MAX_RECORD {
                return true;
            }
        }
        while self.skipped.pop().is_some() {
            if self.size() <= MAX_RECORD {
                return true;
            }
        }
        while self.sessions.len() > 1 {
            self.sessions.remove(0);
            if self.size() <= MAX_RECORD {
                return true;
            }
        }
        false
    }
    /// A new record; `ran_as` adds its goblins.
    pub fn new(id: &str, configuration: &str, agent_name: &str, now: u64) -> Self {
        Self {
            version: VERSION,
            id: id.into(),
            configuration: configuration.into(),
            agent_name: agent_name.into(),
            cwd: None,
            scope: None,
            dev_shell: None,
            docker_enabled: false,
            packages: Packages::default(),
            variables: BTreeMap::new(),
            sessions: vec![],
            skipped: vec![],
            created: now,
            updated: now,
            stopped: None,
        }
    }
    /// Apply `change` unless the result would exceed the record's limits.
    /// Ok(true) when something changed, Err with the reason when it does not
    /// fit; the record is unchanged then.
    pub fn update(
        &mut self,
        change: impl FnOnce(&mut Record),
    ) -> std::result::Result<bool, String> {
        let mut next = self.clone();
        change(&mut next);
        if next == *self {
            return Ok(false);
        }
        if next.packages.approved.len() > MAX_NAMES
            || next.packages.denied.len() > MAX_NAMES
            || next.packages.auto_approve.len() > MAX_NAMES
        {
            return Err(format!(
                "the record holds at most {MAX_NAMES} package names per list"
            ));
        }
        if next.variables.len() > goblins_protocol::MAX_VARIABLES {
            return Err(format!(
                "the record holds at most {} variables",
                goblins_protocol::MAX_VARIABLES
            ));
        }
        if next.size() > MAX_RECORD {
            return Err(format!("the record would exceed {} KiB", MAX_RECORD / 1024));
        }
        *self = next;
        Ok(true)
    }
    /// A grant that reached `ready`. Its latest final outcome decides the
    /// list, so a denied or auto-approved name moves to `approved`.
    pub fn approve(&mut self, name: &str) -> std::result::Result<bool, String> {
        self.update(|r| {
            r.packages.denied.retain(|n| n != name);
            r.packages.auto_approve.retain(|n| n != name);
            if !r.packages.approved.iter().any(|n| n == name) {
                r.packages.approved.push(name.into());
            }
        })
    }
    /// An explicit host denial, the latest final outcome for that name.
    pub fn deny(&mut self, name: &str) -> std::result::Result<bool, String> {
        self.update(|r| {
            r.packages.approved.retain(|n| n != name);
            r.packages.auto_approve.retain(|n| n != name);
            if !r.packages.denied.iter().any(|n| n == name) {
                r.packages.denied.push(name.into());
            }
        })
    }
    /// Authority copied when the goblin was started: kept unless the name is
    /// already approved, and it clears an older denial.
    pub fn auto(&mut self, name: &str) -> std::result::Result<bool, String> {
        self.update(|r| {
            if r.packages.approved.iter().any(|n| n == name) {
                return;
            }
            r.packages.denied.retain(|n| n != name);
            if !r.packages.auto_approve.iter().any(|n| n == name) {
                r.packages.auto_approve.push(name.into());
            }
        })
    }
    /// All keys change together; an empty value removes its key.
    pub fn set_variables(
        &mut self,
        changes: &BTreeMap<String, String>,
    ) -> std::result::Result<bool, String> {
        self.update(|r| {
            for (key, value) in changes {
                if value.is_empty() {
                    r.variables.remove(key);
                } else {
                    r.variables.insert(key.clone(), value.clone());
                }
            }
        })
    }
}

/// Which records `history.prune` removes. Running records never are; with
/// no criterion nothing is selected.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Prune {
    /// Not updated within this many milliseconds.
    #[serde(default)]
    pub older_than: Option<u64>,
    /// Last updated before this Unix millisecond.
    #[serde(default)]
    pub before: Option<u64>,
    /// Beyond the N most recently updated records.
    #[serde(default)]
    pub keep: Option<usize>,
    #[serde(default)]
    pub dry_run: bool,
}
impl Prune {
    pub fn is_empty(&self) -> bool {
        self.older_than.is_none() && self.before.is_none() && self.keep.is_none()
    }
    /// Records matching every given criterion, oldest first.
    pub fn select(
        &self,
        records: &BTreeMap<String, Record>,
        running: &BTreeSet<String>,
        now: u64,
    ) -> Vec<String> {
        if self.is_empty() {
            return vec![];
        }
        let newest: BTreeSet<_> = newest_first(records)
            .into_iter()
            .take(self.keep.unwrap_or(usize::MAX))
            .map(|r| r.id.as_str())
            .collect();
        let mut selected: Vec<_> = records
            .values()
            .filter(|r| !running.contains(&r.id))
            .filter(|r| {
                self.older_than
                    .is_none_or(|age| r.updated < now.saturating_sub(age))
            })
            .filter(|r| self.before.is_none_or(|before| r.updated < before))
            .filter(|r| self.keep.is_none() || !newest.contains(r.id.as_str()))
            .collect();
        selected.sort_by_key(|r| (r.updated, r.id.clone()));
        selected.into_iter().map(|r| r.id.clone()).collect()
    }
}
/// Most recently updated first.
pub fn newest_first(records: &BTreeMap<String, Record>) -> Vec<&Record> {
    let mut list: Vec<_> = records.values().collect();
    list.sort_by(|a, b| b.updated.cmp(&a.updated).then_with(|| b.id.cmp(&a.id)));
    list
}

/// Another process owns the store; the text names it.
#[derive(Debug)]
pub struct Held(pub String);
impl std::fmt::Display for Held {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}
impl std::error::Error for Held {}
enum Op {
    Write(Box<Record>),
    /// Delete these records; `tag` identifies the caller waiting for it.
    Delete {
        ids: Vec<String>,
        tag: Option<u64>,
    },
    /// Write `record`, then delete `sources` only once it is durable: the
    /// records merged into it.
    Merge {
        record: Box<Record>,
        sources: Vec<String>,
    },
}
/// What a delete actually did, once its files are gone for good.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Deleted {
    pub tag: Option<u64>,
    pub removed: Vec<String>,
    /// Records still on disk, with the reason.
    pub failed: Vec<(String, String)>,
}
/// The history store this process owns: its lock, the records and the writer.
pub struct Store {
    pub directory: PathBuf,
    pub records: BTreeMap<String, Record>,
    /// Records whose deletion the writer has not confirmed yet.
    pub deleting: BTreeSet<String>,
    commands: Option<mpsc::Sender<Op>>,
    deleted: mpsc::Receiver<Deleted>,
    thread: Option<JoinHandle<()>>,
    _lock: File,
}
impl Store {
    /// `$XDG_STATE_HOME/goblins/sessions`, resolved even before it exists.
    pub fn default_directory() -> Result<PathBuf> {
        unix::state_directory("sessions", false)
    }
    /// Take the store's lock, naming this process as its `owner`, and read
    /// its records. Fails with `Held` when another process owns the store.
    pub fn open(directory: &Path, owner: &str) -> Result<Self> {
        unix::private_directory(directory)?;
        let directory = fs::canonicalize(directory)?;
        let path = directory.join(".lock");
        let mut lock = unix::lock(&path).map_err(|_| {
            let holder = fs::read_to_string(&path).unwrap_or_default();
            let holder = goblins_protocol::plain_text(holder.trim());
            Held(format!(
                "{} owns {}",
                if holder.is_empty() {
                    "another Goblins process"
                } else {
                    &holder
                },
                directory.display()
            ))
        })?;
        // Readers name the owner from the lock file's contents.
        lock.set_len(0)?;
        lock.write_all(owner.as_bytes())?;
        // Only the owner removes what an interrupted write left behind.
        for entry in fs::read_dir(&directory)? {
            let entry = entry?;
            if entry
                .file_name()
                .to_str()
                .is_some_and(|n| n.starts_with(".tmp-"))
            {
                let _ = fs::remove_file(entry.path());
            }
        }
        let records = load(&directory)?;
        let (commands, received) = mpsc::channel();
        let (results, deleted) = mpsc::channel();
        let target = directory.clone();
        let thread = thread::Builder::new()
            .name("goblins-history".into())
            .spawn(move || {
                while let Ok(op) = received.recv() {
                    let mut batch = vec![op];
                    batch.extend(received.try_iter());
                    for result in apply(&target, batch, sync) {
                        let _ = results.send(result);
                    }
                }
            })?;
        Ok(Self {
            directory,
            records,
            deleting: BTreeSet::new(),
            commands: Some(commands),
            deleted,
            thread: Some(thread),
            _lock: lock,
        })
    }
    /// Replace a record, in memory now and on disk soon. Only what a reader
    /// accepts is written: the record is shrunk to fit, and one that cannot
    /// fit leaves the previous revision in place.
    pub fn write(&mut self, mut record: Record) {
        if !record.fit() {
            eprintln!(
                "goblins: session record {} would exceed {} KiB; keeping the previous one",
                record.id,
                MAX_RECORD / 1024
            );
            return;
        }
        self.records.insert(record.id.clone(), record.clone());
        self.send(Op::Write(Box::new(record)));
    }
    /// Replace `record` with the records `sources` merged into it. They are
    /// deleted only once it is written durably; until then, or if it cannot
    /// be, they stay.
    pub fn merge(&mut self, mut record: Record, sources: &[String]) {
        if !record.fit() {
            eprintln!(
                "goblins: session record {} would exceed {} KiB; not merging into it",
                record.id,
                MAX_RECORD / 1024
            );
            return;
        }
        self.records.insert(record.id.clone(), record.clone());
        self.deleting.extend(sources.iter().cloned());
        self.send(Op::Merge {
            record: Box::new(record),
            sources: sources.to_vec(),
        });
    }
    /// Delete records, dropping any revision of them still queued. They
    /// stay until the writer confirms; `poll` reports the outcome under
    /// `tag`, and a record that could not be deleted is kept.
    pub fn delete(&mut self, ids: &[String], tag: Option<u64>) {
        self.deleting.extend(ids.iter().cloned());
        self.send(Op::Delete {
            ids: ids.to_vec(),
            tag,
        });
    }
    /// Outcomes of finished deletes, applied to the records.
    pub fn poll(&mut self) -> Vec<Deleted> {
        let results: Vec<_> = self.deleted.try_iter().collect();
        for result in &results {
            self.settle(result);
        }
        results
    }
    /// Delete and wait for the outcome, for the CLI without a server.
    pub fn delete_now(&mut self, ids: &[String]) -> Deleted {
        self.delete(ids, Some(0));
        loop {
            match self.deleted.recv() {
                Ok(result) => {
                    self.settle(&result);
                    if result.tag == Some(0) {
                        return result;
                    }
                }
                Err(_) => {
                    return Deleted {
                        tag: Some(0),
                        removed: vec![],
                        failed: ids
                            .iter()
                            .map(|id| (id.clone(), "history writer stopped".into()))
                            .collect(),
                    };
                }
            }
        }
    }
    fn settle(&mut self, result: &Deleted) {
        for id in &result.removed {
            self.records.remove(id);
            self.deleting.remove(id);
        }
        for (id, reason) in &result.failed {
            self.deleting.remove(id);
            eprintln!("goblins: cannot delete session record {id}: {reason}");
        }
    }
    fn send(&self, op: Op) {
        if let Some(commands) = &self.commands {
            // The thread only stops when this sender is dropped.
            let _ = commands.send(op);
        }
    }
    /// Delete the least recently updated records that are not running,
    /// beyond `keep`. Records left without `stopped` by a crash count as
    /// stopped. Returns the deleted ids.
    pub fn retain(&mut self, keep: usize, running: &BTreeSet<String>) -> Vec<String> {
        let excess = (self.records.len() - self.deleting.len()).saturating_sub(keep);
        let mut old: Vec<_> = self
            .records
            .values()
            .filter(|r| !running.contains(&r.id) && !self.deleting.contains(&r.id))
            .map(|r| (r.updated, r.id.clone()))
            .collect();
        old.sort();
        let ids: Vec<_> = old.into_iter().take(excess).map(|(_, id)| id).collect();
        if !ids.is_empty() {
            self.delete(&ids, None);
        }
        ids
    }
    /// A record by its id, any goblin session id it ran as, its agent's
    /// conversation id, or a unique prefix of one of its ids. Names are
    /// reusable labels and select nothing.
    pub fn resolve(&self, target: &str) -> std::result::Result<&Record, String> {
        if target.is_empty() {
            return Err("name a recorded goblin by id; see 'goblins history'".into());
        }
        let exact = |r: &&Record| {
            r.id == target
                || r.sessions.iter().any(|s| s == target)
                || r.conversation() == Some(target)
        };
        if let Some(record) = newest_first(&self.records).into_iter().find(exact) {
            return Ok(record);
        }
        let prefixed: Vec<_> = self
            .records
            .values()
            .filter(|r| {
                r.id.starts_with(target) || r.sessions.iter().any(|s| s.starts_with(target))
            })
            .collect();
        match prefixed.len() {
            1 => Ok(prefixed[0]),
            0 => Err(format!(
                "no recorded goblin '{}'; see 'goblins history'",
                goblins_protocol::plain_text(target)
            )),
            n => Err(format!(
                "{n} records have ids starting with '{}'; use more of the id",
                goblins_protocol::plain_text(target)
            )),
        }
    }
}
impl Drop for Store {
    fn drop(&mut self) {
        // Closing the channel lets the writer finish what is queued.
        self.commands.take();
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}
fn path(directory: &Path, id: &str) -> PathBuf {
    directory.join(format!("{id}.json"))
}
/// One batch of queued operations, in order: only the newest revision of a
/// record is written, and a delete drops revisions queued before it.
/// A directory fsync, which makes renames and deletions in it durable.
fn sync(directory: &Path) -> io::Result<()> {
    File::open(directory)?.sync_all()
}
fn apply(directory: &Path, batch: Vec<Op>, sync: impl Fn(&Path) -> io::Result<()>) -> Vec<Deleted> {
    // Revisions coalesce, but never move past a delete queued after them.
    let mut pending = BTreeMap::new();
    let flush = |pending: &mut BTreeMap<String, Box<Record>>| {
        for record in std::mem::take(pending).values() {
            if let Err(e) = write(directory, record) {
                eprintln!("goblins: cannot write session record {}: {e}", record.id);
            }
        }
    };
    let mut results = vec![];
    for op in batch {
        match op {
            Op::Write(record) => {
                pending.insert(record.id.clone(), record);
            }
            Op::Delete { ids, tag } => {
                // Queued revisions of deleted records are dropped.
                for id in &ids {
                    pending.remove(id);
                }
                flush(&mut pending);
                results.push(remove(directory, ids, tag, &sync));
            }
            Op::Merge { record, sources } => {
                pending.remove(&record.id);
                for id in &sources {
                    pending.remove(id);
                }
                flush(&mut pending);
                results.push(match write(directory, &record) {
                    Ok(()) => remove(directory, sources, None, &sync),
                    // Nothing is lost: the sources stay, and so does the
                    // previous revision of the record.
                    Err(e) => Deleted {
                        tag: None,
                        removed: vec![],
                        failed: sources
                            .into_iter()
                            .map(|id| {
                                let reason = format!(
                                    "record {} it merged into was not written: {e}",
                                    record.id
                                );
                                (id, reason)
                            })
                            .collect(),
                    },
                });
            }
        }
    }
    flush(&mut pending);
    results
}
/// Delete record files, durably before they are reported removed.
fn remove(
    directory: &Path,
    ids: Vec<String>,
    tag: Option<u64>,
    sync: impl Fn(&Path) -> io::Result<()>,
) -> Deleted {
    let mut result = Deleted {
        tag,
        ..Default::default()
    };
    for id in ids {
        match fs::remove_file(path(directory, &id)) {
            Ok(()) => result.removed.push(id),
            Err(e) if e.kind() == io::ErrorKind::NotFound => result.removed.push(id),
            Err(e) => result.failed.push((id, e.to_string())),
        }
    }
    // A deletion that a crash could undo has not happened yet.
    if !result.removed.is_empty()
        && let Err(e) = sync(directory)
    {
        let removed = std::mem::take(&mut result.removed);
        result
            .failed
            .extend(removed.into_iter().map(|id| (id, e.to_string())));
    }
    result
}
/// Temp file, fsync, rename and directory fsync: a reader sees the old or
/// the new record, never part of one.
fn write(directory: &Path, record: &Record) -> io::Result<()> {
    if !goblins_protocol::identifier(&record.id) {
        return Err(io::Error::other("invalid record id"));
    }
    let temp = directory.join(format!(".tmp-{}.json", record.id));
    let result = (|| {
        let mut file = OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW)
            .open(&temp)?;
        serde_json::to_writer_pretty(&mut file, record)?;
        file.write_all(b"\n")?;
        file.sync_all()?;
        fs::rename(&temp, path(directory, &record.id))?;
        File::open(directory)?.sync_all()
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temp);
    }
    result
}
/// Every readable record; anything else is reported and left alone. Without
/// the store lock this is only a snapshot.
pub fn load(directory: &Path) -> Result<BTreeMap<String, Record>> {
    let mut records = BTreeMap::new();
    for entry in fs::read_dir(directory)? {
        let entry = entry?;
        let name = entry.file_name();
        let Some(name) = name.to_str() else {
            continue;
        };
        if name.starts_with(".tmp-") {
            // Being written, or left by a crash; the owner cleans it up.
            continue;
        }
        let Some(id) = name.strip_suffix(".json") else {
            continue;
        };
        if !goblins_protocol::identifier(id) {
            continue;
        }
        match read(&entry.path()) {
            Ok(mut record) if record.id == id && READABLE.contains(&record.version) => {
                // Written back as the current version.
                record.version = VERSION;
                records.insert(id.to_string(), record);
            }
            Ok(_) => eprintln!("goblins: ignoring session record {name}: unexpected id or version"),
            Err(e) => eprintln!("goblins: ignoring session record {name}: {e}"),
        }
    }
    Ok(records)
}
fn read(path: &Path) -> Result<Record> {
    let file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(path)?;
    if !file.metadata()?.is_file() {
        return Err("not a regular file".into());
    }
    let mut bytes = Vec::new();
    file.take(MAX_RECORD as u64 + 1).read_to_end(&mut bytes)?;
    if bytes.len() > MAX_RECORD {
        return Err("record too large".into());
    }
    Ok(serde_json::from_slice(&bytes)?)
}

/// `30d`, `12h` or `2w` in milliseconds.
pub fn parse_age(text: &str) -> std::result::Result<u64, String> {
    let error =
        || format!("invalid age '{text}': use a number of days (30d), hours (12h) or weeks (2w)");
    let unit = match text.chars().last() {
        Some('h') => 3_600_000,
        Some('d') => 86_400_000,
        Some('w') => 7 * 86_400_000,
        _ => return Err(error()),
    };
    let number = &text[..text.len() - 1];
    if number.is_empty() || !number.bytes().all(|b| b.is_ascii_digit()) {
        return Err(error());
    }
    number
        .parse::<u64>()
        .ok()
        .and_then(|n| n.checked_mul(unit))
        .ok_or_else(error)
}
/// The start of a local calendar day, `YYYY-MM-DD`, in Unix milliseconds.
pub fn parse_date(text: &str) -> std::result::Result<u64, String> {
    let error = || format!("invalid date '{text}': use YYYY-MM-DD");
    let parts: Vec<_> = text.split('-').collect();
    if parts.len() != 3
        || parts[0].len() != 4
        || parts[1].len() != 2
        || parts[2].len() != 2
        || !parts.iter().all(|p| p.bytes().all(|b| b.is_ascii_digit()))
    {
        return Err(error());
    }
    let [year, month, day] = [parts[0], parts[1], parts[2]].map(|p| p.parse::<i32>().unwrap());
    if !(1970..=9999).contains(&year) || !(1..=12).contains(&month) || !(1..=31).contains(&day) {
        return Err(error());
    }
    let mut tm: libc::tm = unsafe { std::mem::zeroed() };
    tm.tm_year = year - 1900;
    tm.tm_mon = month - 1;
    tm.tm_mday = day;
    tm.tm_isdst = -1;
    let seconds = unsafe { libc::mktime(&mut tm) };
    // mktime normalizes 2026-02-30 into March; that is not a date.
    if seconds < 0 || tm.tm_mday != day || tm.tm_mon != month - 1 {
        return Err(error());
    }
    Ok(seconds as u64 * 1000)
}
/// `YYYY-MM-DD HH:MM` in local time.
pub fn local_time(millis: u64) -> String {
    let seconds = (millis / 1000) as libc::time_t;
    let mut tm: libc::tm = unsafe { std::mem::zeroed() };
    if unsafe { libc::localtime_r(&seconds, &mut tm) }.is_null() {
        return "-".into();
    }
    format!(
        "{:04}-{:02}-{:02} {:02}:{:02}",
        tm.tm_year + 1900,
        tm.tm_mon + 1,
        tm.tm_mday,
        tm.tm_hour,
        tm.tm_min
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    /// A record that ran as one goblin of the same id.
    fn record(id: &str, updated: u64) -> Record {
        let mut r = Record::new(id, "shell", "snikk", updated);
        r.ran_as(id);
        r.updated = updated;
        r
    }
    fn on_disk(directory: &Path) -> BTreeMap<String, Record> {
        load(directory).unwrap()
    }
    /// Wait until the writer confirmed every delete.
    fn settle(s: &mut Store) {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while !s.deleting.is_empty() && std::time::Instant::now() < deadline {
            s.poll();
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        // Writes, too: a tagged no-op delete follows them through the writer.
        s.delete_now(&[]);
    }
    fn store() -> (Store, PathBuf) {
        let root = unix::temp_directory().unwrap();
        let directory = root.join("sessions");
        (Store::open(&directory, "test").unwrap(), root)
    }
    #[test]
    fn approved_and_denied_follow_the_latest_final_outcome() {
        let mut r = record("a", 1);
        assert_eq!(r.approve("jq"), Ok(true));
        assert_eq!(r.approve("ripgrep"), Ok(true));
        assert_eq!(r.approve("jq"), Ok(false));
        assert_eq!(r.deny("nmap"), Ok(true));
        assert_eq!(r.packages.approved, ["jq", "ripgrep"]);
        // A later denial moves it, and a later grant moves it back, last.
        assert_eq!(r.deny("jq"), Ok(true));
        assert_eq!(r.packages.approved, ["ripgrep"]);
        assert_eq!(r.packages.denied, ["nmap", "jq"]);
        assert_eq!(r.approve("jq"), Ok(true));
        assert_eq!(r.packages.approved, ["ripgrep", "jq"]);
        assert_eq!(r.packages.denied, ["nmap"]);
    }
    #[test]
    fn auto_approve_yields_to_explicit_outcomes_and_v1_records_read_as_v2() {
        let mut r = record("a", 1);
        r.deny("nmap").unwrap();
        r.approve("jq").unwrap();
        // Copied authority clears an older denial but never an approval.
        assert_eq!(r.auto("nmap"), Ok(true));
        assert_eq!(r.auto("jq"), Ok(false));
        assert_eq!(r.auto("htop"), Ok(true));
        assert_eq!(r.packages.auto_approve, ["nmap", "htop"]);
        assert!(r.packages.denied.is_empty());
        // Explicit outcomes move a name out of auto_approve.
        r.approve("htop").unwrap();
        r.deny("nmap").unwrap();
        assert_eq!(r.packages.approved, ["jq", "htop"]);
        assert_eq!(r.packages.denied, ["nmap"]);
        assert!(r.packages.auto_approve.is_empty());
        // An approved name that failed to restore stays approved; a host
        // denial of its new request removes it, so it is not restored again.
        r.deny("jq").unwrap();
        assert_eq!(r.packages.approved, ["htop"]);
        assert_eq!(r.packages.denied, ["nmap", "jq"]);
        // A version 1 record has no auto_approve and is read as version 2.
        let (_store, root) = store();
        let directory = root.join("v1");
        fs::create_dir(&directory).unwrap();
        let mut v1 = serde_json::to_value(record("old", 1)).unwrap();
        v1["version"] = 1.into();
        v1["packages"]
            .as_object_mut()
            .unwrap()
            .remove("auto_approve");
        fs::write(directory.join("old.json"), v1.to_string()).unwrap();
        let mut v3 = v1.clone();
        v3["version"] = 3.into();
        fs::write(
            directory.join("new.json"),
            v3.to_string().replace("\"old\"", "\"new\""),
        )
        .unwrap();
        let loaded = load(&directory).unwrap();
        assert_eq!(loaded.keys().collect::<Vec<_>>(), ["old"]);
        assert_eq!(loaded["old"].version, VERSION);
        assert!(loaded["old"].packages.auto_approve.is_empty());
        fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn limits_leave_the_record_unchanged() {
        let mut r = record("a", 1);
        for i in 0..MAX_NAMES {
            r.approve(&format!("p{i}")).unwrap();
        }
        let before = r.clone();
        assert!(r.approve("one-more").unwrap_err().contains("256"));
        assert_eq!(r, before);
        // Long names reach the byte limit first.
        let mut r = record("b", 1);
        let long = "x".repeat(190);
        let mut refused = None;
        for i in 0..2 * MAX_NAMES {
            let name = format!("{long}{i}");
            let result = if i % 2 == 0 {
                r.approve(&name)
            } else {
                r.deny(&name)
            };
            if let Err(e) = result {
                refused = Some(e);
                break;
            }
        }
        assert!(refused.unwrap().contains("64 KiB"));
        assert!(serde_json::to_vec_pretty(&r).unwrap().len() < MAX_RECORD);
        let mut r = record("c", 1);
        let set = |pairs: &[(&str, &str)]| {
            pairs
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect::<BTreeMap<_, _>>()
        };
        assert_eq!(
            r.set_variables(&set(&[("session_id", "x"), ("session_name", "y")])),
            Ok(true)
        );
        assert_eq!(r.set_variables(&set(&[("session_name", "")])), Ok(true));
        assert_eq!(r.variables, set(&[("session_id", "x")]));
        let many: BTreeMap<_, _> = (0..16).map(|i| (format!("k{i}"), "v".into())).collect();
        assert!(r.set_variables(&many).is_err());
        assert_eq!(r.variables, set(&[("session_id", "x")]));
    }
    #[test]
    fn writes_shrink_a_record_to_what_a_reader_accepts() {
        let (mut s, _root) = store();
        // Near the limit with package names, then skip entries on top, as a
        // resume adds them outside `update`.
        let mut r = record("a", 1);
        let long = "x".repeat(190);
        let mut i = 0;
        while r.approve(&format!("{long}{i}")).is_ok() && r.size() < 53 * 1024 {
            i += 1;
        }
        for i in 0..64 {
            r.skipped
                .push(Skipped::new(&format!("p{i}"), &"why ".repeat(100)));
        }
        r.ran_as("s1");
        r.ran_as("s2");
        assert!(r.size() > MAX_RECORD);
        s.write(r.clone());
        let written = s.records["a"].clone();
        assert!(written.size() <= MAX_RECORD);
        // Reasons go first, and only as many as needed.
        assert_eq!(written.packages, r.packages);
        assert_eq!(written.skipped.len(), 64);
        assert!(written.skipped[0].reason.starts_with("why"));
        assert!(written.skipped[63].reason.is_empty());
        assert_eq!(written.sessions, r.sessions);
        let directory = s.directory.clone();
        settle(&mut s);
        drop(s);
        assert_eq!(on_disk(&directory)["a"], written);
        // A record that cannot fit leaves the previous revision.
        let (mut s, _root) = store();
        let mut fits = record("b", 1);
        fits.ran_as("s1");
        s.write(fits.clone());
        let mut huge = fits.clone();
        huge.variables
            .insert("session_name".into(), "y".repeat(2 * MAX_RECORD));
        s.write(huge);
        assert_eq!(s.records["b"], fits);
        let directory = s.directory.clone();
        settle(&mut s);
        drop(s);
        assert_eq!(on_disk(&directory)["b"], fits);
    }
    #[test]
    fn writes_replace_records_atomically_and_survive_reopening() {
        let (mut s, root) = store();
        let mut a = record("a", 1);
        s.write(a.clone());
        a.approve("jq").unwrap();
        a.updated = 2;
        s.write(a.clone());
        let directory = s.directory.clone();
        drop(s);
        assert_eq!(on_disk(&directory)["a"], a);
        let mode = fs::metadata(path(&directory, "a")).unwrap();
        assert_eq!(
            std::os::unix::fs::PermissionsExt::mode(&mode.permissions()) & 0o777,
            0o600
        );
        assert!(fs::read_dir(&directory).unwrap().all(|e| {
            !e.unwrap()
                .file_name()
                .to_str()
                .unwrap()
                .starts_with(".tmp-")
        }));
        // An interrupted write leaves the previous record and is cleaned up.
        fs::write(directory.join(".tmp-a.json"), b"{\"partial").unwrap();
        fs::write(directory.join("broken.json"), b"{\"partial").unwrap();
        // Reading without the lock leaves an owner's temporary file alone.
        load(&directory).unwrap();
        assert!(directory.join(".tmp-a.json").exists());
        let s = Store::open(&directory, "the test owner (pid 1)").unwrap();
        assert_eq!(s.records.len(), 1);
        assert_eq!(s.records["a"], a);
        assert!(!directory.join(".tmp-a.json").exists());
        // One owner at a time.
        let held = Store::open(&directory, "second").err().unwrap();
        assert!(held.is::<Held>());
        assert_eq!(
            held.to_string(),
            format!("the test owner (pid 1) owns {}", directory.display())
        );
        drop(s);
        fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn the_newest_queued_revision_wins_and_a_delete_drops_queued_ones() {
        let root = unix::temp_directory().unwrap();
        let mut first = record("a", 1);
        let mut second = first.clone();
        second.approve("jq").unwrap();
        second.updated = 2;
        apply(
            &root,
            vec![
                Op::Write(Box::new(first.clone())),
                Op::Write(Box::new(second.clone())),
            ],
            sync,
        );
        assert_eq!(on_disk(&root)["a"], second);
        // A prune after a queued write: neither revision is left.
        first.updated = 3;
        let results = apply(
            &root,
            vec![
                Op::Write(Box::new(first.clone())),
                Op::Delete {
                    ids: vec!["a".into()],
                    tag: Some(7),
                },
                Op::Write(Box::new(record("b", 4))),
            ],
            sync,
        );
        assert_eq!(
            results,
            [Deleted {
                tag: Some(7),
                removed: vec!["a".into()],
                failed: vec![]
            }]
        );
        assert_eq!(on_disk(&root).into_keys().collect::<Vec<_>>(), ["b"]);
        fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn a_merge_deletes_its_sources_only_once_the_record_is_written() {
        let root = unix::temp_directory().unwrap();
        apply(
            &root,
            vec![
                Op::Write(Box::new(record("a", 1))),
                Op::Write(Box::new(record("b", 2))),
            ],
            sync,
        );
        let mut merged = record("c", 3);
        merged.approve("jq").unwrap();
        // The record cannot be written (here: an id no file may have).
        let mut unwritable = merged.clone();
        unwritable.id = "../c".into();
        let results = apply(
            &root,
            vec![Op::Merge {
                record: Box::new(unwritable),
                sources: vec!["a".into(), "b".into()],
            }],
            sync,
        );
        assert!(results[0].removed.is_empty());
        assert_eq!(
            results[0]
                .failed
                .iter()
                .map(|f| f.0.as_str())
                .collect::<Vec<_>>(),
            ["a", "b"]
        );
        assert!(results[0].failed[0].1.contains("was not written"));
        assert_eq!(on_disk(&root).into_keys().collect::<Vec<_>>(), ["a", "b"]);
        // Written: the sources go, and a queued revision of one goes with them.
        let results = apply(
            &root,
            vec![
                Op::Write(Box::new(record("a", 4))),
                Op::Merge {
                    record: Box::new(merged.clone()),
                    sources: vec!["a".into(), "b".into()],
                },
            ],
            sync,
        );
        assert_eq!(results[0].removed, ["a", "b"]);
        assert_eq!(on_disk(&root).into_values().collect::<Vec<_>>(), [merged]);
        fs::remove_dir_all(root).unwrap();
        // Through the store: kept in memory until the writer confirms.
        let (mut s, _root) = store();
        s.write(record("a", 1));
        s.merge(record("c", 3), &["a".into()]);
        assert!(s.deleting.contains("a"));
        settle(&mut s);
        assert!(!s.records.contains_key("a") && s.records.contains_key("c"));
    }
    #[test]
    fn a_failed_delete_is_reported_and_the_record_kept() {
        use std::os::unix::fs::PermissionsExt;
        let (mut s, root) = store();
        s.write(record("a", 1));
        s.write(record("b", 2));
        let directory = s.directory.clone();
        settle(&mut s);
        fs::set_permissions(&directory, fs::Permissions::from_mode(0o500)).unwrap();
        let result = s.delete_now(&["a".into()]);
        fs::set_permissions(&directory, fs::Permissions::from_mode(0o700)).unwrap();
        assert!(result.removed.is_empty());
        assert_eq!(result.failed.len(), 1);
        assert_eq!(result.failed[0].0, "a");
        assert!(s.records.contains_key("a"));
        assert!(s.deleting.is_empty());
        let result = s.delete_now(&["a".into()]);
        assert_eq!(result.removed, ["a"]);
        assert!(!s.records.contains_key("a"));
        drop(s);
        assert_eq!(on_disk(&directory).into_keys().collect::<Vec<_>>(), ["b"]);
        fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn deletes_are_durable_before_they_are_reported() {
        let root = unix::temp_directory().unwrap();
        apply(&root, vec![Op::Write(Box::new(record("a", 1)))], sync);
        apply(&root, vec![Op::Write(Box::new(record("b", 1)))], sync);
        let synced = std::cell::RefCell::new(vec![]);
        let delete = |id: &str| Op::Delete {
            ids: vec![id.into()],
            tag: None,
        };
        // Each delete fsyncs the directory after unlinking.
        let results = apply(&root, vec![delete("a")], |d: &Path| {
            synced.borrow_mut().push(d.join("a.json").exists());
            Ok(())
        });
        assert_eq!(*synced.borrow(), [false]);
        assert_eq!(results[0].removed, ["a"]);
        // An fsync that fails leaves the deletion unconfirmed.
        let results = apply(&root, vec![delete("b")], |_: &Path| {
            Err(io::Error::other("fsync failed"))
        });
        assert!(results[0].removed.is_empty());
        assert_eq!(
            results[0].failed,
            [("b".to_string(), "fsync failed".to_string())]
        );
        fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn retention_skips_running_records_and_counts_crash_left_ones() {
        let (mut s, root) = store();
        let mut crashed = record("crashed", 1);
        crashed.stopped = None;
        s.write(crashed);
        let mut stopped = record("stopped", 2);
        stopped.stopped = Some(2);
        s.write(stopped);
        s.write(record("running", 0));
        s.write(record("newest", 5));
        let running = BTreeSet::from(["running".to_string()]);
        assert_eq!(s.retain(2, &running), ["crashed", "stopped"]);
        // Records being deleted are not selected twice.
        assert!(s.retain(2, &running).is_empty());
        settle(&mut s);
        assert_eq!(s.records.keys().collect::<Vec<_>>(), ["newest", "running"]);
        // Running records stay even beyond the limit.
        assert_eq!(s.retain(0, &running), ["newest"]);
        let directory = s.directory.clone();
        drop(s);
        assert_eq!(
            on_disk(&directory).into_keys().collect::<Vec<_>>(),
            ["running"]
        );
        fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn prune_needs_a_criterion_and_combines_them() {
        let day = 86_400_000;
        let records: BTreeMap<_, _> = [("a", 1), ("b", 10), ("c", 20), ("d", 30), ("live", 0)]
            .map(|(id, d)| (id.to_string(), record(id, d * day)))
            .into();
        let running = BTreeSet::from(["live".to_string()]);
        let now = 31 * day;
        let prune = |p: Prune| p.select(&records, &running, now);
        assert!(prune(Prune::default()).is_empty());
        assert_eq!(
            prune(Prune {
                older_than: Some(15 * day),
                ..Default::default()
            }),
            ["a", "b"]
        );
        assert_eq!(
            prune(Prune {
                before: Some(20 * day),
                ..Default::default()
            }),
            ["a", "b"]
        );
        assert_eq!(
            prune(Prune {
                keep: Some(2),
                ..Default::default()
            }),
            ["a", "b"]
        );
        assert_eq!(
            prune(Prune {
                keep: Some(1),
                older_than: Some(25 * day),
                ..Default::default()
            }),
            ["a"]
        );
        assert_eq!(
            prune(Prune {
                keep: Some(0),
                ..Default::default()
            }),
            ["a", "b", "c", "d"]
        );
    }
    #[test]
    fn records_resolve_by_any_of_their_ids_and_never_by_name() {
        let (mut s, root) = store();
        let mut line = record("aa-s1", 1);
        line.agent_name = "morb".into();
        line.ran_as("ab-s2");
        line.ran_as("b-s3");
        line.ran_as("b-s3");
        line.variables.insert("session_id".into(), "c819".into());
        s.write(line);
        let mut other = record("ac-s9", 2);
        other.agent_name = "morb".into();
        s.write(other);
        assert_eq!(s.records["aa-s1"].sessions, ["aa-s1", "ab-s2", "b-s3"]);
        assert_eq!(s.records["aa-s1"].latest(), "b-s3");
        for target in ["aa-s1", "ab-s2", "b-s3", "c819", "b", "aa"] {
            assert_eq!(s.resolve(target).unwrap().id, "aa-s1", "{target}");
        }
        assert_eq!(s.resolve("ac").unwrap().id, "ac-s9");
        assert!(s.resolve("a").unwrap_err().contains("2 records"));
        assert!(
            s.resolve("morb")
                .unwrap_err()
                .contains("no recorded goblin")
        );
        // A long-lived line keeps the newest goblin ids.
        let mut long = record("x", 1);
        for i in 0..MAX_SESSIONS + 5 {
            long.ran_as(&format!("x-s{i}"));
        }
        assert_eq!(long.sessions.len(), MAX_SESSIONS);
        assert_eq!(long.latest(), format!("x-s{}", MAX_SESSIONS + 4));
        drop(s);
        fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn skip_reasons_are_one_bounded_line_and_round_trip() {
        let skipped = Skipped::new(
            "hello",
            &format!("error:\n\u{1b}[31mbroken {}", "é".repeat(200)),
        );
        assert!(skipped.reason.starts_with("error: \\u001b[31mbroken é"));
        assert!(skipped.reason.len() <= 256);
        assert!(!skipped.reason.chars().any(char::is_control));
        let mut r = record("a", 1);
        r.skipped = vec![skipped, Skipped::new("docker", "no Docker in scope 'work'")];
        let root = unix::temp_directory().unwrap();
        apply(&root, vec![Op::Write(Box::new(r.clone()))], sync);
        assert_eq!(on_disk(&root)["a"], r);
        assert_eq!(
            serde_json::to_value(&r.skipped[1]).unwrap(),
            serde_json::json!({"name":"docker","reason":"no Docker in scope 'work'"})
        );
        fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn record_ids_are_short_free_and_unlike_goblin_ids() {
        let id = new_id(|_| false).unwrap();
        assert_eq!(id.len(), 8);
        assert!(goblins_protocol::identifier(&id));
        assert!(
            id.bytes()
                .all(|b| b"0123456789abcdefghjkmnpqrstvwxyz".contains(&b))
        );
        let taken = std::cell::Cell::new(0);
        let second = new_id(|candidate| {
            taken.set(taken.get() + 1);
            candidate == id || taken.get() < 3
        })
        .unwrap();
        assert_ne!(second, id);
        assert!(new_id(|_| true).is_err());
    }
    #[test]
    fn ages_and_dates() {
        assert_eq!(parse_age("30d"), Ok(30 * 86_400_000));
        assert_eq!(parse_age("2w"), Ok(14 * 86_400_000));
        assert_eq!(parse_age("12h"), Ok(12 * 3_600_000));
        for bad in ["", "d", "30", "-1d", "1.5d", "30m", "99999999999999999999d"] {
            assert!(parse_age(bad).is_err(), "{bad}");
        }
        let first = parse_date("2026-07-01").unwrap();
        let second = parse_date("2026-07-02").unwrap();
        assert_eq!(second - first, 86_400_000);
        assert!(local_time(first).starts_with("2026-07-01 00:00"));
        for bad in ["2026-02-30", "2026-13-01", "26-01-01", "2026-1-01", "today"] {
            assert!(parse_date(bad).is_err(), "{bad}");
        }
    }
}
