//! One runtime's membership lease. EOF lets the scope clean up after a crash.
use crate::{Result, config::Launch, process::Cancellation, scope_keeper::NamespaceFds};
use goblins_runtime::{
    Event, Fd,
    channel::{Channel, Descriptor, Frame},
    scope::{self, Message},
};
use std::{
    collections::BTreeMap,
    fs::File,
    os::{
        fd::{AsRawFd, OwnedFd, RawFd},
        unix::net::UnixStream,
    },
    path::PathBuf,
    sync::{
        Mutex,
        mpsc::{Receiver, SyncSender},
    },
    time::{Duration, Instant},
};

pub(crate) struct Broker {
    pub events: SyncSender<Event>,
    pub scope_failures: SyncSender<String>,
    pub memberships: Receiver<std::result::Result<Fd, String>>,
}
impl Broker {
    pub fn request(&self, launch: &Launch, cancel: &Cancellation) -> Result<Client> {
        let selected = launch.scope.as_ref().ok_or("launch has no scope")?;
        self.events.try_send(Event::ScopeNeeded {
            name: selected.name.clone(),
            executable: launch.scope_runtime.clone(),
        })?;
        loop {
            cancel.check()?;
            match self.memberships.recv_timeout(Duration::from_millis(20)) {
                Ok(Ok(fd)) => return Client::join(fd.into(), launch, cancel),
                Ok(Err(message)) => return Err(message.into()),
                Err(std::sync::mpsc::RecvTimeoutError::Timeout) => (),
                Err(_) => return Err("scope membership request was cancelled".into()),
            }
        }
    }
}
pub(crate) struct Client {
    pub name: String,
    pub definition: goblins_runtime::config::ScopeDefinition,
    pub namespaces: NamespaceFds,
    storage: BTreeMap<String, File>,
    channel: Mutex<Connection>,
}
#[derive(Clone, Copy)]
enum Reply {
    Joined,
    Storage,
    Registered,
    DockerPending,
    DockerAttached,
    DockerDetached,
}
impl Reply {
    fn expected(message: &Message) -> Result<Self> {
        Ok(match message {
            Message::Join(_) => Self::Joined,
            Message::OpenStorage { .. } => Self::Storage,
            Message::RegisterMember { .. } => Self::Registered,
            Message::AttachDocker { more: true, .. } | Message::ExtendDocker { more: true, .. } => {
                Self::DockerPending
            }
            Message::AttachDocker { more: false, .. }
            | Message::ExtendDocker { more: false, .. } => Self::DockerAttached,
            Message::DetachDocker {} => Self::DockerDetached,
            _ => return Err("invalid scope exchange request".into()),
        })
    }
    fn accepts(self, message: &Message) -> bool {
        matches!(
            (self, message),
            (Self::Joined, Message::Joined { .. })
                | (Self::Storage, Message::Storage { .. })
                | (Self::Registered, Message::Registered {})
                | (Self::DockerPending, Message::DockerPending {})
                | (Self::DockerAttached, Message::DockerAttached { .. })
                | (Self::DockerDetached, Message::DockerDetached {})
        )
    }
}
/// Cancellation can interrupt a queued exchange. Its reply still precedes
/// Left on the stream, and must be matched before orderly release can finish.
struct Connection {
    channel: Channel,
    pending: Option<Reply>,
}
impl Connection {
    fn new(stream: UnixStream) -> std::io::Result<Self> {
        Ok(Self {
            channel: Channel::new(stream)?,
            pending: None,
        })
    }
}
impl std::ops::Deref for Connection {
    type Target = Channel;
    fn deref(&self) -> &Channel {
        &self.channel
    }
}
impl std::ops::DerefMut for Connection {
    fn deref_mut(&mut self) -> &mut Channel {
        &mut self.channel
    }
}
fn take(fds: &mut [Option<OwnedFd>], descriptor: Descriptor) -> Result<File> {
    fds.get_mut(descriptor.index)
        .and_then(Option::take)
        .map(File::from)
        .ok_or_else(|| "invalid scope descriptor index".into())
}
fn exchange(
    channel: &mut Connection,
    message: Message,
    mut fds: Vec<OwnedFd>,
    cancel: &Cancellation,
    deadline: Option<Instant>,
) -> Result<Frame> {
    if channel.pending.is_some() {
        return Err("scope exchange already pending".into());
    }
    let expected = Reply::expected(&message)?;
    channel.queue(&scope::encode(&message), &mut fds)?;
    channel.pending = Some(expected);
    loop {
        cancel.check()?;
        if deadline.is_some_and(|d| Instant::now() >= d) {
            return Err("scope registration exceeded helper startup limit".into());
        }
        channel.flush()?;
        if let Some(frame) = channel.receive()? {
            let message = scope::decode(frame.value.clone())?;
            channel.pending = None;
            if let Message::Error { message } = message {
                return Err(message.into());
            }
            if !expected.accepts(&message) {
                return Err("invalid scope exchange reply".into());
            }
            return Ok(frame);
        }
        let fd = channel.fd().ok_or("scope process exited")?;
        let mut poll = libc::pollfd {
            fd,
            events: libc::POLLIN
                | if channel.wants_write() {
                    libc::POLLOUT
                } else {
                    0
                },
            revents: 0,
        };
        unsafe {
            libc::poll(&mut poll, 1, 20);
        }
    }
}
impl Client {
    #[cfg(test)]
    pub(crate) fn fixture(stream: UnixStream) -> Self {
        Self {
            name: "failed".into(),
            definition: goblins_runtime::config::ScopeDefinition {
                persistent: false,
                pid: false,
                docker: true,
                storage: vec![],
            },
            namespaces: NamespaceFds {
                outer: File::open("/dev/null").unwrap(),
                user: File::open("/dev/null").unwrap(),
                net: File::open("/dev/null").unwrap(),
                pid: None,
                net_path: PathBuf::new(),
            },
            storage: BTreeMap::new(),
            channel: Mutex::new(Connection::new(stream).unwrap()),
        }
    }
    pub fn join(fd: OwnedFd, launch: &Launch, cancel: &Cancellation) -> Result<Self> {
        let selected = launch.scope.as_ref().ok_or("launch has no scope")?;
        let mut channel = Connection::new(UnixStream::from(fd))?;
        let frame = exchange(
            &mut channel,
            Message::Join(scope::Join {
                definition: selected.definition.clone(),
                bwrap: launch.bwrap.clone(),
                helper: launch.helper.clone(),
                pasta: launch.pasta.clone(),
                allow_nix: launch.allow_nix,
            }),
            vec![],
            cancel,
            None,
        )?;
        let Message::Joined {
            outer,
            user,
            net,
            pid,
            directory: _,
        } = scope::decode(frame.value)?
        else {
            return Err("invalid scope Join reply".into());
        };
        let mut fds: Vec<_> = frame.fds.into_iter().map(Some).collect();
        let namespaces = NamespaceFds {
            outer: take(&mut fds, outer)?,
            user: take(&mut fds, user)?,
            net: take(&mut fds, net)?,
            pid: pid.map(|fd| take(&mut fds, fd)).transpose()?,
            net_path: PathBuf::new(),
        };
        let names: Vec<_> = selected.mounts.keys().cloned().collect();
        let mut storage = BTreeMap::new();
        for batch in names.chunks(goblins_runtime::channel::MAX_FDS) {
            let frame = exchange(
                &mut channel,
                Message::OpenStorage {
                    names: batch.to_vec(),
                },
                vec![],
                cancel,
                None,
            )?;
            let Message::Storage { directories } = scope::decode(frame.value)? else {
                return Err("invalid scope storage reply".into());
            };
            if directories.len() != batch.len()
                || batch.iter().any(|name| !directories.contains_key(name))
            {
                return Err("scope storage reply names do not match".into());
            }
            let mut fds: Vec<_> = frame.fds.into_iter().map(Some).collect();
            for (name, descriptor) in directories {
                storage.insert(name, take(&mut fds, descriptor)?);
            }
        }
        Ok(Self {
            name: selected.name.clone(),
            definition: selected.definition.clone(),
            namespaces,
            storage,
            channel: Mutex::new(channel),
        })
    }
    pub fn fds(&self) -> (RawFd, RawFd, Option<RawFd>) {
        (
            self.namespaces.user.as_raw_fd(),
            self.namespaces.net.as_raw_fd(),
            self.namespaces.pid.as_ref().map(AsRawFd::as_raw_fd),
        )
    }
    pub fn storage(&self, name: &str) -> Result<&File> {
        self.storage
            .get(name)
            .ok_or_else(|| format!("scope has no storage named '{name}'").into())
    }
    pub fn register(
        &self,
        namespace: File,
        cancel: &Cancellation,
        deadline: Instant,
    ) -> Result<()> {
        let mut channel = self.channel.lock().map_err(|_| "scope channel poisoned")?;
        let frame = exchange(
            &mut channel,
            Message::RegisterMember {
                mountns: Descriptor::new(0),
            },
            vec![namespace.into()],
            cancel,
            Some(deadline),
        )?;
        if !matches!(scope::decode(frame.value)?, Message::Registered {}) {
            return Err("invalid scope registration acknowledgement".into());
        }
        Ok(())
    }
    pub fn docker(
        &self,
        config: Option<scope::DockerConfig>,
        parts: Vec<crate::scope_docker::Part>,
        cancel: &Cancellation,
    ) -> Result<PathBuf> {
        let extending = config.is_none();
        let mut config = config;
        let mut parts = parts.into_iter().peekable();
        let mut channel = self.channel.lock().map_err(|_| "scope channel poisoned")?;
        loop {
            let mut fds = vec![];
            let batch = parts
                .by_ref()
                .take(goblins_runtime::channel::MAX_FDS)
                .map(|part| part.encode(&mut fds))
                .collect();
            let more = parts.peek().is_some();
            let message = if extending {
                Message::ExtendDocker { parts: batch, more }
            } else {
                Message::AttachDocker {
                    config: config.take(),
                    parts: batch,
                    more,
                }
            };
            let frame = exchange(&mut channel, message, fds, cancel, None)?;
            match scope::decode(frame.value)? {
                Message::DockerPending {} if more => (),
                Message::DockerAttached { socket } if !more => return Ok(socket),
                _ => return Err("invalid scope Docker acknowledgement".into()),
            }
        }
    }
    pub fn detach_docker(&self, cancel: &Cancellation) -> Result<()> {
        let mut channel = self.channel.lock().map_err(|_| "scope channel poisoned")?;
        let frame = exchange(&mut channel, Message::DetachDocker {}, vec![], cancel, None)?;
        if !matches!(scope::decode(frame.value)?, Message::DockerDetached {}) {
            return Err("invalid Docker detach acknowledgement".into());
        }
        Ok(())
    }
    pub fn begin_leave(&self) -> Result<()> {
        let mut channel = self.channel.lock().map_err(|_| "scope channel poisoned")?;
        channel.queue(&scope::encode(&Message::Leave {}), &mut vec![])?;
        channel.flush()?;
        Ok(())
    }
    pub fn finish_leave(&self, deadline: Instant) -> Result<()> {
        let mut channel = self.channel.lock().map_err(|_| "scope channel poisoned")?;
        loop {
            channel.flush()?;
            if let Some(frame) = channel.receive()? {
                let message = scope::decode(frame.value)?;
                if let Some(expected) = channel.pending.take() {
                    if !expected.accepts(&message) && !matches!(message, Message::Error { .. }) {
                        return Err("invalid interrupted scope exchange reply".into());
                    }
                    continue;
                }
                if !matches!(message, Message::Left {}) {
                    return Err("invalid scope Leave acknowledgement".into());
                }
                return Ok(());
            }
            if channel.closed() {
                return Err("scope exited without Leave acknowledgement".into());
            }
            if Instant::now() >= deadline {
                channel.close();
                return Err("scope release exceeded the stop deadline".into());
            }
            let fd = channel.fd().unwrap();
            let mut poll = libc::pollfd {
                fd,
                events: libc::POLLIN
                    | if channel.wants_write() {
                        libc::POLLOUT
                    } else {
                        0
                    },
                revents: 0,
            };
            unsafe {
                libc::poll(&mut poll, 1, 5);
            }
        }
    }
    pub fn alive(&self) -> bool {
        let Ok(channel) = self.channel.try_lock() else {
            return true;
        };
        let Some(fd) = channel.fd() else {
            return false;
        };
        // A cancelled exchange can leave its normal reply readable. Detect
        // actual peer shutdown even when those reply bytes precede EOF.
        let death = libc::POLLRDHUP | libc::POLLHUP | libc::POLLERR | libc::POLLNVAL;
        let mut poll = libc::pollfd {
            fd,
            events: death,
            revents: 0,
        };
        if unsafe { libc::poll(&mut poll, 1, 0) } > 0 && poll.revents & death != 0 {
            return false;
        }
        let mut byte = 0u8;
        let n = unsafe {
            libc::recv(
                fd,
                (&mut byte as *mut u8).cast(),
                1,
                libc::MSG_PEEK | libc::MSG_DONTWAIT,
            )
        };
        (n > 0 && channel.pending.is_some())
            || (n < 0
                && matches!(
                    std::io::Error::last_os_error().kind(),
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::Interrupted
                ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use goblins_runtime::config::{Scope, ScopeDefinition};
    fn cancelled_attach() -> (Client, Channel) {
        let (runtime, server) = UnixStream::pair().unwrap();
        let client = Client::fixture(runtime);
        let cancel = Cancellation::default();
        cancel.cancel();
        assert!(
            client
                .docker(
                    Some(scope::DockerConfig {
                        bwrap: "/unused".into(),
                        helper: "/unused".into(),
                        daemon: "/unused".into()
                    }),
                    vec![],
                    &cancel
                )
                .is_err()
        );
        client.begin_leave().unwrap();
        let mut server = Channel::new(server).unwrap();
        assert!(matches!(
            scope::decode(server.receive().unwrap().unwrap().value).unwrap(),
            Message::AttachDocker { more: false, .. }
        ));
        assert!(matches!(
            scope::decode(server.receive().unwrap().unwrap().value).unwrap(),
            Message::Leave {}
        ));
        (client, server)
    }
    #[test]
    fn cancelled_attach_drains_its_reply_then_waits_for_cleanup_left() {
        for reply in [
            Message::DockerAttached {
                socket: "/unused".into(),
            },
            Message::Error {
                message: "startup cancelled".into(),
            },
        ] {
            let directory = goblins_unix::temp_directory().unwrap();
            std::fs::write(directory.join("engine-storage"), "live").unwrap();
            let (client, mut server) = cancelled_attach();
            server.queue(&scope::encode(&reply), &mut vec![]).unwrap();
            server.flush().unwrap();
            std::fs::remove_dir_all(&directory).unwrap();
            server
                .queue(&scope::encode(&Message::Left {}), &mut vec![])
                .unwrap();
            server.flush().unwrap();
            client
                .finish_leave(Instant::now() + scope::RELEASE_TIMEOUT)
                .unwrap();
            assert!(!directory.exists());
            assert!(client.channel.lock().unwrap().pending.is_none());
        }
    }
    #[test]
    fn cancelled_attach_reply_alone_cannot_acknowledge_leave() {
        let (client, mut server) = cancelled_attach();
        server
            .queue(
                &scope::encode(&Message::DockerAttached {
                    socket: "/unused".into(),
                }),
                &mut vec![],
            )
            .unwrap();
        server.flush().unwrap();
        let error = client.finish_leave(Instant::now()).unwrap_err().to_string();
        assert!(
            error.contains("scope release exceeded the stop deadline"),
            "{error}"
        );
        assert!(client.channel.lock().unwrap().closed());
    }
    #[test]
    fn cancelled_attach_does_not_drain_an_unrelated_reply() {
        let (client, mut server) = cancelled_attach();
        server
            .queue(
                &scope::encode(&Message::Storage {
                    directories: BTreeMap::new(),
                }),
                &mut vec![],
            )
            .unwrap();
        server.flush().unwrap();
        let error = client
            .finish_leave(Instant::now() + scope::RELEASE_TIMEOUT)
            .unwrap_err()
            .to_string();
        assert!(
            error.contains("invalid interrupted scope exchange reply"),
            "{error}"
        );
    }
    #[test]
    fn cancelled_attach_pending_reply_is_live_but_buffered_reply_does_not_hide_eof() {
        for reply in [
            Message::DockerAttached {
                socket: "/unused".into(),
            },
            Message::Error {
                message: "startup cancelled".into(),
            },
        ] {
            let (client, mut server) = cancelled_attach();
            server.queue(&scope::encode(&reply), &mut vec![]).unwrap();
            server.flush().unwrap();
            assert!(client.alive(), "a normal pending reply is not scope death");
            assert!(client.channel.lock().unwrap().pending.is_some());
            server.close();
            assert!(
                !client.alive(),
                "buffered pending reply must not hide real scope EOF"
            );
        }
        let (runtime, server) = UnixStream::pair().unwrap();
        let client = Client::fixture(runtime);
        let mut server = Channel::new(server).unwrap();
        server
            .queue(
                &scope::encode(&Message::DockerAttached {
                    socket: "/unsolicited".into(),
                }),
                &mut vec![],
            )
            .unwrap();
        server.flush().unwrap();
        assert!(
            !client.alive(),
            "unsolicited input without a pending reply remains fatal"
        );
    }
    #[test]
    fn docker_batches_transfer_the_entire_donor_chain_without_raw_fd_arguments() {
        use crate::scope_docker::Part;
        use goblins_runtime::{ResourceLease, lease::resource_exclusive};
        let _no_forks = goblins_unix::no_forks();
        let root = goblins_unix::temp_directory().unwrap();
        let owners: Vec<_> = ["ancestor", "donor"]
            .into_iter()
            .map(|name| {
                let path = root.join(name);
                std::fs::create_dir(&path).unwrap();
                std::fs::write(path.join("root"), "retained").unwrap();
                ResourceLease::new(path).unwrap()
            })
            .collect();
        let paths: Vec<_> = owners.iter().map(|l| l.directory().to_path_buf()).collect();
        let count = goblins_runtime::channel::MAX_FDS * 2 + 1;
        let mut parts: Vec<_> = (0..count)
            .map(|i| {
                Part::Bind(
                    File::open("/dev/null").unwrap(),
                    format!("/mount{i}").into(),
                    true,
                )
            })
            .collect();
        parts.extend(owners.iter().map(|l| Part::Lease(l.share().unwrap())));
        let (runtime, server) = UnixStream::pair().unwrap();
        let client = Client {
            name: "batch".into(),
            definition: ScopeDefinition {
                persistent: false,
                pid: false,
                docker: true,
                storage: vec![],
            },
            namespaces: NamespaceFds {
                outer: File::open("/dev/null").unwrap(),
                user: File::open("/dev/null").unwrap(),
                net: File::open("/dev/null").unwrap(),
                pid: None,
                net_path: PathBuf::new(),
            },
            storage: BTreeMap::new(),
            channel: Mutex::new(Connection::new(runtime).unwrap()),
        };
        let receiver = std::thread::spawn(move || {
            let mut server = Channel::new(server).unwrap();
            let mut accepted = vec![];
            let mut batches = 0;
            let deadline = Instant::now() + Duration::from_secs(10);
            loop {
                assert!(Instant::now() < deadline, "Docker transaction stalled");
                let Some(frame) = server.receive().unwrap() else {
                    std::thread::yield_now();
                    continue;
                };
                let Message::AttachDocker {
                    config,
                    parts,
                    more,
                } = scope::decode(frame.value.clone()).unwrap()
                else {
                    panic!("unexpected Docker message");
                };
                assert_eq!(config.is_some(), batches == 0);
                assert!(frame.fds.len() <= goblins_runtime::channel::MAX_FDS);
                accepted.extend(Part::decode(parts, frame).unwrap());
                batches += 1;
                let response = if more {
                    Message::DockerPending {}
                } else {
                    Message::DockerAttached {
                        socket: "/scope/docker.sock".into(),
                    }
                };
                server
                    .queue(&scope::encode(&response), &mut vec![])
                    .unwrap();
                server.flush().unwrap();
                if !more {
                    assert_eq!(batches, 3);
                    return accepted;
                }
            }
        });
        assert_eq!(
            client
                .docker(
                    Some(scope::DockerConfig {
                        bwrap: "/bwrap".into(),
                        helper: "/helper".into(),
                        daemon: "/daemon".into()
                    }),
                    parts,
                    &Cancellation::default()
                )
                .unwrap(),
            PathBuf::from("/scope/docker.sock")
        );
        let accepted = receiver.join().unwrap();
        assert_eq!(
            accepted
                .iter()
                .filter(|p| matches!(p, Part::Bind(..)))
                .count(),
            count
        );
        assert_eq!(
            accepted
                .iter()
                .filter(|p| matches!(p, Part::Lease(_)))
                .count(),
            2
        );
        drop((owners, client));
        for path in &paths {
            assert!(path.join("root").exists());
            assert!(resource_exclusive(path).unwrap().is_none());
        }
        drop(accepted);
        for path in &paths {
            assert!(!path.exists());
        }
        std::fs::remove_dir(root).unwrap();
    }
    #[test]
    fn unresponsive_scope_release_closes_membership_at_the_deadline() {
        let (runtime, server) = UnixStream::pair().unwrap();
        let mut server = Channel::new(server).unwrap();
        let client = Client {
            name: "timeout".into(),
            definition: ScopeDefinition {
                persistent: false,
                pid: false,
                docker: false,
                storage: vec![],
            },
            namespaces: NamespaceFds {
                outer: File::open("/dev/null").unwrap(),
                user: File::open("/dev/null").unwrap(),
                net: File::open("/dev/null").unwrap(),
                pid: None,
                net_path: PathBuf::new(),
            },
            storage: BTreeMap::new(),
            channel: Mutex::new(Connection::new(runtime).unwrap()),
        };
        client.begin_leave().unwrap();
        assert!(matches!(
            scope::decode(server.receive().unwrap().unwrap().value).unwrap(),
            Message::Leave {}
        ));
        assert!(
            client
                .finish_leave(Instant::now())
                .unwrap_err()
                .to_string()
                .contains("stop deadline")
        );
        assert!(client.channel.lock().unwrap().closed());
        assert!(server.receive().unwrap().is_none());
        assert!(server.closed());
    }

    #[test]
    fn storage_exceeding_one_frame_is_received_in_bounded_batches() {
        let count = goblins_runtime::channel::MAX_FDS * 2 + 1;
        let names: Vec<_> = (0..count).map(|i| format!("storage{i}")).collect();
        let definition = ScopeDefinition {
            persistent: false,
            pid: false,
            docker: false,
            storage: names.clone(),
        };
        let mut launch: Launch = serde_json::from_value(serde_json::json!({
            "shell":"/unused", "posix_shell":"/unused", "helper":"/unused", "bwrap":"/unused", "flake":"unused", "client_package":"/unused"
        })).unwrap();
        launch.scope = Some(Scope {
            name: "batch".into(),
            definition,
            mounts: names
                .iter()
                .map(|n| (n.clone(), PathBuf::from(format!("/storage/{n}"))))
                .collect(),
        });
        let (client, server) = UnixStream::pair().unwrap();
        let thread = std::thread::spawn(move || {
            let mut server = Channel::new(server).unwrap();
            let receive = |server: &mut Channel| -> Message {
                loop {
                    if let Some(frame) = server.receive().unwrap() {
                        return scope::decode(frame.value).unwrap();
                    }
                    assert!(!server.closed());
                    std::thread::sleep(Duration::from_millis(1));
                }
            };
            let reply = |server: &mut Channel, message: Message, mut fds| {
                server.queue(&scope::encode(&message), &mut fds).unwrap();
                server.flush().unwrap();
            };
            assert!(matches!(receive(&mut server), Message::Join(_)));
            reply(
                &mut server,
                Message::Joined {
                    outer: Descriptor::new(0),
                    user: Descriptor::new(1),
                    net: Descriptor::new(2),
                    pid: None,
                    directory: "/scope".into(),
                },
                (0..3)
                    .map(|_| File::open("/dev/null").unwrap().into())
                    .collect(),
            );
            let mut total = 0;
            let mut batches = 0;
            while total < count {
                let Message::OpenStorage { names } = receive(&mut server) else {
                    panic!("expected storage batch");
                };
                assert!(!names.is_empty());
                assert!(names.len() <= goblins_runtime::channel::MAX_FDS);
                total += names.len();
                batches += 1;
                let fds = names
                    .iter()
                    .map(|_| File::open("/dev/null").unwrap().into())
                    .collect();
                let directories = names
                    .into_iter()
                    .enumerate()
                    .map(|(i, name)| (name, Descriptor::new(i)))
                    .collect();
                reply(&mut server, Message::Storage { directories }, fds);
            }
            assert_eq!(total, count);
            assert_eq!(batches, 3);
        });
        let client = Client::join(client.into(), &launch, &Cancellation::default()).unwrap();
        assert_eq!(client.storage.len(), count);
        for name in names {
            assert!(client.storage(&name).is_ok());
        }
        thread.join().unwrap();
    }
}
