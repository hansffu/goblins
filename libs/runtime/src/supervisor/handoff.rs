use super::*;
use contract::{WireDirectory, WireHandoff, WireLease, WireSource, WireStdio};
use serde::Deserialize;

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
fn recreate_to_wire(value: &crate::Recreate) -> contract::Recreate {
    contract::Recreate {
        packages: value.packages.clone(),
        auto_approve: value.auto_approve.clone(),
        docker: value.docker,
        variables: value.variables.clone(),
        worktree: value.worktree.as_ref().map(|w| contract::Worktree {
            name: w.name.clone(),
            path: w.path.clone(),
        }),
    }
}
fn recreate_from_wire(value: contract::Recreate) -> crate::Recreate {
    crate::Recreate {
        packages: value.packages,
        auto_approve: value.auto_approve,
        docker: value.docker,
        variables: value.variables,
        worktree: value.worktree.map(|w| crate::worktrees::Worktree {
            name: w.name,
            path: w.path,
        }),
    }
}
pub(super) fn encode_source_wire(source: &Source, b: &mut Builder) -> io::Result<WireSource> {
    let wire = match source {
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
        } => WireSource::Host {
            configuration: configuration.clone(),
            name: name.clone(),
            scope: scope.clone(),
            dev_shell: dev_shell.clone(),
            workspace: workspace.clone(),
            cwd: cwd.clone(),
            state: state.clone(),
            history: history.clone(),
            recreate: recreate.as_ref().map(recreate_to_wire),
            sequence: *sequence,
        },
        Source::Member {
            scope,
            owner_scope,
            manifest,
            configuration,
            auto,
            state,
            history,
            worktree,
            arguments,
        } => {
            let scope = scope.as_ref().map(|fd| b.fd(fd)).transpose()?;
            WireSource::Member {
                scope,
                owner_scope: owner_scope.clone(),
                manifest: manifest.clone(),
                configuration: configuration.clone(),
                auto: auto.clone(),
                state: state.clone(),
                history: history.clone(),
                worktree: worktree.clone(),
                arguments: arguments.clone(),
            }
        }
    };
    Ok(wire)
}
#[cfg(test)]
pub(super) fn encode_source(source: &Source, b: &mut Builder) -> io::Result<Value> {
    // Preserve the original Source producer's json! failure boundary.
    Ok(json!(encode_source_wire(source, b)?))
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
            recreate: recreate.map(recreate_from_wire),
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
            worktree,
            arguments,
        } => Source::Member {
            scope: scope.map(|fd| d.take(fd).map(Fd::from)).transpose()?,
            owner_scope,
            manifest,
            configuration,
            auto,
            state,
            history,
            worktree,
            arguments,
        },
    })
}
fn lease(lease: &ResourceLease, b: &mut Builder) -> io::Result<WireLease> {
    let fd = unsafe { BorrowedFd::borrow_raw(lease.fd()) }.try_clone_to_owned()?;
    Ok(WireLease {
        directory: lease.directory().to_path_buf(),
        lock: b.owned(fd)?,
    })
}
pub(super) fn encode_handoff_wire(h: &Handoff, b: &mut Builder) -> io::Result<WireHandoff> {
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
        let mut part = Builder::new(contract::supervisor_notifications::Leases::NAME, b.transfer);
        let leases = h.leases[at..end]
            .iter()
            .map(|l| lease(l, &mut part))
            .collect::<io::Result<Vec<_>>>()?;
        part.notification::<contract::supervisor_notifications::Leases>(
            contract::SupervisorLeasesParams {
                transfer: b.transfer,
                leases,
                more: end < count,
            },
        )?;
        b.following.push_back(part.frame());
        at = end;
    }
    Ok(header)
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
            name if name == contract::supervisor_notifications::SupervisorStart::NAME => {
                let supervisor_api = p.take("supervisor_api")?;
                if supervisor_api != API {
                    return Err(invalid("incompatible supervisor API"));
                }
                let source_wire: Value = p.take("source")?;
                let source = decode_source(source_wire.clone(), &mut d)?;
                let mut wire = contract::SupervisorStartRead {
                    supervisor_api,
                    source: source_wire,
                    directory: p.take("directory")?,
                    dimensions: p.take("dimensions")?,
                    detached: p.take("detached")?,
                    payload_nofile: p.take("payload_nofile")?,
                    legacy_listener: p
                        .0
                        .remove("legacy_listener")
                        .map(serde_json::from_value::<Option<Descriptor>>)
                        .transpose()?
                        .flatten(),
                    handoff: None,
                };
                let legacy_listener = wire
                    .legacy_listener
                    .map(|fd| d.take(fd).map(Fd::from))
                    .transpose()?;
                wire.handoff = p.take::<Option<Value>>("handoff")?;
                p.finish()?;
                let mut start = SupervisorStart {
                    supervisor_api: wire.supervisor_api,
                    source,
                    directory: wire.directory,
                    dimensions: wire.dimensions,
                    detached: wire.detached,
                    payload_nofile: wire.payload_nofile,
                    legacy_listener,
                    handoff: None,
                };
                let handoff = wire.handoff;
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
            name if name == contract::supervisor_notifications::CommandStart::NAME => {
                let h = p.take("handoff")?;
                p.finish()?;
                self.handoff(Target::StartCommand, h, &mut d, bytes)?
            }
            name if name == contract::supervisor_notifications::EventExported::NAME => {
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
            name if name == contract::supervisor_notifications::Leases::NAME => {
                let wire = contract::SupervisorLeasesParams {
                    transfer: p.take("transfer")?,
                    leases: p.take("leases")?,
                    more: p.take("more")?,
                };
                p.finish()?;
                let contract::SupervisorLeasesParams {
                    transfer,
                    leases,
                    more,
                } = wire;
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
            name if name == contract::supervisor_notifications::EventManifest::NAME => {
                let sequence = p.take("sequence")?;
                let path = p.take("path")?;
                let root = p.take("root")?;
                let catalogue_wire: String = p.take("catalogue")?;
                let catalogue = serde_json::from_str(&catalogue_wire).map_err(other)?;
                let owner = p.take::<Option<Descriptor>>("owner")?;
                let owner_fd = owner.map(|fd| d.take(fd)).transpose()?;
                let wire = contract::SupervisorEventManifestParams {
                    sequence,
                    path,
                    root,
                    catalogue: catalogue_wire,
                    owner,
                    transfer: p.take("transfer")?,
                };
                p.finish()?;
                if wire.transfer == 0 {
                    return Err(invalid("invalid ownership transfer id"));
                }
                let lease =
                    ManifestLease::from_transfer(wire.path, wire.root, owner_fd).map_err(other)?;
                Some(Received {
                    message: Message::Event(Event::Manifest {
                        sequence: wire.sequence,
                        lease,
                        catalogue,
                    }),
                    adopted: Some(wire.transfer),
                })
            }
            name if name == contract::supervisor_notifications::Maintenance::NAME => {
                let supervisor_api = p.take("supervisor_api")?;
                let operation = p.take("operation")?;
                let stdout = p.take("stdout")?;
                let stdout_fd = d.take(stdout)?.into();
                let stderr = p.take("stderr")?;
                let stderr_fd = d.take(stderr)?.into();
                let wire = contract::SupervisorMaintenanceParams {
                    supervisor_api,
                    operation,
                    stdout,
                    stderr,
                };
                p.finish()?;
                let request = Maintenance {
                    supervisor_api: wire.supervisor_api,
                    operation: wire.operation,
                    stdout: stdout_fd,
                    stderr: stderr_fd,
                };
                valid_maintenance(&request)?;
                Some(Received {
                    message: Message::Maintenance(request),
                    adopted: None,
                })
            }
            name if name == contract::supervisor_notifications::MaintenanceFinished::NAME => {
                let wire = contract::SupervisorMaintenanceFinishedParams {
                    result: p.take("result")?,
                };
                p.finish()?;
                Some(Received {
                    message: Message::MaintenanceFinished {
                        result: wire.result,
                    },
                    adopted: None,
                })
            }
            name if name == contract::supervisor_notifications::Preflight::NAME => {
                let wire = contract::SupervisorPreflightParams {
                    supervisor_api: p.take("supervisor_api")?,
                    path: p.take("path")?,
                    directory: p.take("directory")?,
                    sequence: p.take("sequence")?,
                };
                p.finish()?;
                valid_preflight(wire.supervisor_api, &wire.directory)?;
                Some(Received {
                    message: Message::Preflight {
                        supervisor_api: wire.supervisor_api,
                        path: wire.path,
                        directory: wire.directory,
                        sequence: wire.sequence,
                    },
                    adopted: None,
                })
            }
            name if name == contract::supervisor_notifications::PreflightFailed::NAME => {
                let wire = contract::SupervisorPreflightFailedParams {
                    error: p.take("error")?,
                };
                p.finish()?;
                Some(Received {
                    message: Message::PreflightFailed { error: wire.error },
                    adopted: None,
                })
            }
            name if name == contract::supervisor_notifications::Stopping::NAME => {
                p.finish()?;
                Some(Received {
                    message: Message::Stopping,
                    adopted: None,
                })
            }
            name if name == contract::supervisor_notifications::Released::NAME => {
                p.finish()?;
                Some(Received {
                    message: Message::Released,
                    adopted: None,
                })
            }
            name if name == contract::supervisor_notifications::Adopted::NAME => {
                let wire = contract::SupervisorAdoptedParams {
                    transfer: p.take("transfer")?,
                };
                p.finish()?;
                Some(Received {
                    message: Message::Adopted {
                        transfer: wire.transfer,
                    },
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
