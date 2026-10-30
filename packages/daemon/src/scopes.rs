//! Daemon-owned scope child registry. All operations run on its main-loop
//! thread; manifest resolution and namespace setup remain off the event pump.
use crate::Result;
use goblins_runtime::{
    Fd,
    channel::{Channel, Descriptor},
    scope::{self, Message},
};
use goblins_unix as unix;
use std::{
    collections::{BTreeMap, VecDeque},
    io,
    os::{
        fd::{AsRawFd, OwnedFd},
        unix::{net::UnixStream, process::CommandExt},
    },
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
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
    executable: PathBuf,
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
        let (daemon, child) = UnixStream::pair()?;
        let fd = child.as_raw_fd();
        let mut command = Command::new(executable);
        command
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        unsafe {
            command.pre_exec(move || {
                if fd != scope::CONTROL_FD {
                    unix::cvt(libc::dup2(fd, scope::CONTROL_FD))?;
                }
                unix::cvt(libc::fcntl(scope::CONTROL_FD, libc::F_SETFD, 0))?;
                unix::cvt(libc::syscall(libc::SYS_close_range, 4u32, u32::MAX, 4u32) as i32)?;
                Ok(())
            });
        }
        let mut child = command.spawn()?;
        drop(command);
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
            let _ = child.wait();
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
        if self.channel.closed() {
            self.stopping = true;
        }
        if self.channel.flush().is_err() {
            self.stopping = true;
        }
        Ok(self.child.try_wait()?.is_none())
    }
}
impl Scopes {
    #[cfg(test)]
    pub(crate) fn insert_fake_process(
        &mut self,
        name: &str,
        child: Child,
        stream: UnixStream,
        added: u64,
    ) {
        self.entries.insert(
            name.into(),
            Entry {
                process: Some(Process {
                    child,
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
                !p.stopping && !p.channel.closed() && matches!(p.child.try_wait(), Ok(None))
            })
    }
    pub fn membership(
        &mut self,
        state: &Path,
        name: &str,
        executable: Option<&Path>,
    ) -> Result<Fd> {
        if self.shutting_down {
            return Err("daemon is stopping".into());
        }
        if !goblins_protocol::scope_name(name) {
            return Err("invalid scope name".into());
        }
        if executable.is_none() && !self.live(name) {
            return Err("the owner's scope has stopped".into());
        }
        let entry = self.entries.entry(name.into()).or_default();
        if entry.pending.len() >= MAX_PENDING {
            return Err("scope membership queue is full".into());
        }
        let (runtime, member) = UnixStream::pair()?;
        let program = executable.map(Path::to_path_buf).unwrap_or_default();
        if entry.process.is_none() && executable.is_some() {
            entry.process = Process::start(state, name, &program)?;
        }
        entry.pending.push_back(Pending {
            fd: member.into(),
            executable: program,
        });
        // Count newly accepted AddMember before processing any Idle report;
        // this makes crossing an older Idle safe even if output is blocked.
        entry.send_pending()?;
        Ok(OwnedFd::from(runtime).into())
    }
    /// Workers perform orderly Leave while stopping. Their acknowledgements
    /// depend on Idle/Stop reports, so keep pumping before joining any worker.
    pub fn shutdown(&mut self, state: &Path) {
        self.shutting_down = true;
        for entry in self.entries.values_mut() {
            entry.pending.clear();
        }
        let deadline = std::time::Instant::now() + scope::RELEASE_TIMEOUT;
        while !self.entries.is_empty() && std::time::Instant::now() < deadline {
            self.tick(state);
            if !self.entries.is_empty() {
                std::thread::sleep(std::time::Duration::from_millis(5));
            }
        }
        // A scope that cannot finish (for example SIGSTOP) must not hang daemon
        // teardown. Kill only after the orderly release budget, as a failure
        // fallback; normal engine shutdown retains its membership until done.
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
                // Queue newer memberships first so Idle cannot stop a scope
                // whose next lease is waiting in a bounded output queue.
                entry.send_pending()?;
                if let Some(process) = &mut entry.process {
                    if !process.tick()? {
                        entry.process = None;
                    }
                }
                if !self.shutting_down && entry.process.is_none() {
                    if let Some(pending) = entry.pending.front() {
                        entry.process = Process::start(state, name, &pending.executable)?;
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
                }
            }
            entry.process.is_some() || !entry.pending.is_empty()
        });
    }
}
impl Entry {
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
                        executable: pending.executable,
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
                let _ = p.child.wait();
            }
        }
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn idle_crossing_a_new_add_member_does_not_stop_scope() {
        // Exercise the real control channel/counter decision without namespace
        // setup: a short-lived child is only a reapable Process fixture.
        let (daemon, peer) = UnixStream::pair().unwrap();
        let child = Command::new("true").spawn().unwrap();
        let mut process = Process {
            child,
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
        process.child.wait().unwrap();
    }
}
