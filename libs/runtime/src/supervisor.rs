//! Private supervisor vocabulary and descriptor codec, over `channel::Channel`.
//! Raw JSON is opaque on this channel: only typed capability fields may use
//! the transport's `$fd` marker. Scope framing and public host limits are unchanged.
use crate::{
    Command, Event, Fd, Handoff, HostOutcome, Recreate, Source, Stdio, TerminalId, TerminalStatus,
};
use crate::{
    channel::{self, Descriptor, Frame},
    config::ManifestLease,
    lease::ResourceLease,
};
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
    pub handoff: Option<Box<Handoff>>,
}
pub enum Message {
    SupervisorStart(SupervisorStart),
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
                b.method = "SupervisorStart";
                b.value("supervisor_api", &start.supervisor_api)?;
                let source = encode_source(&start.source, &mut b)?;
                b.value("source", &source)?;
                b.value("directory", &start.directory)?;
                b.value("dimensions", &start.dimensions)?;
                b.value("detached", &start.detached)?;
                let handoff = start
                    .handoff
                    .as_deref()
                    .map(|h| encode_handoff(h, &mut b))
                    .transpose()?;
                b.value("handoff", &handoff)?;
            }
            Message::Command(command) => encode_command(command, &mut b)?,
            Message::Event(event) => encode_event(event, &mut b)?,
            Message::Stopping => b.method = "Stopping",
            Message::Released => b.method = "Released",
            Message::Adopted { transfer } => {
                b.method = "Adopted";
                b.value("transfer", transfer)?;
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
fn valid_start(start: &SupervisorStart) -> io::Result<()> {
    if start.supervisor_api != API {
        return Err(invalid("incompatible supervisor API"));
    }
    if !start.directory.is_absolute() || !matches!(start.dimensions, (1..=1000, 1..=1000)) {
        return Err(invalid("invalid supervisor startup parameters"));
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
    ($b:ident, fd, $name:expr, $v:expr) => {{ let fd = $b.fd($v)?; $b.value($name, &fd) }};
    ($b:ident, stdio, $name:expr, $v:expr) => {{ let value = encode_stdio($v, $b)?; $b.value($name, &value) }};
    ($b:ident, membership, $name:expr, $v:expr) => {{ let value: Result<Descriptor,String> = match $v { Ok(fd) => Ok($b.fd(fd)?), Err(error) => Err(error.clone()) }; $b.value($name, &value) }};
    ($b:ident, preview, $name:expr, $v:expr) => { $b.value($name, &encode_preview($v)) };
    ($b:ident, call, $name:expr, $v:expr) => { $b.opaque($name, &json!({"jsonrpc":$v.jsonrpc,"id":$v.id,"method":$v.method,"params":$v.params})) };
}
macro_rules! get {
    ($p:ident, $d:ident, plain, $name:expr, $ty:ty) => {
        $p.take::<$ty>($name)?
    };
    ($p:ident, $d:ident, opaque, $name:expr, $ty:ty) => {
        $p.opaque::<$ty>($name)?
    };
    ($p:ident, $d:ident, fd, $name:expr, $ty:ty) => {
        Fd::from($d.take($p.take($name)?)?)
    };
    ($p:ident, $d:ident, stdio, $name:expr, $ty:ty) => {
        decode_stdio($p.take($name)?, $d)?
    };
    ($p:ident, $d:ident, membership, $name:expr, $ty:ty) => {
        match $p.take::<Result<Descriptor, String>>($name)? {
            Ok(fd) => Ok($d.take(fd)?.into()),
            Err(e) => Err(e),
        }
    };
    ($p:ident, $d:ident, preview, $name:expr, $ty:ty) => {
        decode_preview($p.take($name)?)?
    };
    ($p:ident, $d:ident, call, $name:expr, $ty:ty) => {
        $p.opaque::<$ty>($name)?
    };
}
macro_rules! command_codec {
    ($($v:ident { $($f:ident : $ty:ty => $kind:ident),* $(,)? }),* $(,)?) => {
        fn encode_command(command: &Command, b: &mut Builder) -> io::Result<()> {
            match command {
                Command::Cancel(id) => { b.method="Command.Cancel"; b.value("id",id)?; }
                Command::Stop => b.method="Command.Stop",
                Command::Start(handoff) => { b.method="Command.Start"; let h=encode_handoff(handoff,b)?; b.value("handoff",&h)?; }
                $(Command::$v { $($f),* } => { b.method=concat!("Command.", stringify!($v)); $(put!(b,$kind,stringify!($f),$f)?;)* }),*
            }
            Ok(())
        }
        fn decode_command(method:&str, mut p:Params, d:&mut Descriptors) -> io::Result<Command> {
            let command=match method {
                "Command.Cancel" => Command::Cancel(p.take("id")?),
                "Command.Stop" => Command::Stop,
                $(concat!("Command.",stringify!($v)) => Command::$v { $($f:get!(p,d,$kind,stringify!($f),$ty)),* }),*,
                _ => return Err(invalid("unknown supervisor command")),
            };
            p.finish()?; Ok(command)
        }
    }
}
command_codec! {
    Attach { terminal:TerminalId=>plain, stream:Fd=>fd, rows:u16=>plain, cols:u16=>plain },
    Detach { terminal:TerminalId=>plain },
    Resize { terminal:TerminalId=>plain, rows:u16=>plain, cols:u16=>plain },
    SandboxSend { connection:u64=>plain, message:Value=>opaque },
    SandboxClose { connection:u64=>plain },
    ScopeFailed {},
    ScopeMembership { result:Result<Fd,String> =>membership },
    Preview { approval:u64=>plain, package:String=>plain },
    Decide { request:goblins_protocol::Request=>plain, approved:bool=>plain, output:Option<PathBuf> =>plain },
    Refresh { approval:u64=>plain, changes:crate::devshell::Changes=>plain },
    Run { serial:u64=>plain, app:String=>plain },
    Diff { serial:u64=>plain, changes:crate::devshell::Changes=>plain },
    Spawn { id:u64=>plain, kind:crate::SpawnKind=>plain, argv:Option<Vec<String>> =>plain, cwd:Option<String> =>plain, env:Vec<(String,String)> =>plain, stdio:Stdio=>stdio },
    Signal { id:u64=>plain, signal:i32=>plain },
    CancelProcess { id:u64=>plain, everything:bool=>plain },
    KillAll {},
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
                Event::Manifest { sequence,lease } => {
                    b.method="Event.Manifest"; b.value("sequence",sequence)?; b.value("path",&lease.path)?; b.value("root",&lease.root())?;
                    let owner=b.manifest.take().map(|fd| b.owned(fd)).transpose()?;
                    b.value("owner",&owner)?; let transfer = b.transfer; b.value("transfer",&transfer)?;
                }
                Event::Exported { member,result } => {
                    b.method="Event.Exported"; b.value("member",member)?;
                    let result=match result { Ok(h) => Ok(encode_handoff(h,b)?), Err(e)=>Err(e.clone()) };
                    b.value("result",&result)?;
                }
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
    Preview { approval:u64=>plain, result:Result<crate::catalog::Preview,String> =>preview },
    Started { docker_enabled:bool=>plain, policy:Box<crate::policy::Policy> =>plain, support:crate::policy::Support=>plain, dev_shell:Option<crate::policy::DevShellSummary> =>opaque, identity:crate::Identity=>plain, recreated:Option<Box<crate::Recreated>> =>plain, auto:Box<crate::Auto> =>plain },
    Progress { request:String=>plain, state:String=>plain },
    Mode { server:bool=>plain },
    Refresh { approval:u64=>plain, result:Result<(Value,String),String> =>opaque },
    Call { serial:u64=>plain, result:Result<Value,String> =>opaque },
    DevShell { summary:crate::policy::DevShellSummary=>opaque },
    Granted { reply:goblins_protocol::Reply=>plain, detail:Option<String> =>plain, output:Option<PathBuf> =>plain },
    Spawned { id:u64=>plain, pid:i32=>plain },
    Terminal { terminal:TerminalId=>plain, status:TerminalStatus=>plain, error:Option<String> =>plain },
    SpawnFailed { id:u64=>plain, code:Option<i32> =>plain, message:String=>plain },
    Exited { id:u64=>plain, code:Option<i32> =>plain, signal:Option<i32> =>plain },
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
