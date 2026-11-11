//! Private supervisor vocabulary and descriptor codec, over `channel::Channel`.
//! Raw JSON is opaque on this channel: only typed capability fields may use
//! the transport's `$fd` marker. Scope framing and public host limits are unchanged.
use crate::{Command, Event, Fd, Handoff, HostOutcome, Source, Stdio};
#[cfg(test)]
use crate::{Recreate, TerminalId, TerminalStatus};
use crate::{
    channel::{self, Descriptor, Frame},
    config::ManifestLease,
    lease::ResourceLease,
};
use goblins_protocol::contract::{self, Notification};
#[cfg(test)]
use serde::Deserialize;
use serde::{Serialize, de::DeserializeOwned};
use serde_json::{Map, Value, json};
use std::{
    collections::VecDeque,
    io,
    os::fd::{AsRawFd, BorrowedFd, OwnedFd},
    path::PathBuf,
};

pub const API: u32 = 1;
pub const CONTROL_FD: i32 = 3;
/// Bootstrap data, not the later `Command::Start` that hands off to a member.
pub struct SupervisorStart {
    pub supervisor_api: u32,
    pub source: Source,
    pub directory: PathBuf,
    pub dimensions: (u16, u16),
    pub detached: bool,
    /// Original daemon soft limit, absent when it was not raised.
    pub payload_nofile: Option<u64>,
    /// A prebound legacy frontend listener; the daemon never accepts from it.
    pub legacy_listener: Option<Fd>,
    pub handoff: Option<Box<Handoff>>,
}
/// Maintenance selects paths, while the implementation retains deletion policy.
pub use contract::MaintenanceOperation;
pub struct Maintenance {
    pub supervisor_api: u32,
    pub operation: MaintenanceOperation,
    /// Explicit output capabilities; common entry still has null stdio.
    pub stdout: Fd,
    pub stderr: Fd,
}
pub enum Message {
    SupervisorStart(SupervisorStart),
    Maintenance(Maintenance),
    MaintenanceFinished {
        result: Result<(), String>,
    },
    /// Startup-only validation/rooting in one pinned child, without an engine.
    Preflight {
        supervisor_api: u32,
        path: PathBuf,
        directory: PathBuf,
        sequence: u64,
    },
    /// An operation error; the one-shot child still completes successfully.
    PreflightFailed {
        error: String,
    },
    Command(Command),
    Event(Event),
    /// Stop reached the dispatcher; cleanup is not yet complete.
    Stopping,
    /// Monotonic sandbox/resource release, independent of terminal lifetime.
    Released,
    /// Ownership acknowledgement, never a request-admission credit.
    Adopted {
        transfer: u64,
    },
}
/// Retain this object through actual transmission. For every ownership-bearing
/// message, retain it until typed adoption or convert it with `into_pending`.
/// Transmission alone never releases the original resource/manifest leases.
/// Frames precede the original value in drop order, so unsent descriptor
/// duplicates close before their original resource leases.
pub struct Prepared {
    pub frames: VecDeque<Frame>,
    original: Option<Message>,
    transfer: u64,
    adoption: bool,
}
impl Prepared {
    pub fn transfer(&self) -> u64 {
        self.transfer
    }
    pub fn needs_adoption(&self) -> bool {
        self.adoption
    }
    /// After every frame has actually flushed, move the original typed leases
    /// into the actor's transfer-id map. Hold them until Adopted or channel
    /// teardown; on supervisor death the daemon waits for pidfd exit first.
    pub fn into_pending(mut self) -> io::Result<Option<PendingTransfer>> {
        if !self.frames.is_empty() {
            return Err(invalid("ownership transfer has unsent frames"));
        }
        Ok(if self.adoption {
            Some(PendingTransfer {
                transfer: self.transfer,
                original: self.original.take(),
            })
        } else {
            None
        })
    }
    pub fn adopted(&mut self, transfer: u64) -> io::Result<()> {
        if transfer != self.transfer || !self.adoption || !self.frames.is_empty() {
            return Err(invalid("unexpected ownership acknowledgement"));
        }
        self.adoption = false;
        // A receiver that already rejected its generation may have dropped
        // its guard. Releasing ours cleans that root too; a live receiver's
        // independent ownership lock prevents premature removal.
        self.original.take();
        Ok(())
    }
}
/// Full typed sender ownership after transmission, never just cleanup paths.
/// Drop after receiver teardown/EOF, or disarm after its typed Adopted token.
/// Drop makes the existing single nonblocking cleanup attempt; a crashed or
/// misbehaving peer retaining raw copies leaves collection to normal GC.
pub struct PendingTransfer {
    transfer: u64,
    original: Option<Message>,
}
impl PendingTransfer {
    pub fn transfer(&self) -> u64 {
        self.transfer
    }
    pub fn adopted(&mut self, transfer: u64) -> io::Result<()> {
        if transfer != self.transfer || self.original.is_none() {
            return Err(invalid("unexpected ownership acknowledgement"));
        }
        self.original.take();
        Ok(())
    }
}
pub struct Received {
    pub message: Message,
    /// Queue only after the returned owned message has been adopted.
    pub adopted: Option<u64>,
}
#[derive(Default)]
pub struct Encoder {
    next: u64,
}
impl Encoder {
    pub fn prepare(&mut self, mut message: Message) -> io::Result<Prepared> {
        self.next = self
            .next
            .checked_add(1)
            .ok_or_else(|| invalid("transfer id exhausted"))?;
        let transfer = self.next;
        let root = if let Message::Event(Event::Manifest { lease, .. }) = &mut message {
            lease.prepare_transfer().map_err(other)?
        } else {
            None
        };
        let mut b = Builder::new("", transfer);
        b.manifest = root;
        match &message {
            Message::SupervisorStart(start) => {
                valid_start(start)?;
                let source = encode_source_wire(&start.source, &mut b)?;
                // Retain the original Source json! failure point before other fields.
                let _ = json!(&source);
                // Preserve path serialization failure before descriptor duplication.
                serde_json::to_value(&start.directory)?;
                // Precede handoff batching so its first-frame allowance still
                // accounts for this descriptor and the member scope handle.
                let legacy_listener = start
                    .legacy_listener
                    .as_ref()
                    .map(|fd| b.fd(fd))
                    .transpose()?;
                let handoff = start
                    .handoff
                    .as_deref()
                    .map(|h| encode_handoff_wire(h, &mut b))
                    .transpose()?;
                // The original handoff helper serialized its completed header here.
                if let Some(header) = &handoff {
                    serde_json::to_value(header).map_err(other)?;
                }
                b.notification::<contract::supervisor_notifications::SupervisorStart>(
                    contract::SupervisorStartParams {
                        supervisor_api: start.supervisor_api,
                        source,
                        directory: start.directory.clone(),
                        dimensions: start.dimensions,
                        detached: start.detached,
                        payload_nofile: start.payload_nofile,
                        legacy_listener,
                        handoff,
                    },
                )?;
            }
            Message::Preflight {
                supervisor_api,
                path,
                directory,
                sequence,
            } => {
                valid_preflight(*supervisor_api, directory)?;
                b.notification::<contract::supervisor_notifications::Preflight>(
                    contract::SupervisorPreflightParams {
                        supervisor_api: *supervisor_api,
                        path: path.clone(),
                        directory: directory.clone(),
                        sequence: *sequence,
                    },
                )?;
            }
            Message::PreflightFailed { error } => {
                b.notification::<contract::supervisor_notifications::PreflightFailed>(
                    contract::SupervisorPreflightFailedParams {
                        error: error.clone(),
                    },
                )?;
            }
            Message::Maintenance(request) => {
                valid_maintenance(request)?;
                // Preserve operation serialization failure before stream duplication.
                serde_json::to_value(&request.operation)?;
                let stdout = b.fd(&request.stdout)?;
                let stderr = b.fd(&request.stderr)?;
                b.notification::<contract::supervisor_notifications::Maintenance>(
                    contract::SupervisorMaintenanceParams {
                        supervisor_api: request.supervisor_api,
                        operation: request.operation.clone(),
                        stdout,
                        stderr,
                    },
                )?;
            }
            Message::MaintenanceFinished { result } => {
                b.notification::<contract::supervisor_notifications::MaintenanceFinished>(
                    contract::SupervisorMaintenanceFinishedParams {
                        result: result.clone(),
                    },
                )?;
            }
            Message::Command(command) => encode_command(command, &mut b)?,
            Message::Event(event) => encode_event(event, &mut b)?,
            Message::Stopping => b.notification::<contract::supervisor_notifications::Stopping>(
                contract::SupervisorEmptyParams {},
            )?,
            Message::Released => b.notification::<contract::supervisor_notifications::Released>(
                contract::SupervisorEmptyParams {},
            )?,
            Message::Adopted { transfer } => {
                b.notification::<contract::supervisor_notifications::Adopted>(
                    contract::SupervisorAdoptedParams {
                        transfer: *transfer,
                    },
                )?;
            }
        }
        let adoption = matches!(
            message,
            Message::Event(Event::Manifest { .. } | Event::Exported { result: Ok(_), .. })
                | Message::Command(Command::Start(_))
                | Message::SupervisorStart(SupervisorStart {
                    handoff: Some(_),
                    ..
                })
        );
        let frames = b.finish()?;
        Ok(Prepared {
            frames,
            original: Some(message),
            transfer,
            adoption,
        })
    }
}
fn invalid(message: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message)
}
fn other(error: impl std::fmt::Display) -> io::Error {
    invalid(&error.to_string())
}
fn valid_maintenance(request: &Maintenance) -> io::Result<()> {
    if request.supervisor_api != API {
        return Err(invalid("incompatible supervisor API"));
    }
    for fd in [&request.stdout, &request.stderr] {
        let flags = goblins_unix::cvt(unsafe { libc::fcntl(fd.as_raw_fd(), libc::F_GETFL) })?;
        if flags & libc::O_ACCMODE == libc::O_RDONLY {
            return Err(invalid("maintenance output descriptor is not writable"));
        }
    }
    Ok(())
}
fn valid_preflight(api: u32, directory: &std::path::Path) -> io::Result<()> {
    if api != API {
        return Err(invalid("incompatible supervisor API"));
    }
    if !directory.is_absolute() {
        return Err(invalid("invalid preflight directory"));
    }
    Ok(())
}
fn valid_start(start: &SupervisorStart) -> io::Result<()> {
    if start.supervisor_api != API {
        return Err(invalid("incompatible supervisor API"));
    }
    if !start.directory.is_absolute() || !matches!(start.dimensions, (1..=1000, 1..=1000)) {
        return Err(invalid("invalid supervisor startup parameters"));
    }
    if matches!(start.payload_nofile, Some(0 | u64::MAX)) {
        return Err(invalid("invalid payload descriptor limit"));
    }
    if matches!(start.source, Source::Member { .. }) != start.handoff.is_some() {
        return Err(invalid(
            "member startup requires its handoff; host startup must not have one",
        ));
    }
    Ok(())
}
struct Builder {
    method: &'static str,
    params: Map<String, Value>,
    fds: Vec<OwnedFd>,
    following: VecDeque<Frame>,
    transfer: u64,
    manifest: Option<OwnedFd>,
}
impl Builder {
    fn new(method: &'static str, transfer: u64) -> Self {
        Self {
            method,
            params: Map::new(),
            fds: vec![],
            following: VecDeque::new(),
            transfer,
            manifest: None,
        }
    }
    fn notification<N: Notification>(&mut self, value: N::Params) -> io::Result<()> {
        let Value::Object(params) = serde_json::to_value(value)? else {
            return Err(invalid("expected control object"));
        };
        self.method = N::NAME;
        self.params = params;
        Ok(())
    }
    fn value(&mut self, key: &str, value: &impl Serialize) -> io::Result<()> {
        self.params.insert(key.into(), serde_json::to_value(value)?);
        Ok(())
    }
    fn opaque(&mut self, key: &str, value: &impl Serialize) -> io::Result<()> {
        self.params
            .insert(key.into(), Value::String(serde_json::to_string(value)?));
        Ok(())
    }
    fn owned(&mut self, fd: OwnedFd) -> io::Result<Descriptor> {
        if self.fds.len() >= channel::MAX_FDS {
            return Err(invalid("descriptor frame budget exceeded"));
        }
        let descriptor = Descriptor::new(self.fds.len());
        self.fds.push(fd);
        Ok(descriptor)
    }
    fn fd(&mut self, fd: &impl AsRawFd) -> io::Result<Descriptor> {
        // The original typed message remains owned by Prepared. SCM_RIGHTS
        // itself also duplicates an OFD; no host path is reopened here.
        self.owned(unsafe { BorrowedFd::borrow_raw(fd.as_raw_fd()) }.try_clone_to_owned()?)
    }
    fn frame(self) -> Frame {
        Frame {
            value: json!({"jsonrpc":"2.0","method":self.method,"params":self.params}),
            fds: self.fds,
        }
    }
    fn finish(mut self) -> io::Result<VecDeque<Frame>> {
        let mut frames = std::mem::take(&mut self.following);
        frames.push_front(self.frame());
        let bytes = frames.iter().try_fold(0usize, |sum, f| {
            sum.checked_add(serde_json::to_vec(&f.value)?.len())
                .ok_or_else(|| invalid("control message too large"))
        })?;
        // Batching extends the descriptor budget, not the logical byte budget.
        if bytes > channel::MAX_BODY {
            return Err(invalid("control message too large"));
        }
        Ok(frames)
    }
}
struct Params(Map<String, Value>);
impl Params {
    fn new(value: Value) -> io::Result<Self> {
        match value {
            Value::Object(map) => Ok(Self(map)),
            _ => Err(invalid("expected control object")),
        }
    }
    fn take<T: DeserializeOwned>(&mut self, name: &str) -> io::Result<T> {
        serde_json::from_value(
            self.0
                .remove(name)
                .ok_or_else(|| invalid(&format!("missing control field {name}")))?,
        )
        .map_err(other)
    }
    fn opaque<T: DeserializeOwned>(&mut self, name: &str) -> io::Result<T> {
        serde_json::from_str(&self.take::<String>(name)?).map_err(other)
    }
    fn finish(self) -> io::Result<()> {
        if self.0.is_empty() {
            Ok(())
        } else {
            Err(invalid("unexpected control field"))
        }
    }
}
struct Descriptors(Vec<Option<OwnedFd>>);
impl Descriptors {
    fn new(fds: Vec<OwnedFd>) -> io::Result<Self> {
        if fds.len() > channel::MAX_FDS {
            return Err(invalid("descriptor frame budget exceeded"));
        }
        Ok(Self(fds.into_iter().map(Some).collect()))
    }
    fn take(&mut self, descriptor: Descriptor) -> io::Result<OwnedFd> {
        self.0
            .get_mut(descriptor.index)
            .and_then(Option::take)
            .ok_or_else(|| invalid("invalid or duplicate descriptor reference"))
    }
    fn finish(self) -> io::Result<()> {
        if self.0.iter().all(Option::is_none) {
            Ok(())
        } else {
            Err(invalid("unreferenced control descriptor"))
        }
    }
}
macro_rules! put {
    ($b:ident, plain, $name:expr, $v:expr) => { $b.value($name, $v) };
    ($b:ident, opaque, $name:expr, $v:expr) => { $b.opaque($name, $v) };
    ($b:ident, membership, $name:expr, $v:expr) => {{ let value: Result<Descriptor,String> = match $v { Ok(fd) => Ok($b.fd(fd)?), Err(error) => Err(error.clone()) }; $b.value($name, &value) }};
    ($b:ident, call, $name:expr, $v:expr) => { $b.opaque($name, &json!({"jsonrpc":$v.jsonrpc,"id":$v.id,"method":$v.method,"params":$v.params})) };
}
macro_rules! get {
    ($p:ident, $d:ident, plain, $name:expr, $ty:ty) => {
        $p.take::<$ty>($name)?
    };
    ($p:ident, $d:ident, opaque, $name:expr, $ty:ty) => {
        $p.opaque::<$ty>($name)?
    };
    ($p:ident, $d:ident, membership, $name:expr, $ty:ty) => {
        match $p.take::<Result<Descriptor, String>>($name)? {
            Ok(fd) => Ok($d.take(fd)?.into()),
            Err(e) => Err(e),
        }
    };
    ($p:ident, $d:ident, call, $name:expr, $ty:ty) => {
        $p.opaque::<$ty>($name)?
    };
}
macro_rules! command_codec {
    ($($v:ident { $($f:ident : $ty:ty => $kind:ident),* $(,)? }),* $(,)?) => {
        fn encode_command(command: &Command, b: &mut Builder) -> io::Result<()> {
            match command {
                Command::Cancel(id) => b.notification::<contract::supervisor_notifications::CommandCancel>(contract::SupervisorCommandCancelParams { id:*id })?,
                Command::Stop => b.method="Command.Stop",
                Command::Start(handoff) => {
                    let handoff=encode_handoff_wire(handoff,b)?;
                    // Preserve the original helper's serialization boundary.
                    serde_json::to_value(&handoff).map_err(other)?;
                    b.notification::<contract::supervisor_notifications::CommandStart>(contract::SupervisorCommandStartParams { handoff })?;
                }
                Command::Spawn { id,kind,argv,cwd,env,stdio } => {
                    let stdio=encode_stdio_wire(stdio,b)?;
                    b.notification::<contract::supervisor_notifications::CommandSpawn>(contract::SupervisorCommandSpawnParams { id:*id,kind:*kind,argv:argv.clone(),cwd:cwd.clone(),env:env.clone(),stdio })?;
                }
                Command::Signal { id,signal } => b.notification::<contract::supervisor_notifications::CommandSignal>(contract::SupervisorCommandSignalParams { id:*id,signal:*signal })?,
                Command::CancelProcess { id,everything } => b.notification::<contract::supervisor_notifications::CommandCancelProcess>(contract::SupervisorCommandCancelProcessParams { id:*id,everything:*everything })?,
                Command::KillAll => b.notification::<contract::supervisor_notifications::CommandKillAll>(contract::SupervisorEmptyParams {})?,
                Command::Attach { terminal,stream,rows,cols } => {
                    // Keep terminal serialization before stream duplication.
                    serde_json::to_value(terminal)?;
                    let stream=b.fd(stream)?;
                    b.notification::<contract::supervisor_notifications::CommandAttach>(contract::SupervisorCommandAttachParams { terminal:*terminal,stream,rows:*rows,cols:*cols })?;
                }
                Command::Detach { terminal } => b.notification::<contract::supervisor_notifications::CommandDetach>(contract::SupervisorCommandDetachParams { terminal:*terminal })?,
                Command::Resize { terminal,rows,cols } => b.notification::<contract::supervisor_notifications::CommandResize>(contract::SupervisorCommandResizeParams { terminal:*terminal,rows:*rows,cols:*cols })?,
                Command::TerminalCall { request,terminal,call } => b.notification::<contract::supervisor_notifications::CommandTerminalCall>(contract::SupervisorCommandTerminalCallParams { request:*request,terminal:*terminal,call:call.clone() })?,
                Command::TerminalCancel { request } => b.notification::<contract::supervisor_notifications::CommandTerminalCancel>(contract::SupervisorCommandTerminalCancelParams { request:*request })?,
                Command::Preview { approval,package } => b.notification::<contract::supervisor_notifications::CommandPreview>(contract::SupervisorCommandPreviewParams { approval:*approval,package:package.clone() })?,
                Command::Decide { request,approved,output } => b.notification::<contract::supervisor_notifications::CommandDecide>(contract::SupervisorCommandDecideParams { request:request.clone(),approved:*approved,output:output.clone() })?,
                Command::Refresh { approval,changes } => b.notification::<contract::supervisor_notifications::CommandRefresh>(contract::SupervisorCommandRefreshParams { approval:*approval,changes:changes.clone() })?,
                Command::Run { serial,app } => b.notification::<contract::supervisor_notifications::CommandRun>(contract::SupervisorCommandRunParams { serial:*serial,app:app.clone() })?,
                Command::Diff { serial,changes } => b.notification::<contract::supervisor_notifications::CommandDiff>(contract::SupervisorCommandDiffParams { serial:*serial,changes:changes.clone() })?,
                $(Command::$v { $($f),* } => { b.method=concat!("Command.", stringify!($v)); $(put!(b,$kind,stringify!($f),$f)?;)* }),*
            }
            Ok(())
        }
        fn decode_command(method:&str, mut p:Params, d:&mut Descriptors) -> io::Result<Command> {
            let command=match method {
                name if name==contract::supervisor_notifications::CommandCancel::NAME => {
                    let wire=contract::SupervisorCommandCancelParams { id:p.take("id")? };
                    Command::Cancel(wire.id)
                },
                "Command.Stop" => Command::Stop,
                name if name==contract::supervisor_notifications::CommandSpawn::NAME => {
                    let wire=contract::SupervisorCommandSpawnParams {
                        id:p.take("id")?,kind:p.take("kind")?,argv:p.take("argv")?,cwd:p.take("cwd")?,env:p.take("env")?,
                        stdio:serde_json::from_value(p.take::<Value>("stdio")?)?,
                    };
                    let stdio=decode_stdio_wire(wire.stdio,d)?;
                    Command::Spawn { id:wire.id,kind:wire.kind,argv:wire.argv,cwd:wire.cwd,env:wire.env,stdio }
                },
                name if name==contract::supervisor_notifications::CommandSignal::NAME => {
                    let wire=contract::SupervisorCommandSignalParams { id:p.take("id")?,signal:p.take("signal")? };
                    Command::Signal { id:wire.id,signal:wire.signal }
                },
                name if name==contract::supervisor_notifications::CommandCancelProcess::NAME => {
                    let wire=contract::SupervisorCommandCancelProcessParams { id:p.take("id")?,everything:p.take("everything")? };
                    Command::CancelProcess { id:wire.id,everything:wire.everything }
                },
                name if name==contract::supervisor_notifications::CommandKillAll::NAME => Command::KillAll,
                name if name==contract::supervisor_notifications::CommandAttach::NAME => {
                    let terminal=p.take("terminal")?;
                    let descriptor=p.take("stream")?;
                    // Consume the capability before reading dimensions, as before.
                    let stream=d.take(descriptor)?.into();
                    let wire=contract::SupervisorCommandAttachParams { terminal,stream:descriptor,rows:p.take("rows")?,cols:p.take("cols")? };
                    Command::Attach { terminal:wire.terminal,stream,rows:wire.rows,cols:wire.cols }
                },
                name if name==contract::supervisor_notifications::CommandDetach::NAME => {
                    let wire=contract::SupervisorCommandDetachParams { terminal:p.take("terminal")? };
                    Command::Detach { terminal:wire.terminal }
                },
                name if name==contract::supervisor_notifications::CommandResize::NAME => {
                    let wire=contract::SupervisorCommandResizeParams { terminal:p.take("terminal")?,rows:p.take("rows")?,cols:p.take("cols")? };
                    Command::Resize { terminal:wire.terminal,rows:wire.rows,cols:wire.cols }
                },
                name if name==contract::supervisor_notifications::CommandTerminalCall::NAME => {
                    let wire=contract::SupervisorCommandTerminalCallParams { request:p.take("request")?,terminal:p.take("terminal")?,call:p.take("call")? };
                    Command::TerminalCall { request:wire.request,terminal:wire.terminal,call:wire.call }
                },
                name if name==contract::supervisor_notifications::CommandTerminalCancel::NAME => {
                    let wire=contract::SupervisorCommandTerminalCancelParams { request:p.take("request")? };
                    Command::TerminalCancel { request:wire.request }
                },
                name if name==contract::supervisor_notifications::CommandPreview::NAME => {
                    let wire=contract::SupervisorCommandPreviewParams { approval:p.take("approval")?,package:p.take("package")? };
                    Command::Preview { approval:wire.approval,package:wire.package }
                },
                name if name==contract::supervisor_notifications::CommandDecide::NAME => {
                    let wire=contract::SupervisorCommandDecideParams { request:p.take("request")?,approved:p.take("approved")?,output:p.take("output")? };
                    Command::Decide { request:wire.request,approved:wire.approved,output:wire.output }
                },
                name if name==contract::supervisor_notifications::CommandRefresh::NAME => {
                    let wire=contract::SupervisorCommandRefreshParams { approval:p.take("approval")?,changes:p.take("changes")? };
                    Command::Refresh { approval:wire.approval,changes:wire.changes }
                },
                name if name==contract::supervisor_notifications::CommandRun::NAME => {
                    let wire=contract::SupervisorCommandRunParams { serial:p.take("serial")?,app:p.take("app")? };
                    Command::Run { serial:wire.serial,app:wire.app }
                },
                name if name==contract::supervisor_notifications::CommandDiff::NAME => {
                    let wire=contract::SupervisorCommandDiffParams { serial:p.take("serial")?,changes:p.take("changes")? };
                    Command::Diff { serial:wire.serial,changes:wire.changes }
                },
                $(concat!("Command.",stringify!($v)) => Command::$v { $($f:get!(p,d,$kind,stringify!($f),$ty)),* }),*,
                _ => return Err(invalid("unknown supervisor command")),
            };
            p.finish()?; Ok(command)
        }
    }
}
command_codec! {
    RecordEvent { event:String=>plain, fields:Value=>opaque },
    SandboxSend { connection:u64=>plain, message:Value=>opaque },
    SandboxClose { connection:u64=>plain },
    ScopeFailed {},
    ScopeMembership { result:Result<Fd,String> =>membership },
    Export { member:String=>plain },
    PrepareHostJob { request:String=>plain, cwd:PathBuf=>plain },
    RunHostJob { request:String=>plain, argv:Vec<String> =>plain, env:Vec<(String,String)> =>plain, timeout:Option<u64> =>plain },
    CancelHostJob { request:String=>plain },
}
macro_rules! event_codec {
    ($($v:ident { $($f:ident : $ty:ty => $kind:ident),* $(,)? }),* $(,)?) => {
        fn encode_event(event:&Event,b:&mut Builder) -> io::Result<()> {
            match event {
                Event::Failed(error) => { b.method="Event.Failed"; b.value("error",error)?; }
                Event::Manifest { sequence,lease,catalogue } => {
                    // Preserve path/root serialization before catalogue and owner.
                    serde_json::to_value(&lease.path)?;
                    let root=lease.root().map(std::path::Path::to_path_buf);
                    serde_json::to_value(&root)?;
                    // Configuration names remain opaque to descriptor scanning.
                    let catalogue=serde_json::to_string(catalogue)?;
                    let owner=b.manifest.take().map(|fd| b.owned(fd)).transpose()?;
                    let transfer=b.transfer;
                    b.notification::<contract::supervisor_notifications::EventManifest>(contract::SupervisorEventManifestParams {
                        sequence:*sequence, path:lease.path.clone(), root, catalogue, owner, transfer,
                    })?;
                }
                Event::Exported { member,result } => {
                    let result=match result {
                        Ok(h) => {
                            let handoff=encode_handoff_wire(h,b)?;
                            serde_json::to_value(&handoff).map_err(other)?;
                            Ok(handoff)
                        },
                        Err(error)=>Err(error.clone()),
                    };
                    b.notification::<contract::supervisor_notifications::EventExported>(contract::SupervisorEventExportedParams { member:member.clone(),result })?;
                }
                Event::Spawned { id,pid } => b.notification::<contract::supervisor_notifications::EventSpawned>(contract::SupervisorEventSpawnedParams { id:*id,pid:*pid })?,
                Event::SpawnFailed { id,code,message } => b.notification::<contract::supervisor_notifications::EventSpawnFailed>(contract::SupervisorEventSpawnFailedParams { id:*id,code:*code,message:message.clone() })?,
                Event::Exited { id,code,signal } => b.notification::<contract::supervisor_notifications::EventExited>(contract::SupervisorEventExitedParams { id:*id,code:*code,signal:*signal })?,
                Event::Terminal { terminal,status,error } => b.notification::<contract::supervisor_notifications::EventTerminal>(contract::SupervisorEventTerminalParams { terminal:*terminal,status:*status,error:error.clone() })?,
                Event::TerminalResult { request,result } => {
                    let result=serde_json::to_string(result)?;
                    b.notification::<contract::supervisor_notifications::EventTerminalResult>(contract::SupervisorEventTerminalResultParams { request:*request,result })?;
                },
                Event::Preview { approval,result } => {
                    let result=encode_preview_wire(result);
                    b.notification::<contract::supervisor_notifications::EventPreview>(contract::SupervisorEventPreviewParams { approval:*approval,result })?;
                },
                Event::Progress { request,state } => b.notification::<contract::supervisor_notifications::EventProgress>(contract::SupervisorEventProgressParams { request:request.clone(),state:state.clone() })?,
                Event::Granted { reply,detail,output } => b.notification::<contract::supervisor_notifications::EventGranted>(contract::SupervisorEventGrantedParams { reply:goblins_protocol::Reply { id:reply.id.clone(),status:reply.status.clone(),message:reply.message.clone() },detail:detail.clone(),output:output.clone() })?,
                Event::Refresh { approval,result } => {
                    let result=serde_json::to_string(result)?;
                    b.notification::<contract::supervisor_notifications::EventRefresh>(contract::SupervisorEventRefreshParams { approval:*approval,result })?;
                },
                Event::Call { serial,result } => {
                    let result=serde_json::to_string(result)?;
                    b.notification::<contract::supervisor_notifications::EventCall>(contract::SupervisorEventCallParams { serial:*serial,result })?;
                },
                Event::DevShell { summary } => {
                    let summary=serde_json::to_string(summary)?;
                    b.notification::<contract::supervisor_notifications::EventDevShell>(contract::SupervisorEventDevShellParams { summary })?;
                },
                $(Event::$v { $($f),* } => { b.method=concat!("Event.",stringify!($v)); $(put!(b,$kind,stringify!($f),$f)?;)* }),*
                Event::InitLost => b.method="Event.InitLost",
                Event::PayloadExited => b.method="Event.PayloadExited",
            }
            Ok(())
        }
        fn decode_event(method:&str,mut p:Params,_d:&mut Descriptors)->io::Result<Event> {
            let event=match method {
                "Event.Failed"=>Event::Failed(p.take("error")?),
                "Event.InitLost"=>Event::InitLost,
                "Event.PayloadExited"=>Event::PayloadExited,
                name if name==contract::supervisor_notifications::EventSpawned::NAME => {
                    let wire=contract::SupervisorEventSpawnedParams { id:p.take("id")?,pid:p.take("pid")? };
                    Event::Spawned { id:wire.id,pid:wire.pid }
                },
                name if name==contract::supervisor_notifications::EventSpawnFailed::NAME => {
                    let wire=contract::SupervisorEventSpawnFailedParams { id:p.take("id")?,code:p.take("code")?,message:p.take("message")? };
                    Event::SpawnFailed { id:wire.id,code:wire.code,message:wire.message }
                },
                name if name==contract::supervisor_notifications::EventExited::NAME => {
                    let wire=contract::SupervisorEventExitedParams { id:p.take("id")?,code:p.take("code")?,signal:p.take("signal")? };
                    Event::Exited { id:wire.id,code:wire.code,signal:wire.signal }
                },
                name if name==contract::supervisor_notifications::EventTerminal::NAME => {
                    let wire=contract::SupervisorEventTerminalParams { terminal:p.take("terminal")?,status:p.take("status")?,error:p.take("error")? };
                    Event::Terminal { terminal:wire.terminal,status:wire.status,error:wire.error }
                },
                name if name==contract::supervisor_notifications::EventTerminalResult::NAME => {
                    let wire=contract::SupervisorEventTerminalResultParams { request:p.take("request")?,result:p.take("result")? };
                    // Opaque inner JSON still parses before unknown-root rejection.
                    let result=serde_json::from_str(&wire.result).map_err(other)?;
                    Event::TerminalResult { request:wire.request,result }
                },
                name if name==contract::supervisor_notifications::EventPreview::NAME => {
                    let wire=contract::SupervisorEventPreviewParams { approval:p.take("approval")?,result:p.take("result")? };
                    Event::Preview { approval:wire.approval,result:decode_preview_wire(wire.result) }
                },
                name if name==contract::supervisor_notifications::EventProgress::NAME => {
                    let wire=contract::SupervisorEventProgressParams { request:p.take("request")?,state:p.take("state")? };
                    Event::Progress { request:wire.request,state:wire.state }
                },
                name if name==contract::supervisor_notifications::EventGranted::NAME => {
                    let wire=contract::SupervisorEventGrantedParams { reply:p.take("reply")?,detail:p.take("detail")?,output:p.take("output")? };
                    Event::Granted { reply:wire.reply,detail:wire.detail,output:wire.output }
                },
                name if name==contract::supervisor_notifications::EventRefresh::NAME => {
                    let wire=contract::SupervisorEventRefreshParams { approval:p.take("approval")?,result:p.take("result")? };
                    let result=serde_json::from_str(&wire.result).map_err(other)?;
                    Event::Refresh { approval:wire.approval,result }
                },
                name if name==contract::supervisor_notifications::EventCall::NAME => {
                    let wire=contract::SupervisorEventCallParams { serial:p.take("serial")?,result:p.take("result")? };
                    let result=serde_json::from_str(&wire.result).map_err(other)?;
                    Event::Call { serial:wire.serial,result }
                },
                name if name==contract::supervisor_notifications::EventDevShell::NAME => {
                    let wire=contract::SupervisorEventDevShellParams { summary:p.take("summary")? };
                    let summary=serde_json::from_str(&wire.summary).map_err(other)?;
                    Event::DevShell { summary }
                },
                $(concat!("Event.",stringify!($v))=>Event::$v { $($f:get!(p,_d,$kind,stringify!($f),$ty)),* }),*,
                _=>return Err(invalid("unknown supervisor event")),
            };
            p.finish()?; Ok(event)
        }
    }
}
event_codec! {
    ScopeFailed { name:String=>plain },
    ScopeNeeded { name:String=>plain },
    Started { docker_enabled:bool=>plain, policy:Box<crate::policy::Policy> =>plain, support:crate::policy::Support=>plain, dev_shell:Option<crate::policy::DevShellSummary> =>opaque, identity:crate::Identity=>plain, recreated:Option<Box<crate::Recreated>> =>plain, auto:Box<crate::Auto> =>plain, worktrees:Box<crate::worktrees::Report> =>plain },
    Mode { server:bool=>plain },
    SandboxOpened { connection:u64=>plain },
    SandboxCall { connection:u64=>plain, call:goblins_jsonrpc::Call=>call },
    SandboxClosed { connection:u64=>plain },
    HostJobPrepared { request:String=>plain, result:Result<PathBuf,String> =>plain },
    HostJobFailed { request:String=>plain, error:String=>plain },
    HostJobFinished { request:String=>plain, outcome:Result<HostOutcome,String> =>plain },
    Stopped { exit_code:Option<i32> =>plain },
}
mod handoff;
pub mod outbound;
pub use handoff::Decoder;
use handoff::*;
#[cfg(test)]
mod tests;

#[cfg(test)]
mod outbound_tests;
