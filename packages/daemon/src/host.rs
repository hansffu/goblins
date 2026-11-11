//! Host API client shared by approval presentation and the launch CLI.
use crate::Result;
use crate::controller::PermissionRecord;
use crate::controller::Snapshot;
use goblins_jsonrpc::{self as rpc, Decoder};
use goblins_protocol::contract::{self, Method};
use serde_json::{Value, json};
use std::{
    collections::VecDeque,
    io::{self, Read, Write},
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

/// Borrowed stdio capabilities; descriptor numbers never enter the JSON DTO.
pub enum ExecStdio<'a> {
    Tty(&'a std::os::fd::OwnedFd),
    Fds([&'a std::os::fd::OwnedFd; 3]),
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
        let init = contract::initialize(&mut stream, json!(1), contract::EndpointRole::Host)?;
        Ok(Some(Self {
            features: init.features,
            instance: init.instance,
            stream,
            next: 1,
        }))
    }
    pub fn call_typed<M: Method>(&mut self, params: M::Params) -> Result<M::Result> {
        self.next += 1;
        Ok(contract::exchange::<M>(
            &mut self.stream,
            json!(self.next),
            params,
        )?)
    }
    pub fn call_typed_with_wire<M: Method>(
        &mut self,
        params: M::Params,
    ) -> Result<(M::Result, Value)> {
        self.next += 1;
        Ok(contract::exchange_with_wire::<M>(
            &mut self.stream,
            json!(self.next),
            params,
        )?)
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
    /// A call whose reply may take as long as its work does, such as a
    /// terminal wait: the reply is waited for without a deadline, until
    /// `stop` is set. Giving up drops this connection, which cancels the call.
    pub fn call_patient(
        &mut self,
        method: &str,
        params: Value,
        stop: &std::sync::atomic::AtomicBool,
    ) -> Result<Value> {
        use std::os::fd::AsRawFd;
        self.next += 1;
        let id = json!(self.next);
        self.stream.set_write_timeout(Some(rpc::TIMEOUT))?;
        self.stream
            .write_all(&rpc::encode(&rpc::request(id.clone(), method, params))?)?;
        while !goblins_unix::readable(self.stream.as_raw_fd(), 100)? {
            if stop.load(std::sync::atomic::Ordering::Relaxed) {
                return Err("interrupted".into());
            }
        }
        let reply = rpc::read(&mut self.stream)?;
        rpc::validate_response(&reply, &id)?;
        if let Some(error) = reply.get("error") {
            return Err(error["message"]
                .as_str()
                .unwrap_or("the daemon refused the call")
                .into());
        }
        Ok(reply["result"].clone())
    }
    /// Send a request with descriptors attached, without reading its reply.
    /// Returns the request ID.
    pub fn send_with_fds(
        &mut self,
        method: &str,
        params: Value,
        fds: &[std::os::fd::RawFd],
    ) -> Result<Value> {
        use std::os::fd::AsRawFd;
        self.next += 1;
        let id = json!(self.next);
        let frame = rpc::encode(&rpc::request(id.clone(), method, params))?;
        self.stream.set_write_timeout(Some(rpc::TIMEOUT))?;
        // One message: the descriptors arrive with the request's first byte.
        goblins_unix::ancillary::send(
            self.stream.as_raw_fd(),
            &frame,
            fds,
            goblins_protocol::HOST_MAX_FDS,
        )?;
        Ok(id)
    }
    /// Start the generated exec contract while retaining descriptor ownership
    /// in the caller until the send completes. Completion is read by the relay.
    pub fn send_exec(
        &mut self,
        params: contract::ExecParams,
        stdio: ExecStdio<'_>,
    ) -> Result<Value> {
        use std::os::fd::AsRawFd;
        let fds = match stdio {
            ExecStdio::Tty(fd) if params.tty => vec![fd.as_raw_fd()],
            ExecStdio::Fds(fds) if !params.tty => fds.into_iter().map(AsRawFd::as_raw_fd).collect(),
            _ => return Err("exec stdio does not match tty".into()),
        };
        self.send_with_fds(
            contract::host::SessionsExec::NAME,
            contract::params_value::<contract::host::SessionsExec>(params)?,
            &fds,
        )
    }
    /// The connection itself, for a caller that keeps it open (an exec).
    pub fn into_stream(self) -> UnixStream {
        self.stream
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generated_exec_send_binds_stdio_without_consuming_borrowed_descriptors() {
        use std::os::fd::AsRawFd;
        let (stream, peer) = UnixStream::pair().unwrap();
        let mut client = Client {
            instance: String::new(),
            features: vec![],
            stream,
            next: 1,
        };
        let fd = goblins_unix::pipe().unwrap().0;
        for tty in [false, true] {
            let params = contract::ExecParams {
                session: "a".into(),
                pin: None,
                argv: None,
                cwd: None,
                env: vec![],
                tty,
                interactive: false,
            };
            if !tty {
                assert!(
                    client
                        .send_exec(params.clone(), ExecStdio::Tty(&fd))
                        .is_err()
                );
                assert_eq!(client.next, 1);
            }
            let stdio = if tty {
                ExecStdio::Tty(&fd)
            } else {
                ExecStdio::Fds([&fd, &fd, &fd])
            };
            let id = client.send_exec(params, stdio).unwrap();
            let mut frame = [0; 4096];
            let (length, received) =
                goblins_unix::ancillary::receive(peer.as_raw_fd(), &mut frame, 0).unwrap();
            assert_eq!(received.len(), if tty { 1 } else { 3 });
            let body = frame[..length]
                .windows(4)
                .position(|s| s == b"\r\n\r\n")
                .unwrap()
                + 4;
            let call = rpc::parse(&frame[body..length]).unwrap();
            assert_eq!(call["id"], id);
            assert_eq!(call["method"], contract::host::SessionsExec::NAME);
            assert_eq!(
                call["params"],
                json!({"session":"a","env":[],"tty":tty,"interactive":false})
            );
            assert!(unsafe { libc::fcntl(fd.as_raw_fd(), libc::F_GETFD) } >= 0);
        }
        // No descriptor number or capability is ever part of the JSON object.
        assert_eq!(client.next, 3);
    }
    use std::os::fd::AsRawFd;

    #[test]
    fn host_client_refuses_four_descriptors_before_sending() {
        let _no_forks = goblins_unix::no_forks();
        let (stream, peer) = UnixStream::pair().unwrap();
        let mut client = Client {
            instance: "test".into(),
            features: vec![],
            stream,
            next: 1,
        };
        let (_reader, writer) = goblins_unix::pipe().unwrap();
        let fd = writer.as_raw_fd();
        assert_eq!(
            client
                .send_with_fds("sessions.exec", json!({"session":"a"}), &[fd; 4])
                .unwrap_err()
                .to_string(),
            "too many descriptors"
        );
        assert_eq!(
            client.next, 2,
            "preserve request-id consumption on failed sends"
        );
        assert!(unsafe { libc::fcntl(fd, libc::F_GETFD) } >= 0);
        assert_eq!(
            goblins_unix::ancillary::receive(peer.as_raw_fd(), &mut [0; 1024], libc::MSG_DONTWAIT)
                .unwrap_err()
                .kind(),
            io::ErrorKind::WouldBlock
        );
        client
            .send_with_fds("sessions.exec", json!({"session":"a"}), &[fd; 3])
            .unwrap();
        let (n, fds) =
            goblins_unix::ancillary::receive(peer.as_raw_fd(), &mut [0; 1024], 0).unwrap();
        assert!(n > 0);
        assert_eq!(fds.len(), 3);
    }

    #[test]
    fn a_patient_call_gives_up_when_asked_and_drops_its_connection() {
        let (stream, mut peer) = UnixStream::pair().unwrap();
        let mut client = Client {
            instance: String::new(),
            features: vec![],
            stream,
            next: 1,
        };
        let stop = std::sync::atomic::AtomicBool::new(false);
        let error = std::thread::scope(|scope| {
            scope.spawn(|| {
                std::thread::sleep(std::time::Duration::from_millis(250));
                stop.store(true, std::sync::atomic::Ordering::Relaxed);
            });
            // The daemon never answers.
            client.call_patient("terminals.wait", json!({}), &stop)
        })
        .unwrap_err();
        assert_eq!(error.to_string(), "interrupted");
        // The request went out, and closing the client closes the connection.
        assert!(rpc::read(&mut peer).unwrap()["method"] == "terminals.wait");
        drop(client);
        assert_eq!(peer.read(&mut [0; 1]).unwrap(), 0);
    }
}
