//! Host API client shared by approval presentation and the launch CLI.
use crate::Result;
use crate::controller::PermissionRecord;
use crate::controller::Snapshot;
use goblins_jsonrpc::{self as rpc, Decoder};
use goblins_protocol::contract::{self, Method, Response};
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
    pub fn call_profile<P: contract::ParamsProfile>(
        &mut self,
        params: P::Params,
    ) -> Result<<P::Method as Method>::Result> {
        let wire = self.call(
            <P::Method as Method>::NAME,
            contract::profile_params::<P>(params)?,
        )?;
        Ok(contract::decode_reply::<P::Method>(wire)?)
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
    pub fn call_patient_typed<M: Method>(
        &mut self,
        params: M::Params,
        stop: &std::sync::atomic::AtomicBool,
    ) -> Result<M::Result> {
        let wire = self.call_patient(M::NAME, contract::params_value::<M>(params)?, stop)?;
        Ok(M::Result::decode(wire)?)
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
    pub fn decide(
        &mut self,
        p: &PermissionRecord,
        approved: bool,
    ) -> Result<(contract::PermissionDecisionReply, Value)> {
        self.call_typed_with_wire::<contract::host::PermissionsDecide>(
            contract::PermissionDecisionParams {
                session: p.session.clone(),
                request: p.id.clone(),
                approval: p.approval.clone(),
                approved,
            },
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
    fn raid_owner_profiles_send_exact_shown_payloads_and_ignore_legacy_successes() {
        use std::io::Write;
        let fixtures: Value =
            serde_json::from_str(include_str!("../../../libs/api/schema/fixtures.json")).unwrap();
        let cases = fixtures["raid_owner_requests"].as_array().unwrap().clone();
        let _no_forks = goblins_unix::no_forks();
        std::thread::scope(|scope| {
            let (stream, mut peer) = UnixStream::pair().unwrap();
            stream.set_read_timeout(Some(rpc::TIMEOUT)).unwrap();
            peer.set_read_timeout(Some(rpc::TIMEOUT)).unwrap();
            let mut client = Client {
                instance: "test".into(),
                features: vec![],
                stream,
                next: 0,
            };
            let expected = cases.clone();
            let server = scope.spawn(move || {
                for case in expected {
                    let call = rpc::read(&mut peer).unwrap();
                    assert_eq!(call["method"], case["method"]);
                    assert_eq!(call["params"], case["params"]);
                    peer.write_all(
                        &rpc::encode(&rpc::result(call["id"].clone(), json!(["legacy", null])))
                            .unwrap(),
                    )
                    .unwrap();
                }
            });
            for case in cases {
                if case["method"] == "raids.set_owner" {
                    client
                        .call_profile::<contract::host_param_profiles::RaidOwnerSetOwner>(
                            serde_json::from_value(case["params"].clone()).unwrap(),
                        )
                        .unwrap();
                } else {
                    client
                        .call_profile::<contract::host_param_profiles::RaidOwnerDestroy>(
                            serde_json::from_value(case["params"].clone()).unwrap(),
                        )
                        .unwrap();
                }
            }
            server.join().unwrap();
        });
    }
    #[test]
    fn host_raid_bindings_preserve_cli_request_presence_and_original_results() {
        use std::io::Write;
        let _no_forks = goblins_unix::no_forks();
        std::thread::scope(|scope| {
            let (stream, mut peer) = UnixStream::pair().unwrap();
            stream.set_read_timeout(Some(rpc::TIMEOUT)).unwrap();
            peer.set_read_timeout(Some(rpc::TIMEOUT)).unwrap();
            let mut client = Client {
                instance: "test".into(),
                features: vec![],
                stream,
                next: 0,
            };
            let replies = [
                json!({"extension":true}),
                Value::Null,
                json!({"accepted":"legacy"}),
                json!({"name":"crew","id":"r","members":[{"id":"a","agent_name":"alpha","options":{"model":7}}]}),
                json!([]),
                json!({"paused":true}),
            ];
            let outputs = replies.clone();
            let server = scope.spawn(move || {
                for (index, (method, params)) in [
                    ("raids.invite", json!({"raid":"crew","sessions":["a"]})),
                    ("raids.destroy", json!({"raid":"crew","stop_members":false})),
                    ("raids.set_owner", json!({"raid":"crew","session":"a"})),
                    ("raids.members", json!({"raid":"crew"})),
                    (
                        "raids.set_role",
                        json!({"raid":"crew","session":"a","role":"reviewer"}),
                    ),
                    ("raids.kick", json!({"raid":"crew","session":"b"})),
                ]
                .into_iter()
                .enumerate()
                {
                    let call = rpc::read(&mut peer).unwrap();
                    assert_eq!(call["method"], method);
                    assert_eq!(call["params"], params);
                    peer.write_all(
                        &rpc::encode(&rpc::result(call["id"].clone(), outputs[index].clone()))
                            .unwrap(),
                    )
                    .unwrap();
                }
            });
            assert_eq!(
                client
                    .call_typed_with_wire::<contract::host::RaidsInvite>(
                        contract::RaidInviteParams {
                            raid: "crew".into(),
                            sessions: vec!["a".into()]
                        }
                    )
                    .unwrap()
                    .1,
                replies[0]
            );
            assert_eq!(
                client
                    .call_typed_with_wire::<contract::host::RaidsDestroy>(
                        contract::RaidDestroyParams {
                            raid: "crew".into(),
                            id: None,
                            generation: None,
                            stop_members: false
                        }
                    )
                    .unwrap()
                    .1,
                replies[1]
            );
            assert_eq!(
                client
                    .call_typed_with_wire::<contract::host::RaidsSetOwner>(
                        contract::RaidSetOwnerParams {
                            raid: "crew".into(),
                            session: "a".into(),
                            id: None,
                            generation: None
                        }
                    )
                    .unwrap()
                    .1,
                replies[2]
            );
            let (display, wire) = client
                .call_typed_with_wire::<contract::host::RaidsMembers>(
                    contract::RaidSelectorParams {
                        raid: "crew".into(),
                    },
                )
                .unwrap();
            assert_eq!(wire, replies[3]);
            assert_eq!(display.name, "crew");
            assert_eq!(display.members[0].options, "model=?");
            assert_eq!(
                client
                    .call_typed_with_wire::<contract::host::RaidsSetRole>(
                        contract::RaidSetRoleParams {
                            raid: "crew".into(),
                            session: "a".into(),
                            role: "reviewer".into()
                        }
                    )
                    .unwrap()
                    .1,
                replies[4]
            );
            assert_eq!(
                client
                    .call_typed_with_wire::<contract::host::RaidsKick>(contract::RaidKickParams {
                        raid: "crew".into(),
                        session: "b".into()
                    })
                    .unwrap()
                    .1,
                replies[5]
            );
            server.join().unwrap();
        });
    }

    #[test]
    fn host_job_bindings_preserve_original_wire_for_printing_and_status_fallbacks() {
        use std::io::Write;
        let _no_forks = goblins_unix::no_forks();
        std::thread::scope(|scope| {
            let (stream, mut peer) = UnixStream::pair().unwrap();
            stream.set_read_timeout(Some(rpc::TIMEOUT)).unwrap();
            peer.set_read_timeout(Some(rpc::TIMEOUT)).unwrap();
            let mut client = Client {
                instance: "test".into(),
                features: vec![],
                stream,
                next: 0,
            };
            let outputs = [
                Value::Null,
                json!({"accepted":"legacy","extra":[1]}),
                json!({"request":7,"status":"finished","code":4294967296i64,"message":"done"}),
                json!([]),
            ];
            let replies = outputs.clone();
            let server = scope.spawn(move || {
                for (index, method) in ["host.list", "host.cancel", "host.status", "host.status"]
                    .into_iter()
                    .enumerate()
                {
                    let call = rpc::read(&mut peer).unwrap();
                    assert_eq!(call["method"], method);
                    assert_eq!(
                        call["params"],
                        if index == 0 {
                            json!({})
                        } else {
                            json!({"request":"job"})
                        }
                    );
                    peer.write_all(
                        &rpc::encode(&rpc::result(call["id"].clone(), replies[index].clone()))
                            .unwrap(),
                    )
                    .unwrap();
                }
            });
            let (_, wire) = client
                .call_typed_with_wire::<contract::host::HostList>(contract::EmptyParams {})
                .unwrap();
            assert_eq!(wire, outputs[0]);
            let (_, wire) = client
                .call_typed_with_wire::<contract::host::HostCancel>(contract::HostJobParams {
                    request: "job".into(),
                })
                .unwrap();
            assert_eq!(wire, outputs[1]);
            let (reply, wire) = client
                .call_typed_with_wire::<contract::host::HostStatus>(contract::HostJobParams {
                    request: "job".into(),
                })
                .unwrap();
            assert_eq!(wire, outputs[2]);
            assert_eq!(reply.code, Some(4294967296));
            assert_eq!(reply.request, None);
            assert_eq!(reply.status.as_deref(), Some("finished"));
            let (reply, wire) = client
                .call_typed_with_wire::<contract::host::HostStatus>(contract::HostJobParams {
                    request: "job".into(),
                })
                .unwrap();
            assert_eq!(wire, outputs[3]);
            assert!(reply.code.is_none() && reply.status.is_none());
            server.join().unwrap();
        });
    }

    #[test]
    fn permission_decisions_bind_generated_calls_and_preserve_plain_wire_results() {
        use std::io::Write;
        let _no_forks = goblins_unix::no_forks();
        let fixtures: Value =
            serde_json::from_str(include_str!("../../../libs/api/schema/fixtures.json")).unwrap();
        let cases = fixtures["permission_decision_result_cases"]
            .as_array()
            .unwrap()
            .clone();
        let record = PermissionRecord {
            kind: "package".into(),
            id: "a".into(),
            session: "session-a".into(),
            agent_name: "snikk".into(),
            approval: "approval-a".into(),
            package: "hello".into(),
            reason: "tests".into(),
            state: "pending".into(),
            approved: None,
            preview: None,
            message: None,
            name: None,
            started_at: None,
            log: None,
        };
        let (stream, mut peer) = UnixStream::pair().unwrap();
        stream.set_read_timeout(Some(rpc::TIMEOUT)).unwrap();
        peer.set_read_timeout(Some(rpc::TIMEOUT)).unwrap();
        // Keep the client inside the scope: unwinding closes its socket,
        // then joins the server before the no-forks guard can be released.
        std::thread::scope(|scope| {
            let mut client = Client {
                instance: "test".into(),
                features: vec![],
                stream,
                next: 0,
            };
            let replies = cases.clone();
            let server = scope.spawn(move || {
                for (i, case) in replies.iter().enumerate() {
                    let call = rpc::read(&mut peer).unwrap();
                    assert_eq!(call["method"], contract::host::PermissionsDecide::NAME);
                    assert_eq!(
                        call["params"],
                        json!({"session":"session-a","request":"a","approval":"approval-a","approved":i%2==0})
                    );
                    peer.write_all(
                        &rpc::encode(&rpc::result(call["id"].clone(), case["result"].clone())).unwrap(),
                    )
                    .unwrap();
                }
            });
            for (i, case) in cases.iter().enumerate() {
                let (reply, wire) = client.decide(&record, i % 2 == 0).unwrap();
                assert_eq!(wire, case["result"]);
                assert_eq!(
                    reply.accepted,
                    wire.get("accepted").and_then(Value::as_bool)
                );
                assert_eq!(
                    reply.request.as_deref(),
                    wire.get("request").and_then(Value::as_str)
                );
                assert_eq!(
                    reply.approved,
                    wire.get("approved").and_then(Value::as_bool)
                );
            }
            server.join().unwrap();
        });
    }

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
