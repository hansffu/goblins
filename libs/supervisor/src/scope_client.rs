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
    pub directory: PathBuf,
    pub namespaces: NamespaceFds,
    storage: BTreeMap<String, File>,
    channel: Mutex<Channel>,
}
fn take(fds: &mut [Option<OwnedFd>], descriptor: Descriptor) -> Result<File> {
    fds.get_mut(descriptor.index)
        .and_then(Option::take)
        .map(File::from)
        .ok_or_else(|| "invalid scope descriptor index".into())
}
fn exchange(
    channel: &mut Channel,
    message: Message,
    mut fds: Vec<OwnedFd>,
    cancel: &Cancellation,
    deadline: Option<Instant>,
) -> Result<Frame> {
    channel.queue(&scope::encode(&message), &mut fds)?;
    loop {
        cancel.check()?;
        if deadline.is_some_and(|d| Instant::now() >= d) {
            return Err("scope registration exceeded helper startup limit".into());
        }
        channel.flush()?;
        if let Some(frame) = channel.receive()? {
            if let Message::Error { message } = scope::decode(frame.value.clone())? {
                return Err(message.into());
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
    pub fn join(fd: OwnedFd, launch: &Launch, cancel: &Cancellation) -> Result<Self> {
        let selected = launch.scope.as_ref().ok_or("launch has no scope")?;
        let mut channel = Channel::new(UnixStream::from(fd))?;
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
            directory,
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
            directory,
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
                if !matches!(scope::decode(frame.value)?, Message::Left {}) {
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
    pub fn leave(&self, deadline: Instant) -> Result<()> {
        self.begin_leave()?;
        self.finish_leave(deadline)
    }
    pub fn alive(&self) -> bool {
        let Ok(channel) = self.channel.try_lock() else {
            return true;
        };
        let Some(fd) = channel.fd() else {
            return false;
        };
        let mut byte = 0u8;
        let n = unsafe {
            libc::recv(
                fd,
                (&mut byte as *mut u8).cast(),
                1,
                libc::MSG_PEEK | libc::MSG_DONTWAIT,
            )
        };
        n < 0
            && matches!(
                std::io::Error::last_os_error().kind(),
                std::io::ErrorKind::WouldBlock | std::io::ErrorKind::Interrupted
            )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use goblins_runtime::config::{Scope, ScopeDefinition};
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
            directory: "/unused".into(),
            namespaces: NamespaceFds {
                outer: File::open("/dev/null").unwrap(),
                user: File::open("/dev/null").unwrap(),
                net: File::open("/dev/null").unwrap(),
                pid: None,
                net_path: PathBuf::new(),
            },
            storage: BTreeMap::new(),
            channel: Mutex::new(Channel::new(runtime).unwrap()),
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
