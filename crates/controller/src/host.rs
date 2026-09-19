//! Host API client shared by approval presentation and the launch CLI.
use crate::{
    Result,
    controller::{PermissionRecord, Snapshot},
};
use goblins_protocol::rpc::{self, Decoder};
use serde_json::{Value, json};
use std::{
    collections::VecDeque,
    io::{self, Read},
    os::unix::net::UnixStream,
    path::Path,
    time::Instant,
};
pub struct Client {
    pub instance: String,
    pub features: Vec<String>,
    stream: UnixStream,
    next: u64,
}
impl Client {
    pub fn connect(state: &Path) -> Result<Self> {
        Self::connect_optional(state)?
            .ok_or_else(|| "start the server with 'goblins server start' first".into())
    }
    /// Absence is distinct from a live endpoint that fails initialization.
    pub fn connect_optional(state: &Path) -> Result<Option<Self>> {
        let mut stream = match UnixStream::connect(state.join("host.sock")) {
            Ok(stream) => stream,
            Err(e)
                if matches!(
                    e.kind(),
                    io::ErrorKind::NotFound | io::ErrorKind::ConnectionRefused
                ) =>
            {
                return Ok(None);
            }
            Err(e) => return Err(e.into()),
        };
        let init = rpc::exchange(&mut stream, json!(1), "initialize", json!({"api":1}))?;
        if init["api"] != 1 || init["role"] != "host" {
            return Err("incompatible daemon".into());
        }
        Ok(Some(Self {
            features: serde_json::from_value(init["features"].clone())?,
            instance: init["instance"]
                .as_str()
                .ok_or("missing daemon identity")?
                .into(),
            stream,
            next: 1,
        }))
    }
    pub fn call(&mut self, method: &str, params: Value) -> Result<Value> {
        self.next += 1;
        Ok(rpc::exchange(
            &mut self.stream,
            json!(self.next),
            method,
            params,
        )?)
    }
    pub fn mailbox_call(
        &mut self,
        method: &str,
        params: Value,
    ) -> io::Result<std::result::Result<Value, Value>> {
        self.next += 1;
        goblins_protocol::messages::exchange(&mut self.stream, json!(self.next), method, params)
    }
    pub fn decide(&mut self, p: &PermissionRecord, approved: bool) -> Result<Value> {
        self.call(
            "permissions.decide",
            json!({"session":p.session,"request":p.id,"approval":p.approval,"approved":approved}),
        )
    }
    pub fn subscribe(mut self) -> Result<Subscription> {
        let value = self.call("state.subscribe", json!({}))?;
        self.stream.set_nonblocking(true)?;
        Ok(Subscription {
            stream: self.stream,
            decoder: Decoder::new(rpc::MAX_BODY, None),
            input: VecDeque::new(),
            id: value["subscription"]
                .as_str()
                .ok_or("missing subscription")?
                .into(),
            sequence: value["sequence"].as_u64().ok_or("missing sequence")?,
            snapshot: serde_json::from_value(value["snapshot"].clone())?,
        })
    }
}
pub struct Subscription {
    stream: UnixStream,
    decoder: Decoder,
    input: VecDeque<u8>,
    id: String,
    pub sequence: u64,
    pub snapshot: Snapshot,
}
impl Subscription {
    pub fn tick(&mut self) -> Result<bool> {
        let mut bytes = [0; 16384];
        let mut changed = false;
        match self.stream.read(&mut bytes) {
            Ok(0) => return Err("subscription disconnected; reconnect for a fresh snapshot".into()),
            Ok(n) => self.input.extend(&bytes[..n]),
            Err(e)
                if matches!(
                    e.kind(),
                    io::ErrorKind::WouldBlock | io::ErrorKind::Interrupted
                ) => {}
            Err(e) => return Err(e.into()),
        }
        // Bound work per UI iteration as well as transport memory.
        for _ in 0..16384 {
            let Some(byte) = self.input.pop_front() else {
                break;
            };
            if let (_, Some(body)) = self.decoder.push(&[byte], Instant::now())? {
                let value = rpc::parse(&body)?;
                let p = &value["params"];
                if value["jsonrpc"] != "2.0"
                    || value["method"] != "state.changed"
                    || p["subscription"] != self.id
                    || p["sequence"].as_u64() != Some(self.sequence + 1)
                {
                    return Err("subscription gap; reconnect for a fresh snapshot".into());
                }
                let snapshot: Snapshot = serde_json::from_value(p["snapshot"].clone())?;
                if snapshot.instance != self.snapshot.instance {
                    return Err("daemon instance changed; reconnect".into());
                }
                self.snapshot = snapshot;
                self.sequence += 1;
                changed = true;
                self.decoder = Decoder::new(rpc::MAX_BODY, None);
            }
        }
        self.decoder.push(&[], Instant::now())?;
        Ok(changed)
    }
}
/// Commands carry their immutable displayed identity in their text. Neither a
/// queued complete command nor a partial command can acquire a replacement ID.
pub fn decision<'a>(line: &str, snapshot: &'a Snapshot) -> Option<(&'a PermissionRecord, bool)> {
    let mut words = line.split_whitespace();
    let action = words.next()?;
    let token = words.next()?;
    if words.next().is_some() || !["approve", "deny"].contains(&action) {
        return None;
    }
    snapshot
        .permissions
        .iter()
        .find(|p| p.approval == token && p.state == "pending")
        .map(|p| (p, action == "approve"))
}
