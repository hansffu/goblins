//! Frontend-independent event pump. `tick` never waits for a request, human,
//! helper or Nix. One worker owns startup and sequential grants; its cancellation
//! token is independent of its command queue. No daemon or recovery protocol.
use crate::{
    Result,
    config::Manifest,
    session::Identity,
    unix,
    worker::{Completed, Work, Worker},
};
use goblins_protocol::{FRAME_TIMEOUT, Frame, MAX_FRAME, Reply, Request, line, parse_request};
use serde::Deserialize;
use std::{
    collections::BTreeSet,
    fs::{self, File},
    io::{self, Read},
    os::{
        fd::{AsRawFd, OwnedFd},
        unix::net::{UnixListener, UnixStream},
    },
    path::{Path, PathBuf},
    time::Instant,
};

const MAX_READING_CLIENTS: usize = 16;
#[derive(Debug)]
pub enum Event {
    Connected(String),
    Stopped,
    Request {
        request: Request,
        approval: ApprovalId,
    },
    Result(Reply),
    Detail(String),
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ApprovalId(u64);

struct IncomingHost {
    peer: OwnedFd,
    deadline: Instant,
}
struct IncomingRequest {
    peer: UnixStream,
    frame: Frame,
}
struct Pending {
    approval: ApprovalId,
    peer: UnixStream,
    request: Request,
    decided: bool,
}
struct Active {
    name: String,
    peer: OwnedFd,
    worker: Worker,
    listener: Option<UnixListener>,
    identity: Option<Identity>,
    packages: BTreeSet<String>,
    reading: Vec<IncomingRequest>,
    pending: Option<Pending>,
    stopping: bool,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Attach {
    v: u32,
    op: String,
    name: String,
    configuration: String,
    rows: u16,
    cols: u16,
}

pub struct Controller {
    manifest: Manifest,
    configuration: String,
    workspace: Option<PathBuf>,
    socket_path: PathBuf,
    next_approval: u64,
    _lock: File,
    listener: OwnedFd,
    incoming: Vec<IncomingHost>,
    active: Option<Active>,
}
impl Controller {
    pub fn new(
        manifest: Manifest,
        configuration: String,
        state: &Path,
        workspace: Option<PathBuf>,
    ) -> Result<Self> {
        unix::private_directory(state)?;
        let lock = unix::lock(&state.join("serve.lock"))?;
        let socket_path = state.join("serve.sock");
        if socket_path.is_symlink() {
            return Err("refusing symlink at control socket".into());
        }
        if socket_path.exists() {
            fs::remove_file(&socket_path)?;
        }
        let listener = unix::seqpacket(&socket_path, true)?;
        Ok(Self {
            manifest,
            configuration,
            workspace,
            socket_path,
            next_approval: 0,
            _lock: lock,
            listener,
            incoming: Vec::new(),
            active: None,
        })
    }
    pub fn status(&self) -> serde_json::Value {
        serde_json::json!({"session":self.active.as_ref().and_then(|a| a.identity.as_ref()), "packages":self.active.as_ref().map(|a| &a.packages), "stopping":self.active.as_ref().is_some_and(|a| a.stopping)})
    }
    /// Returns false for stale decisions or disconnected clients. The request
    /// token binds frontend input to the exact pending request, never a successor.
    pub fn decide(&mut self, approval: ApprovalId, approved: bool) -> bool {
        let Some(active) = self.active.as_mut().filter(|a| !a.stopping) else {
            return false;
        };
        let Some(pending) = active
            .pending
            .as_mut()
            .filter(|p| !p.decided && p.approval == approval)
        else {
            return false;
        };
        if unix::disconnected(pending.peer.as_raw_fd()) {
            active.pending.take();
            return false;
        }
        pending.decided = true;
        if active
            .worker
            .commands
            .send(Work::Decide {
                request: pending.request.clone(),
                approved,
            })
            .is_err()
        {
            Self::stop(active);
            return false;
        }
        true
    }
    fn stop(active: &mut Active) {
        active.stopping = true;
        active.worker.cancel.cancel();
        active.listener.take();
        active.reading.clear();
        active.pending.take();
    }
    pub fn stop_session(&mut self) {
        if let Some(active) = &mut self.active {
            Self::stop(active);
        }
    }
    pub fn tick(&mut self) -> Result<Vec<Event>> {
        let now = Instant::now();
        let mut events = Vec::new();
        // Limit both accepted descriptors and work per tick, even under a flood.
        for _ in 0..MAX_READING_CLIENTS {
            match unix::accept(self.listener.as_raw_fd()) {
                Ok(peer) if self.incoming.len() < MAX_READING_CLIENTS => {
                    self.incoming.push(IncomingHost {
                        peer,
                        deadline: now + FRAME_TIMEOUT,
                    })
                }
                Ok(_) => (),
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => break,
                Err(e) => return Err(e.into()),
            }
        }
        let mut index = 0;
        while index < self.incoming.len() {
            let mut bytes = [0; MAX_FRAME + 1];
            let result = if now >= self.incoming[index].deadline {
                Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    "host request deadline exceeded",
                ))
            } else {
                unix::recv(self.incoming[index].peer.as_raw_fd(), &mut bytes)
            };
            if matches!(&result,Err(e) if e.kind() == io::ErrorKind::WouldBlock) {
                index += 1;
                continue;
            }
            let incoming = self.incoming.swap_remove(index);
            let req = (|| -> Result<Attach> {
                let n = result?;
                if n > MAX_FRAME {
                    return Err("oversized host request".into());
                }
                let req: Attach = serde_json::from_slice(&bytes[..n])?;
                if req.v != 1
                    || req.op != "run"
                    || !(1..=1000).contains(&req.rows)
                    || !(1..=1000).contains(&req.cols)
                {
                    return Err("invalid host request".into());
                }
                if req.configuration != self.configuration {
                    return Err("configuration differs from server; run serve and run with the same Goblins package".into());
                }
                if !self.manifest.goblins.contains_key(&req.name) {
                    return Err(format!(
                        "unknown goblin; available: {}",
                        self.manifest
                            .goblins
                            .keys()
                            .cloned()
                            .collect::<Vec<_>>()
                            .join(", ")
                    )
                    .into());
                }
                if self.active.is_some() {
                    return Err("one goblin at a time; exit the existing goblin first".into());
                }
                Ok(req)
            })();
            match req {
                Ok(req) => {
                    let worker = Worker::start(
                        self.manifest.goblins[&req.name].clone(),
                        self.workspace.clone(),
                        req.rows,
                        req.cols,
                    );
                    self.active = Some(Active {
                        name: req.name,
                        peer: incoming.peer,
                        worker,
                        listener: None,
                        identity: None,
                        packages: BTreeSet::new(),
                        reading: Vec::new(),
                        pending: None,
                        stopping: false,
                    });
                }
                Err(e) => {
                    let _ = unix::send(
                        incoming.peer.as_raw_fd(),
                        &serde_json::to_vec(
                            &serde_json::json!({"v":1,"status":"error","message":e.to_string()}),
                        )?,
                    );
                    events.push(Event::Detail(format!("Attach error: {e}")));
                }
            }
        }
        let Some(active) = &mut self.active else {
            return Ok(events);
        };
        // The attachment protocol has no commands after startup. Any input/EOF
        // releases its ownership, including while startup or a build is pending.
        if !active.stopping && unix::readable(active.peer.as_raw_fd(), 0)? {
            Self::stop(active);
        }
        let mut stopped = false;
        loop {
            let result = match active.worker.results.try_recv() {
                Ok(result) => result,
                Err(std::sync::mpsc::TryRecvError::Empty) => break,
                Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                    stopped = true;
                    break;
                }
            };
            match result {
                Completed::Started {
                    master,
                    listener,
                    identity,
                } if !active.stopping => {
                    if unix::send_terminal(active.peer.as_raw_fd(), master.as_raw_fd()).is_err() {
                        Self::stop(active);
                    } else {
                        active.listener = Some(listener);
                        active.identity = Some(identity);
                        events.push(Event::Connected(active.name.clone()));
                    }
                }
                Completed::Started { .. } => (),
                Completed::Granted { reply, detail } => {
                    if let Some(detail) = detail {
                        events.push(Event::Detail(detail));
                    }
                    if let Some(pending) = active.pending.take() {
                        if reply.status == "ready" {
                            active.packages.insert(pending.request.package);
                        }
                        send_reply(&pending.peer, &reply);
                        events.push(Event::Result(reply));
                    }
                }
                Completed::Failed(e) => {
                    let _ = unix::send(
                        active.peer.as_raw_fd(),
                        &serde_json::to_vec(
                            &serde_json::json!({"v":1,"status":"error","message":e}),
                        )?,
                    );
                    events.push(Event::Detail(e));
                    Self::stop(active);
                }
                Completed::Stopped => {
                    stopped = true;
                    break;
                }
            }
        }
        if stopped {
            self.active.take();
            events.push(Event::Stopped);
            return Ok(events);
        }
        if active.stopping {
            return Ok(events);
        }
        if active
            .pending
            .as_ref()
            .is_some_and(|p| !p.decided && unix::disconnected(p.peer.as_raw_fd()))
        {
            active.pending.take();
        }
        if let Some(listener) = &active.listener {
            for _ in 0..MAX_READING_CLIENTS {
                match listener.accept() {
                    Ok((peer, _)) if active.reading.len() < MAX_READING_CLIENTS => {
                        peer.set_nonblocking(true)?;
                        active.reading.push(IncomingRequest {
                            peer,
                            frame: Frame::new(now),
                        });
                    }
                    Ok(_) => (),
                    Err(e) if e.kind() == io::ErrorKind::WouldBlock => break,
                    Err(e) => return Err(e.into()),
                }
            }
        }
        let mut index = 0;
        while index < active.reading.len() {
            let incoming = &mut active.reading[index];
            let mut bytes = [0; MAX_FRAME + 1];
            let n = incoming.peer.read(&mut bytes);
            let result = match n {
                Ok(0) => Some(Err(Reply::new(
                    None,
                    "error",
                    Some("incomplete request".into()),
                ))),
                Ok(n) => frame_result(&mut incoming.frame, &bytes[..n], now),
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                    frame_result(&mut incoming.frame, &[], now)
                }
                Err(e) => Some(Err(Reply::new(None, "error", Some(e.to_string())))),
            };
            let Some(result) = result else {
                index += 1;
                continue;
            };
            let incoming = active.reading.swap_remove(index);
            match result {
                Ok(req) if active.pending.is_none() => {
                    let approval = ApprovalId(self.next_approval);
                    self.next_approval = self
                        .next_approval
                        .checked_add(1)
                        .ok_or("approval identity exhausted")?;
                    events.push(Event::Request {
                        request: req.clone(),
                        approval,
                    });
                    active.pending = Some(Pending {
                        approval,
                        peer: incoming.peer,
                        request: req,
                        decided: false,
                    });
                }
                Ok(req) => send_reply(
                    &incoming.peer,
                    &Reply::new(
                        Some(req.id),
                        "error",
                        Some("another package request is pending".into()),
                    ),
                ),
                Err(reply) => {
                    send_reply(&incoming.peer, &reply);
                    events.push(Event::Result(reply));
                }
            }
        }
        Ok(events)
    }
}
fn frame_result(
    frame: &mut Frame,
    bytes: &[u8],
    now: Instant,
) -> Option<std::result::Result<Request, Reply>> {
    match frame.push(bytes, now) {
        Ok(Some(data)) => Some(parse_request(data)),
        Ok(None) => None,
        Err(e) => Some(Err(Reply::new(None, "error", Some(e.into())))),
    }
}
fn send_reply(peer: &UnixStream, reply: &Reply) {
    if let Ok(bytes) = line(reply) {
        let _ = unix::send(peer.as_raw_fd(), &bytes);
    }
}
impl Drop for Controller {
    fn drop(&mut self) {
        self.active.take();
        let _ = fs::remove_file(&self.socket_path);
    }
}
