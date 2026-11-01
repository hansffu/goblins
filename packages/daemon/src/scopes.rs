//! Daemon-owned scope child registry. All operations run on its main-loop
//! thread; manifest resolution and namespace setup remain off the event pump.
use crate::{Result, supervised::Child};
use goblins_runtime::{
    Fd,
    channel::{Channel, Descriptor},
    scope::{self, Message},
};
use goblins_unix as unix;
use std::{
    collections::{BTreeMap, VecDeque},
    io,
    os::{fd::OwnedFd, unix::net::UnixStream},
    path::Path,
    time::Instant,
};

const MAX_PENDING: usize = 64;
struct Process {
    child: Child,
    channel: Channel,
    added: u64,
    stopping: bool,
}
struct Pending {
    fd: OwnedFd,
    release_deadline: Instant,
}
#[derive(Default)]
struct Entry {
    process: Option<Process>,
    pending: VecDeque<Pending>,
}
#[derive(Default)]
pub(crate) struct Scopes {
    entries: BTreeMap<String, Entry>,
    shutting_down: bool,
    executable: crate::supervised::Executable,
}
fn queue(channel: &mut Channel, message: Message, mut fds: Vec<OwnedFd>) -> Result<()> {
    channel.queue(&scope::encode(&message), &mut fds)?;
    Ok(())
}
impl Process {
    fn start(state: &Path, name: &str, executable: &Path) -> Result<Option<Self>> {
        let root = state.join("scopes");
        unix::private_directory(&root)?;
        let directory = root.join(name);
        if !scope::remove_stale(&directory, false)? {
            return Ok(None);
        }
        let (child, daemon) = Child::start(executable)?;
        let mut channel = Channel::new(daemon)?;
        if let Err(error) = queue(
            &mut channel,
            Message::Setup {
                api: scope::API,
                name: name.into(),
                directory,
            },
            vec![],
        ) {
            let _ = child.kill();
            drop(child);
            return Err(error);
        }
        Ok(Some(Self {
            child,
            channel,
            added: 0,
            stopping: false,
        }))
    }
    fn idle(&mut self, count: u64) -> Result<()> {
        if !self.stopping && count == self.added {
            queue(&mut self.channel, Message::Stop {}, vec![])?;
            self.stopping = true;
        }
        Ok(())
    }
    fn tick(&mut self) -> Result<bool> {
        for _ in 0..32 {
            match self.channel.receive() {
                Ok(Some(frame)) => match scope::decode(frame.value)? {
                    Message::Idle { added } => self.idle(added)?,
                    Message::Stopping {} => self.stopping = true,
                    Message::Error { message } => {
                        self.channel.close();
                        self.stopping = true;
                        eprintln!("scope process: {message}");
                    }
                    _ => return Err("invalid scope process report".into()),
                },
                Ok(None) => break,
                Err(error) => {
                    self.stopping = true;
                    eprintln!("scope channel: {error}");
                    break;
                }
            }
        }
        if self.channel.closed() || self.channel.flush().is_err() {
            self.stopping = true;
            let _ = self.child.kill();
        }
        Ok(!self.child.reap()?)
    }
}
impl Scopes {
    pub(crate) fn new(executable: crate::supervised::Executable) -> Self {
        Self {
            entries: BTreeMap::new(),
            shutting_down: false,
            executable,
        }
    }
    #[cfg(test)]
    pub(crate) fn insert_fake_process(
        &mut self,
        name: &str,
        child: std::process::Child,
        stream: UnixStream,
        added: u64,
    ) {
        self.entries.insert(
            name.into(),
            Entry {
                process: Some(Process {
                    child: Child::fixture(child),
                    channel: Channel::new(stream).unwrap(),
                    added,
                    stopping: false,
                }),
                pending: VecDeque::new(),
            },
        );
    }
    /// An owner's membership must already have a live, non-stopping entry.
    pub fn live(&mut self, name: &str) -> bool {
        self.entries
            .get_mut(name)
            .and_then(|entry| entry.process.as_mut())
            .is_some_and(|p| {
                !p.stopping && !p.channel.closed() && matches!(p.child.exited(), Ok(false))
            })
    }
    pub fn membership(&mut self, state: &Path, name: &str, allow_start: bool) -> Result<Fd> {
        if self.shutting_down {
            return Err("daemon is stopping".into());
        }
        if !goblins_protocol::scope_name(name) {
            return Err("invalid scope name".into());
        }
        if !allow_start && !self.live(name) {
            return Err("the owner's scope has stopped".into());
        }
        let program = if allow_start {
            Some(self.executable.path()?)
        } else {
            None
        };
        let entry = self.entries.entry(name.into()).or_default();
        if entry.pending.len() >= MAX_PENDING {
            return Err("scope membership queue is full".into());
        }
        let (runtime, member) = UnixStream::pair()?;
        if entry.process.is_none()
            && let Some(program) = program
        {
            entry.process = Process::start(state, name, program)?;
        }
        entry.pending.push_back(Pending {
            fd: member.into(),
            release_deadline: Instant::now() + scope::RELEASE_TIMEOUT,
        });
        // Count newly accepted AddMember before processing any Idle report;
        // this makes crossing an older Idle safe even if output is blocked.
        entry.send_pending()?;
        Ok(OwnedFd::from(runtime).into())
    }
    /// Workers perform orderly Leave while stopping. Their acknowledgements
    /// depend on Idle/Stop reports, so keep pumping before joining any worker.
    pub fn shutdown(&mut self, state: &Path) {
        self.begin_shutdown();
        let deadline = std::time::Instant::now() + scope::RELEASE_TIMEOUT;
        while !self.entries.is_empty() && std::time::Instant::now() < deadline {
            self.tick(state);
            if !self.entries.is_empty() {
                std::thread::sleep(std::time::Duration::from_millis(5));
            }
        }
        self.abort();
    }
    pub fn begin_shutdown(&mut self) {
        self.shutting_down = true;
        for entry in self.entries.values_mut() {
            entry.pending.clear();
        }
    }
    pub fn drained(&self) -> bool {
        self.entries.is_empty()
    }
    pub fn abort(&mut self) {
        for entry in self.entries.values_mut() {
            if let Some(process) = &mut entry.process {
                process.channel.close();
                let _ = process.child.kill();
            }
        }
    }
    pub fn tick(&mut self, state: &Path) {
        self.entries.retain(|name, entry| {
            let result = (|| -> Result<()> {
                if entry.process.as_ref().is_none_or(|p| p.stopping) {
                    entry.expire_waiters(Instant::now());
                }
                // Queue newer memberships first so Idle cannot stop a scope
                // whose next lease is waiting in a bounded output queue.
                entry.send_pending()?;
                if let Some(process) = &mut entry.process {
                    if !process.tick()? {
                        entry.process = None;
                    }
                }
                if !self.shutting_down && entry.process.is_none() {
                    if !entry.pending.is_empty() {
                        entry.process = Process::start(state, name, self.executable.path()?)?;
                        entry.send_pending()?;
                    }
                }
                Ok(())
            })();
            if let Err(error) = result {
                eprintln!("scope '{name}': {error}");
                entry.pending.clear();
                if let Some(process) = &mut entry.process {
                    process.stopping = true;
                    process.channel.close();
                    let _ = process.child.kill();
                }
            }
            entry.process.is_some() || !entry.pending.is_empty()
        });
    }
}
impl Entry {
    fn expire_waiters(&mut self, now: Instant) {
        while self
            .pending
            .front()
            .is_some_and(|p| now >= p.release_deadline)
        {
            let pending = self.pending.pop_front().unwrap();
            // This endpoint has never sent data. A small nonblocking refusal
            // fits its empty output queue; closing it remains the EOF fallback.
            if let Ok(mut channel) = Channel::new(UnixStream::from(pending.fd)) {
                let _ = queue(
                    &mut channel,
                    Message::Error {
                        message: "scope is still stopping after the 15-second release budget"
                            .into(),
                    },
                    vec![],
                );
                let _ = channel.flush();
            }
        }
    }
    fn send_pending(&mut self) -> Result<()> {
        let Some(process) = &mut self.process else {
            return Ok(());
        };
        if process.stopping {
            return Ok(());
        }
        while let Some(pending) = self.pending.pop_front() {
            let mut fds = vec![pending.fd];
            match process.channel.queue(
                &scope::encode(&Message::AddMember {
                    fd: Descriptor::new(0),
                }),
                &mut fds,
            ) {
                Ok(()) => {
                    process.added = process
                        .added
                        .checked_add(1)
                        .ok_or("scope membership counter exhausted")?
                }
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                    self.pending.push_front(Pending {
                        fd: fds.pop().unwrap(),
                        release_deadline: pending.release_deadline,
                    });
                    break;
                }
                Err(error) => return Err(error.into()),
            }
        }
        Ok(())
    }
}
impl Drop for Scopes {
    fn drop(&mut self) {
        for entry in self.entries.values_mut() {
            if let Some(p) = &mut entry.process {
                p.channel.close();
            }
        }
        for entry in self.entries.values_mut() {
            if let Some(p) = &mut entry.process {
                let _ = p.child.kill();
            }
        }
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command;
    #[test]
    fn a_guardian_held_old_runtime_fails_a_new_start_at_the_release_deadline() {
        let _no_forks = unix::no_forks();
        let state = unix::temp_directory().unwrap();
        unix::private_directory(&state.join("scopes")).unwrap();
        let runtime = state.join("scopes/held");
        unix::private_directory(&runtime).unwrap();
        std::fs::create_dir(runtime.join("store")).unwrap();
        std::fs::write(runtime.join("store/marker"), "old engine").unwrap();
        let guardian = goblins_runtime::lease::flock(&runtime.join("lock"), false)
            .unwrap()
            .unwrap();
        // This new registry has no child for the previous daemon's scope.
        // The executable must never run while its guardian still holds lock.
        let mut scopes = Scopes::default();
        scopes.executable = crate::supervised::Executable::from_path(Some("/must-not-run".into()));
        let fd = scopes.membership(&state, "held", true).unwrap();
        let mut client = Channel::new(UnixStream::from(OwnedFd::from(fd))).unwrap();
        scopes.tick(&state);
        assert!(scopes.entries["held"].process.is_none());
        assert_eq!(scopes.entries["held"].pending.len(), 1);
        assert!(client.receive().unwrap().is_none());
        assert_eq!(
            std::fs::read_to_string(runtime.join("store/marker")).unwrap(),
            "old engine"
        );
        // Advance the production deadline directly; no 15-second test sleep.
        scopes
            .entries
            .get_mut("held")
            .unwrap()
            .pending
            .front_mut()
            .unwrap()
            .release_deadline = Instant::now();
        scopes.tick(&state);
        assert!(!scopes.entries.contains_key("held"));
        let Message::Error { message } =
            scope::decode(client.receive().unwrap().unwrap().value).unwrap()
        else {
            panic!("missing bounded-start refusal");
        };
        assert!(message.contains("15-second release budget"));
        assert!(!scope::remove_stale(&runtime, false).unwrap());
        assert!(runtime.join("store/marker").exists());
        drop(guardian);
        assert!(scope::remove_stale(&runtime, false).unwrap());
        assert!(!runtime.exists());
        std::fs::remove_dir_all(state).unwrap();
    }
    #[test]
    fn idle_crossing_a_new_add_member_does_not_stop_scope() {
        let _forking = unix::forking();
        // Exercise the real control channel/counter decision without namespace
        // setup: a short-lived child is only a reapable Process fixture.
        let (daemon, peer) = UnixStream::pair().unwrap();
        let child = Command::new("true").spawn().unwrap();
        let mut process = Process {
            child: Child::fixture(child),
            channel: Channel::new(daemon).unwrap(),
            added: 2,
            stopping: false,
        };
        process.idle(1).unwrap();
        assert!(!process.stopping);
        assert!(!process.channel.wants_write());
        process.idle(2).unwrap();
        assert!(process.stopping);
        process.channel.flush().unwrap();
        let mut peer = Channel::new(peer).unwrap();
        assert!(matches!(
            scope::decode(peer.receive().unwrap().unwrap().value).unwrap(),
            Message::Stop {}
        ));
        while !process.child.reap().unwrap() {
            std::thread::yield_now();
        }
    }
}
