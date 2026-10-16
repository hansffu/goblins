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

pub const VERSION: u32 = 1;
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
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Record {
    pub version: u32,
    pub id: String,
    /// The configuration name, launched from the current manifest.
    pub configuration: String,
    pub agent_name: String,
    pub cwd: Option<PathBuf>,
    pub scope: Option<String>,
    pub dev_shell: Option<String>,
    pub docker_enabled: bool,
    pub packages: Packages,
    /// Written only by the goblin itself (`sessions.set-variables`).
    pub variables: BTreeMap<String, String>,
    pub recreated_from: Option<String>,
    /// Approved names that did not resolve when this goblin was recreated.
    pub skipped: Vec<String>,
    /// Unix milliseconds.
    pub created: u64,
    pub updated: u64,
    /// Informational: a goblin that died with its daemon never gets it.
    pub stopped: Option<u64>,
}
impl Record {
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
            recreated_from: None,
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
        if next.packages.approved.len() > MAX_NAMES || next.packages.denied.len() > MAX_NAMES {
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
        // As written: pretty-printed, with a final newline.
        if serde_json::to_vec_pretty(&next).map_or(true, |v| v.len() + 1 > MAX_RECORD) {
            return Err(format!("the record would exceed {} KiB", MAX_RECORD / 1024));
        }
        *self = next;
        Ok(true)
    }
    /// A grant that reached `ready`. Its latest final outcome decides the
    /// list, so a denied name moves to `approved`.
    pub fn approve(&mut self, name: &str) -> std::result::Result<bool, String> {
        self.update(|r| {
            r.packages.denied.retain(|n| n != name);
            if !r.packages.approved.iter().any(|n| n == name) {
                r.packages.approved.push(name.into());
            }
        })
    }
    /// An explicit host denial.
    pub fn deny(&mut self, name: &str) -> std::result::Result<bool, String> {
        self.update(|r| {
            r.packages.approved.retain(|n| n != name);
            if !r.packages.denied.iter().any(|n| n == name) {
                r.packages.denied.push(name.into());
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

enum Op {
    Write(Box<Record>),
    Delete(Vec<String>),
}
/// The history store this process owns: its lock, the records and the writer.
pub struct Store {
    pub directory: PathBuf,
    pub records: BTreeMap<String, Record>,
    commands: Option<mpsc::Sender<Op>>,
    thread: Option<JoinHandle<()>>,
    _lock: File,
}
impl Store {
    /// `$XDG_STATE_HOME/goblins/sessions`, resolved even before it exists.
    pub fn default_directory() -> Result<PathBuf> {
        unix::state_directory("sessions", false)
    }
    /// Take the store's lock and read its records. Fails when another
    /// process owns the store.
    pub fn open(directory: &Path) -> Result<Self> {
        unix::private_directory(directory)?;
        let directory = fs::canonicalize(directory)?;
        let lock = unix::lock(&directory.join(".lock"))
            .map_err(|_| format!("another Goblins server owns {}", directory.display()))?;
        let records = load(&directory)?;
        let (commands, received) = mpsc::channel();
        let target = directory.clone();
        let thread = thread::Builder::new()
            .name("goblins-history".into())
            .spawn(move || {
                while let Ok(op) = received.recv() {
                    let mut batch = vec![op];
                    batch.extend(received.try_iter());
                    apply(&target, batch);
                }
            })?;
        Ok(Self {
            directory,
            records,
            commands: Some(commands),
            thread: Some(thread),
            _lock: lock,
        })
    }
    /// Replace a record, in memory now and on disk soon.
    pub fn write(&mut self, record: Record) {
        self.records.insert(record.id.clone(), record.clone());
        self.send(Op::Write(Box::new(record)));
    }
    /// Delete records, dropping any revision of them still queued.
    pub fn delete(&mut self, ids: &[String]) {
        if ids.is_empty() {
            return;
        }
        for id in ids {
            self.records.remove(id);
        }
        self.send(Op::Delete(ids.to_vec()));
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
        let excess = self.records.len().saturating_sub(keep);
        let mut old: Vec<_> = self
            .records
            .values()
            .filter(|r| !running.contains(&r.id))
            .map(|r| (r.updated, r.id.clone()))
            .collect();
        old.sort();
        let ids: Vec<_> = old.into_iter().take(excess).map(|(_, id)| id).collect();
        self.delete(&ids);
        ids
    }
    /// A record by id, unique id prefix or unique agent name.
    pub fn resolve(&self, target: &str) -> std::result::Result<&Record, String> {
        if target.is_empty() {
            return Err("name a recorded goblin by id or agent name".into());
        }
        if let Some(record) = self.records.get(target) {
            return Ok(record);
        }
        fn unique<'a>(
            matches: Vec<&'a Record>,
            what: &str,
            target: &str,
        ) -> std::result::Result<Option<&'a Record>, String> {
            match matches.len() {
                0 => Ok(None),
                1 => Ok(Some(matches[0])),
                n => Err(format!(
                    "{n} records {what} '{}'; use a record id from 'goblins history'",
                    goblins_protocol::plain_text(target)
                )),
            }
        }
        let prefixed = self
            .records
            .values()
            .filter(|r| r.id.starts_with(target))
            .collect();
        if let Some(record) = unique(prefixed, "have ids starting with", target)? {
            return Ok(record);
        }
        let named = self
            .records
            .values()
            .filter(|r| r.agent_name == target)
            .collect();
        unique(named, "are named", target)?.ok_or_else(|| {
            format!(
                "no recorded goblin '{}'",
                goblins_protocol::plain_text(target)
            )
        })
    }
    /// The records `id` was recreated from, nearest first.
    pub fn ancestors(&self, id: &str) -> Vec<String> {
        let mut seen = BTreeSet::from([id.to_string()]);
        let mut chain = vec![];
        let mut current = self.records.get(id).and_then(|r| r.recreated_from.clone());
        while let Some(parent) = current {
            if !seen.insert(parent.clone()) {
                break;
            }
            current = self
                .records
                .get(&parent)
                .and_then(|r| r.recreated_from.clone());
            chain.push(parent);
        }
        chain
    }
    /// The records recreated from `id`, directly or through others.
    pub fn descendants(&self, id: &str) -> Vec<String> {
        self.records
            .keys()
            .filter(|other| self.ancestors(other).iter().any(|a| a == id))
            .cloned()
            .collect()
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
fn apply(directory: &Path, batch: Vec<Op>) {
    let mut pending = BTreeMap::new();
    for op in batch {
        match op {
            Op::Write(record) => {
                pending.insert(record.id.clone(), record);
            }
            Op::Delete(ids) => {
                for id in ids {
                    pending.remove(&id);
                    match fs::remove_file(path(directory, &id)) {
                        Ok(()) => (),
                        Err(e) if e.kind() == io::ErrorKind::NotFound => (),
                        Err(e) => eprintln!("goblins: cannot delete session record {id}: {e}"),
                    }
                }
            }
        }
    }
    for record in pending.values() {
        if let Err(e) = write(directory, record) {
            eprintln!("goblins: cannot write session record {}: {e}", record.id);
        }
    }
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
            // A write interrupted by a crash; the record before it remains.
            let _ = fs::remove_file(entry.path());
            continue;
        }
        let Some(id) = name.strip_suffix(".json") else {
            continue;
        };
        if !goblins_protocol::identifier(id) {
            continue;
        }
        match read(&entry.path()) {
            Ok(record) if record.id == id && record.version == VERSION => {
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
    fn record(id: &str, updated: u64) -> Record {
        let mut r = Record::new(id, "shell", "snikk", updated);
        r.updated = updated;
        r
    }
    fn on_disk(directory: &Path) -> BTreeMap<String, Record> {
        load(directory).unwrap()
    }
    fn store() -> (Store, PathBuf) {
        let root = unix::temp_directory().unwrap();
        let directory = root.join("sessions");
        (Store::open(&directory).unwrap(), root)
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
        let s = Store::open(&directory).unwrap();
        assert_eq!(s.records.len(), 1);
        assert_eq!(s.records["a"], a);
        assert!(!directory.join(".tmp-a.json").exists());
        // One owner at a time.
        assert!(
            Store::open(&directory)
                .err()
                .unwrap()
                .to_string()
                .contains("another Goblins server")
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
        );
        assert_eq!(on_disk(&root)["a"], second);
        // A prune after a queued write: neither revision is left.
        first.updated = 3;
        apply(
            &root,
            vec![
                Op::Write(Box::new(first.clone())),
                Op::Delete(vec!["a".into()]),
                Op::Write(Box::new(record("b", 4))),
            ],
        );
        assert_eq!(on_disk(&root).into_keys().collect::<Vec<_>>(), ["b"]);
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
    fn records_resolve_by_id_prefix_or_unique_name_and_know_their_lineage() {
        let (mut s, root) = store();
        let mut first = record("aa-s1", 1);
        first.agent_name = "morb".into();
        s.write(first);
        let mut second = record("ab-s2", 2);
        second.agent_name = "grib".into();
        second.recreated_from = Some("aa-s1".into());
        s.write(second);
        let mut third = record("b-s3", 3);
        third.agent_name = "grib".into();
        third.recreated_from = Some("ab-s2".into());
        s.write(third);
        assert_eq!(s.resolve("aa-s1").unwrap().id, "aa-s1");
        assert_eq!(s.resolve("b").unwrap().id, "b-s3");
        assert_eq!(s.resolve("morb").unwrap().id, "aa-s1");
        assert!(s.resolve("a").unwrap_err().contains("2 records"));
        assert!(
            s.resolve("grib")
                .unwrap_err()
                .contains("2 records are named")
        );
        assert!(s.resolve("zog").unwrap_err().contains("no recorded goblin"));
        assert_eq!(s.ancestors("b-s3"), ["ab-s2", "aa-s1"]);
        assert_eq!(s.descendants("aa-s1"), ["ab-s2", "b-s3"]);
        assert!(s.descendants("b-s3").is_empty());
        drop(s);
        fs::remove_dir_all(root).unwrap();
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
