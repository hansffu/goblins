use super::*;
use serde::Deserialize;
use std::collections::BTreeMap;

#[derive(Serialize, Deserialize)]
#[serde(tag = "kind", deny_unknown_fields)]
enum WireStdio {
    Fds { fds: [Descriptor; 3] },
    Tty { slave: Descriptor },
    NewPty { rows: u16, cols: u16 },
}
pub(super) fn encode_stdio(stdio: &Stdio, b: &mut Builder) -> io::Result<Value> {
    serde_json::to_value(match stdio {
        Stdio::Fds(fds) => WireStdio::Fds {
            fds: [b.fd(&fds[0])?, b.fd(&fds[1])?, b.fd(&fds[2])?],
        },
        Stdio::Tty(slave) => WireStdio::Tty {
            slave: b.fd(slave)?,
        },
        Stdio::NewPty { rows, cols } => WireStdio::NewPty {
            rows: *rows,
            cols: *cols,
        },
    })
    .map_err(other)
}
pub(super) fn decode_stdio(value: Value, d: &mut Descriptors) -> io::Result<Stdio> {
    Ok(match serde_json::from_value::<WireStdio>(value)? {
        WireStdio::Fds { fds } => Stdio::Fds([
            d.take(fds[0])?.into(),
            d.take(fds[1])?.into(),
            d.take(fds[2])?.into(),
        ]),
        WireStdio::Tty { slave } => Stdio::Tty(d.take(slave)?.into()),
        WireStdio::NewPty { rows, cols } => Stdio::NewPty { rows, cols },
    })
}
#[derive(Deserialize)]
#[serde(tag = "kind", deny_unknown_fields)]
enum WireSource {
    Host {
        configuration: PathBuf,
        name: String,
        scope: Option<String>,
        dev_shell: Option<String>,
        workspace: Option<PathBuf>,
        cwd: Option<PathBuf>,
        state: PathBuf,
        history: Option<PathBuf>,
        recreate: Option<Recreate>,
        sequence: u64,
    },
    Member {
        scope: Option<Descriptor>,
        owner_scope: Option<String>,
        manifest: PathBuf,
        configuration: String,
        auto: BTreeMap<String, PathBuf>,
        state: PathBuf,
        history: Option<PathBuf>,
    },
}
pub(super) fn encode_source(source: &Source, b: &mut Builder) -> io::Result<Value> {
    Ok(match source {
        Source::Host {
            configuration,
            name,
            scope,
            dev_shell,
            workspace,
            cwd,
            state,
            history,
            recreate,
            sequence,
        } => {
            json!({"kind":"Host","configuration":configuration,"name":name,"scope":scope,"dev_shell":dev_shell,"workspace":workspace,"cwd":cwd,"state":state,"history":history,"recreate":recreate,"sequence":sequence})
        }
        Source::Member {
            scope,
            owner_scope,
            manifest,
            configuration,
            auto,
            state,
            history,
        } => {
            let scope = scope.as_ref().map(|fd| b.fd(fd)).transpose()?;
            json!({"kind":"Member","scope":scope,"owner_scope":owner_scope,"manifest":manifest,"configuration":configuration,"auto":auto,"state":state,"history":history})
        }
    })
}
fn decode_source(value: Value, d: &mut Descriptors) -> io::Result<Source> {
    Ok(match serde_json::from_value::<WireSource>(value)? {
        WireSource::Host {
            configuration,
            name,
            scope,
            dev_shell,
            workspace,
            cwd,
            state,
            history,
            recreate,
            sequence,
        } => Source::Host {
            configuration,
            name,
            scope,
            dev_shell,
            workspace,
            cwd,
            state,
            history,
            recreate,
            sequence,
        },
        WireSource::Member {
            scope,
            owner_scope,
            manifest,
            configuration,
            auto,
            state,
            history,
        } => Source::Member {
            scope: scope.map(|fd| d.take(fd).map(Fd::from)).transpose()?,
            owner_scope,
            manifest,
            configuration,
            auto,
            state,
            history,
        },
    })
}
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct WireLease {
    directory: PathBuf,
    lock: Descriptor,
}
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct WireDirectory {
    fd: Descriptor,
    source: PathBuf,
    destination: PathBuf,
}
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct WireHandoff {
    transfer: u64,
    count: usize,
    workspace: Descriptor,
    working_directory: Option<WireDirectory>,
    // The generation contains arbitrary lock-file JSON. It is opaque to the
    // transport's descriptor scanner, like relayed RPC values.
    dev_shell: String,
    docker: bool,
    leases: Vec<WireLease>,
    more: bool,
}
fn lease(lease: &ResourceLease, b: &mut Builder) -> io::Result<WireLease> {
    let fd = unsafe { BorrowedFd::borrow_raw(lease.fd()) }.try_clone_to_owned()?;
    Ok(WireLease {
        directory: lease.directory().to_path_buf(),
        lock: b.owned(fd)?,
    })
}
pub(super) fn encode_handoff(h: &Handoff, b: &mut Builder) -> io::Result<Value> {
    let workspace = b.fd(&h.workspace)?;
    let working_directory = h
        .working_directory
        .as_ref()
        .map(|w| {
            Ok::<_, io::Error>(WireDirectory {
                fd: b.fd(&w.fd)?,
                source: w.source.clone(),
                destination: w.destination.clone(),
            })
        })
        .transpose()?;
    let count = h.leases.len();
    // Count cannot exceed the unchanged logical-byte budget; do not reserve
    // attacker-declared storage, or invent a smaller public ancestry limit.
    if count > channel::MAX_BODY {
        return Err(invalid("handoff exceeds control message budget"));
    }
    let first = count.min(channel::MAX_FDS - b.fds.len());
    let leases = h.leases[..first]
        .iter()
        .map(|l| lease(l, b))
        .collect::<io::Result<Vec<_>>>()?;
    let header = WireHandoff {
        transfer: b.transfer,
        count,
        workspace,
        working_directory,
        dev_shell: serde_json::to_string(&h.dev_shell)?,
        docker: h.docker,
        leases,
        more: first < count,
    };
    let mut at = first;
    while at < count {
        let end = (at + channel::MAX_FDS).min(count);
        let mut part = Builder::new("Leases", b.transfer);
        part.value("transfer", &b.transfer)?;
        let leases = h.leases[at..end]
            .iter()
            .map(|l| lease(l, &mut part))
            .collect::<io::Result<Vec<_>>>()?;
        part.value("leases", &leases)?;
        part.value("more", &(end < count))?;
        b.following.push_back(part.frame());
        at = end;
    }
    serde_json::to_value(header).map_err(other)
}
fn decode_leases(leases: Vec<WireLease>, d: &mut Descriptors) -> io::Result<Vec<ResourceLease>> {
    // Adopt each batch immediately. Partial transfers retain real cleanup
    // guards, rather than naked descriptors that forget directory ownership.
    leases
        .into_iter()
        .map(|l| ResourceLease::from_parts(l.directory, d.take(l.lock)?).map_err(other))
        .collect()
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct WirePreview {
    output_path: Option<PathBuf>,
    in_store: bool,
    download: Option<String>,
    build_required: bool,
}
pub(super) fn encode_preview(result: &Result<crate::catalog::Preview, String>) -> Value {
    match result {
        Ok(p) => {
            json!({"Ok":{"output_path":p.output_path,"in_store":p.in_store,"download":p.download,"build_required":p.build_required}})
        }
        Err(e) => json!({"Err":e}),
    }
}
pub(super) fn decode_preview(value: Value) -> io::Result<Result<crate::catalog::Preview, String>> {
    Ok(
        serde_json::from_value::<Result<WirePreview, String>>(value)?.map(|p| {
            crate::catalog::Preview {
                output_path: p.output_path,
                in_store: p.in_store,
                download: p.download,
                build_required: p.build_required,
            }
        }),
    )
}
enum Target {
    Startup(SupervisorStart),
    StartCommand,
    Exported(String),
}
struct Pending {
    target: Target,
    handoff: Handoff,
    transfer: u64,
    count: usize,
    bytes: usize,
}
impl Pending {
    fn complete(self) -> Message {
        let handoff = Box::new(self.handoff);
        match self.target {
            Target::Startup(mut start) => {
                start.handoff = Some(handoff);
                Message::SupervisorStart(start)
            }
            Target::StartCommand => Message::Command(Command::Start(handoff)),
            Target::Exported(member) => Message::Event(Event::Exported {
                member,
                result: Ok(handoff),
            }),
        }
    }
}
#[derive(Default)]
pub struct Decoder {
    pending: Option<Pending>,
}
impl Decoder {
    /// One frame, possibly one complete owned message. Cancellation/control
    /// messages may be delivered while a handoff is still being assembled.
    /// A second handoff cannot start until the current transaction completes.
    pub fn receive(&mut self, frame: Frame) -> io::Result<Option<Received>> {
        let result = self.receive_inner(frame);
        if result.is_err() {
            self.pending.take();
        }
        result
    }
    /// Normal ownership rejection drops typed pending state before closing
    /// the transport and its raw/queued descriptors. No rejection reply exists.
    pub fn close(&mut self, channel: &mut channel::Channel) {
        self.pending.take();
        channel.close();
    }
    /// Production receive/teardown order for the supervisor actors. A raw
    /// framing error may already have closed Channel; that crashed/misbehaving
    /// peer overlap retains the existing single-attempt/GC fallback.
    pub fn receive_channel(
        &mut self,
        channel: &mut channel::Channel,
    ) -> io::Result<Option<Received>> {
        let result = match channel.receive() {
            Ok(Some(frame)) => self.receive(frame),
            Ok(None) if channel.closed() => self.finish().map(|()| None),
            Ok(None) => Ok(None),
            // Linux reports reset, rather than clean EOF, when a peer closes
            // with unread data/SCM_RIGHTS. Channel has already closed itself;
            // settle typed receive state exactly as for EOF so the actor can
            // release its pending transfers on this same closed outcome.
            Err(error) if error.kind() == io::ErrorKind::ConnectionReset => {
                self.finish().map(|()| None)
            }
            Err(error) => Err(error),
        };
        if result.is_err() {
            self.close(channel);
        }
        result
    }
    pub fn finish(&mut self) -> io::Result<()> {
        if self.pending.take().is_some() {
            Err(invalid("EOF during handoff transfer"))
        } else {
            Ok(())
        }
    }
    fn handoff(
        &mut self,
        target: Target,
        value: Value,
        d: &mut Descriptors,
        bytes: usize,
    ) -> io::Result<Option<Received>> {
        if self.pending.is_some() {
            return Err(invalid("overlapping handoff transfers"));
        }
        let h: WireHandoff = serde_json::from_value(value)?;
        if h.transfer == 0
            || h.count > channel::MAX_BODY
            || h.count < h.leases.len()
            || h.more != (h.count > h.leases.len())
        {
            return Err(invalid("invalid handoff descriptor count"));
        }
        let working_directory = h
            .working_directory
            .map(|w| {
                Ok::<_, io::Error>(crate::WorkingDirectory {
                    fd: d.take(w.fd)?.into(),
                    source: w.source,
                    destination: w.destination,
                })
            })
            .transpose()?;
        let handoff = Handoff {
            workspace: d.take(h.workspace)?.into(),
            working_directory,
            dev_shell: serde_json::from_str(&h.dev_shell)?,
            docker: h.docker,
            leases: decode_leases(h.leases, d)?,
        };
        let pending = Pending {
            target,
            handoff,
            transfer: h.transfer,
            count: h.count,
            bytes,
        };
        if h.more {
            self.pending = Some(pending);
            Ok(None)
        } else {
            let transfer = pending.transfer;
            let message = pending.complete();
            if let Message::SupervisorStart(start) = &message {
                valid_start(start)?;
            }
            Ok(Some(Received {
                message,
                adopted: Some(transfer),
            }))
        }
    }
    fn receive_inner(&mut self, frame: Frame) -> io::Result<Option<Received>> {
        let bytes = serde_json::to_vec(&frame.value)?.len();
        if bytes > channel::MAX_BODY {
            return Err(invalid("control message too large"));
        }
        let mut d = Descriptors::new(frame.fds)?;
        let mut envelope = Params::new(frame.value)?;
        if envelope.take::<String>("jsonrpc")? != "2.0" {
            return Err(invalid("invalid supervisor protocol envelope"));
        }
        let method = envelope.take::<String>("method")?;
        let mut p = Params::new(envelope.take("params")?)?;
        envelope.finish()?;
        let received = match method.as_str() {
            "SupervisorStart" => {
                let supervisor_api = p.take("supervisor_api")?;
                if supervisor_api != API {
                    return Err(invalid("incompatible supervisor API"));
                }
                let source = decode_source(p.take("source")?, &mut d)?;
                let mut start = SupervisorStart {
                    supervisor_api,
                    source,
                    directory: p.take("directory")?,
                    dimensions: p.take("dimensions")?,
                    detached: p.take("detached")?,
                    handoff: None,
                };
                let handoff = p.take::<Option<Value>>("handoff")?;
                p.finish()?;
                match handoff {
                    Some(h) => self.handoff(Target::Startup(start), h, &mut d, bytes)?,
                    None => {
                        start.handoff = None;
                        valid_start(&start)?;
                        Some(Received {
                            message: Message::SupervisorStart(start),
                            adopted: None,
                        })
                    }
                }
            }
            "Command.Start" => {
                let h = p.take("handoff")?;
                p.finish()?;
                self.handoff(Target::StartCommand, h, &mut d, bytes)?
            }
            "Event.Exported" => {
                let member = p.take("member")?;
                let result = p.take::<Result<Value, String>>("result")?;
                p.finish()?;
                match result {
                    Ok(h) => self.handoff(Target::Exported(member), h, &mut d, bytes)?,
                    Err(error) => Some(Received {
                        message: Message::Event(Event::Exported {
                            member,
                            result: Err(error),
                        }),
                        adopted: None,
                    }),
                }
            }
            "Leases" => {
                let transfer = p.take::<u64>("transfer")?;
                let leases = p.take::<Vec<WireLease>>("leases")?;
                let more = p.take::<bool>("more")?;
                p.finish()?;
                let pending = self
                    .pending
                    .as_mut()
                    .ok_or_else(|| invalid("unexpected handoff batch"))?;
                if transfer != pending.transfer || leases.is_empty() {
                    return Err(invalid("invalid handoff batch"));
                }
                pending.bytes = pending
                    .bytes
                    .checked_add(bytes)
                    .ok_or_else(|| invalid("control message too large"))?;
                if pending.bytes > channel::MAX_BODY
                    || pending.handoff.leases.len() + leases.len() > pending.count
                {
                    return Err(invalid("handoff exceeds declared budget"));
                }
                pending
                    .handoff
                    .leases
                    .extend(decode_leases(leases, &mut d)?);
                if more != (pending.handoff.leases.len() < pending.count) {
                    return Err(invalid("invalid handoff continuation"));
                }
                if more {
                    None
                } else {
                    let pending = self.pending.take().unwrap();
                    let transfer = pending.transfer;
                    let message = pending.complete();
                    if let Message::SupervisorStart(start) = &message {
                        valid_start(start)?;
                    }
                    Some(Received {
                        message,
                        adopted: Some(transfer),
                    })
                }
            }
            "Event.Manifest" => {
                let sequence = p.take("sequence")?;
                let path = p.take("path")?;
                let root = p.take("root")?;
                let owner = p
                    .take::<Option<Descriptor>>("owner")?
                    .map(|fd| d.take(fd))
                    .transpose()?;
                let transfer = p.take::<u64>("transfer")?;
                p.finish()?;
                if transfer == 0 {
                    return Err(invalid("invalid ownership transfer id"));
                }
                let lease = ManifestLease::from_transfer(path, root, owner).map_err(other)?;
                Some(Received {
                    message: Message::Event(Event::Manifest { sequence, lease }),
                    adopted: Some(transfer),
                })
            }
            "Stopping" => {
                p.finish()?;
                Some(Received {
                    message: Message::Stopping,
                    adopted: None,
                })
            }
            "Released" => {
                p.finish()?;
                Some(Received {
                    message: Message::Released,
                    adopted: None,
                })
            }
            "Adopted" => {
                let transfer = p.take("transfer")?;
                p.finish()?;
                Some(Received {
                    message: Message::Adopted { transfer },
                    adopted: None,
                })
            }
            name if name.starts_with("Command.") => Some(Received {
                message: Message::Command(decode_command(name, p, &mut d)?),
                adopted: None,
            }),
            name if name.starts_with("Event.") => Some(Received {
                message: Message::Event(decode_event(name, p, &mut d)?),
                adopted: None,
            }),
            _ => return Err(invalid("unknown supervisor message")),
        };
        d.finish()?;
        Ok(received)
    }
}
