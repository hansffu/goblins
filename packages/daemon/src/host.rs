//! Host API client shared by approval presentation and the launch CLI.
use crate::Result;
use crate::controller::PermissionRecord;
use crate::controller::Snapshot;
use goblins_jsonrpc::{self as rpc, Decoder};
use goblins_protocol::contract::{self, Method, Notification, Response};
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
        let value =
            self.call_typed::<contract::host::StateSubscribe>(contract::MailboxStatusParams {})?;
        self.stream.set_nonblocking(true)?;
        Ok(Subscription {
            stream: self.stream,
            decoder: Decoder::new(rpc::MAX_BODY, None),
            input: VecDeque::new(),
            id: value.subscription.ok_or("missing subscription")?,
            sequence: value.sequence.ok_or("missing sequence")?,
            snapshot: serde_json::from_value(value.snapshot)?,
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
                let value = contract::StateChangedRead::project(&value);
                if value.jsonrpc.as_deref() != Some("2.0")
                    || value.method.as_deref()
                        != Some(contract::host_notifications::StateChanged::NAME)
                    || value.subscription.as_deref() != Some(self.id.as_str())
                    || value.sequence != Some(self.sequence + 1)
                {
                    return Err("subscription gap; reconnect for a fresh snapshot".into());
                }
                let snapshot: Snapshot = serde_json::from_value(value.snapshot)?;
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
    fn communications_profile_preserves_requests_and_independent_page_fallbacks() {
        use std::io::Write;
        let fixtures: Value =
            serde_json::from_str(include_str!("../../../libs/api/schema/fixtures.json")).unwrap();
        let pages = fixtures["communications_page_cases"]
            .as_array()
            .unwrap()
            .clone();
        let _no_forks = goblins_unix::no_forks();
        std::thread::scope(|scope| {
            let (stream, mut peer) = UnixStream::pair().unwrap();
            stream.set_read_timeout(Some(rpc::TIMEOUT)).unwrap();
            peer.set_read_timeout(Some(rpc::TIMEOUT)).unwrap();
            let mut client = Client {
                instance: "i".into(),
                features: vec![],
                stream,
                next: 0,
            };
            let outputs = pages.clone();
            let server = scope.spawn(move || {
                for (index, page) in outputs.into_iter().enumerate() {
                    let call = rpc::read(&mut peer).unwrap();
                    assert_eq!(call["method"], "communications.list");
                    assert_eq!(
                        call["params"],
                        if index % 2 == 0 {
                            json!({"after":0,"session":null})
                        } else {
                            json!({"after":3,"session":"alpha"})
                        }
                    );
                    peer.write_all(&rpc::encode(&rpc::result(call["id"].clone(), page)).unwrap())
                        .unwrap();
                }
            });
            for (index, page) in pages.into_iter().enumerate() {
                let result = client
                    .call_profile::<contract::host_param_profiles::CommunicationsCli>(
                        contract::CommunicationsCliParams {
                            after: if index % 2 == 0 { 0 } else { 3 },
                            session: if index % 2 == 0 {
                                None
                            } else {
                                Some("alpha".into())
                            },
                        },
                    )
                    .unwrap();
                assert_eq!(result.events.as_ref(), page["events"].as_array());
                assert_eq!(result.cursor, page["cursor"].as_u64());
                assert_eq!(result.latest, page["latest"].as_u64());
                assert_eq!(result.session.as_deref(), page["session"].as_str());
            }
            server.join().unwrap();
        });
    }
    #[test]
    fn adapter_status_boundary_accepts_native_fixture_and_rejects_missing_revision() {
        use std::io::Write;
        let fixtures: Value =
            serde_json::from_str(include_str!("../../../libs/api/schema/fixtures.json")).unwrap();
        let valid = fixtures["native_inbox_status"].clone();
        let mut missing = valid.clone();
        missing.as_object_mut().unwrap().remove("revision");
        let _no_forks = goblins_unix::no_forks();
        std::thread::scope(|scope| {
            let (mut client, mut peer) = UnixStream::pair().unwrap();
            client.set_read_timeout(Some(rpc::TIMEOUT)).unwrap();
            peer.set_read_timeout(Some(rpc::TIMEOUT)).unwrap();
            let server = scope.spawn(move || {
                for result in [valid, missing] {
                    let call = rpc::read(&mut peer).unwrap();
                    assert_eq!(call["method"], "inbox.status");
                    assert_eq!(call["params"], json!({}));
                    peer.write_all(&rpc::encode(&rpc::result(call["id"].clone(), result)).unwrap())
                        .unwrap();
                }
            });
            assert!(
                contract::exchange::<contract::sandbox::InboxStatus>(
                    &mut client,
                    json!(1),
                    contract::MailboxStatusParams {}
                )
                .is_ok()
            );
            assert!(
                contract::exchange::<contract::sandbox::InboxStatus>(
                    &mut client,
                    json!(2),
                    contract::MailboxStatusParams {}
                )
                .is_err()
            );
            server.join().unwrap();
        });
    }
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
    fn c15_snapshot() -> Snapshot {
        serde_json::from_value(json!({"instance":"i","sessions":[],"permissions":[]})).unwrap()
    }
    fn c15_subscription() -> (Subscription, UnixStream) {
        let (stream, peer) = UnixStream::pair().unwrap();
        stream.set_nonblocking(true).unwrap();
        peer.set_read_timeout(Some(rpc::TIMEOUT)).unwrap();
        peer.set_write_timeout(Some(rpc::TIMEOUT)).unwrap();
        (
            Subscription {
                stream,
                decoder: Decoder::new(rpc::MAX_BODY, None),
                input: VecDeque::new(),
                id: "sub".into(),
                sequence: 1,
                snapshot: c15_snapshot(),
            },
            peer,
        )
    }
    fn c15_change(sequence: u64, snapshot: Value) -> Value {
        json!({"jsonrpc":"2.0","method":"state.changed","params":{"subscription":"sub","sequence":sequence,"snapshot":snapshot}})
    }
    #[test]
    fn c15_subscribe_preserves_outbound_request_initial_defaults_and_errors() {
        let _no_forks = goblins_unix::no_forks();
        let snapshot = json!({"instance":"i","sessions":[],"permissions":[]});
        for (reply, error) in [
            (
                json!({"subscription":"sub","sequence":0,"snapshot":snapshot,"future":true}),
                None,
            ),
            (
                json!({"subscription":"sub","sequence":u64::MAX,"snapshot":snapshot}),
                None,
            ),
            (
                json!({"subscription":"sub","sequence":0,"snapshot":{"instance":"other","sessions":[],"permissions":[]}}),
                None,
            ),
            (
                json!({"sequence":0,"snapshot":snapshot}),
                Some("missing subscription"),
            ),
            (
                json!({"subscription":17,"sequence":0,"snapshot":snapshot}),
                Some("missing subscription"),
            ),
            (
                json!({"subscription":"sub","snapshot":snapshot}),
                Some("missing sequence"),
            ),
            (
                json!({"subscription":"sub","sequence":1.0,"snapshot":snapshot}),
                Some("missing sequence"),
            ),
            (
                json!({"subscription":"sub","sequence":0,"snapshot":{"sessions":[],"permissions":[]}}),
                Some("missing field `instance`"),
            ),
        ] {
            std::thread::scope(|scope| {
                let (stream, mut peer) = UnixStream::pair().unwrap();
                stream.set_read_timeout(Some(rpc::TIMEOUT)).unwrap();
                stream.set_write_timeout(Some(rpc::TIMEOUT)).unwrap();
                peer.set_read_timeout(Some(rpc::TIMEOUT)).unwrap();
                peer.set_write_timeout(Some(rpc::TIMEOUT)).unwrap();
                let sequence = reply["sequence"].as_u64();
                let expected_instance = reply["snapshot"]["instance"].as_str().map(str::to_owned);
                let server = scope.spawn(move || {
                    let call = rpc::read(&mut peer).unwrap();
                    assert_eq!(
                        call,
                        json!({"jsonrpc":"2.0","id":1,"method":"state.subscribe","params":{}})
                    );
                    peer.write_all(&rpc::encode(&rpc::result(call["id"].clone(), reply)).unwrap())
                        .unwrap();
                });
                let client = Client {
                    instance: "i".into(),
                    features: vec![],
                    stream,
                    next: 0,
                };
                match (client.subscribe(), error) {
                    (Ok(subscription), None) => {
                        assert_eq!(subscription.id, "sub");
                        assert_eq!(Some(subscription.sequence), sequence);
                        assert_eq!(
                            json!(subscription.snapshot),
                            json!({"instance":expected_instance.unwrap(),"sessions":[],"permissions":[],"raids":[],"messaging":{"audit_failed":false,"raid_audit_failed":false}})
                        );
                        drop(subscription);
                    }
                    (Err(actual), Some(expected)) => assert_eq!(actual.to_string(), expected),
                    (Ok(_), Some(expected)) => panic!("accepted invalid initial result: {expected}"),
                    (Err(error), None) => panic!("refused legacy initial result: {error}"),
                }
                server.join().unwrap();
            });
        }
    }

    #[test]
    fn c15_tick_preserves_partial_frames_budget_replacement_and_drop() {
        let _no_forks = goblins_unix::no_forks();
        let (mut sub, mut peer) = c15_subscription();
        assert!(!sub.tick().unwrap());
        let mut replacement = json!(c15_snapshot());
        replacement["messaging"]["audit_failed"] = json!(true);
        // The original reader ignores additive outer fields and even an id.
        let mut additive = c15_change(2, replacement.clone());
        additive["id"] = json!(17);
        additive["future"] = json!(true);
        let frame = rpc::encode(&additive).unwrap();
        let half = frame.len() / 2;
        sub.input.extend(&frame[..half]);
        assert!(!sub.tick().unwrap());
        assert_eq!(sub.sequence, 1);
        sub.input.extend(&frame[half..]);
        assert!(sub.tick().unwrap());
        assert_eq!(sub.sequence, 2);
        assert!(sub.snapshot.messaging.audit_failed);
        let mut large = c15_change(3, json!(c15_snapshot()));
        large["padding"] = json!("x".repeat(20_000));
        sub.input.extend(rpc::encode(&large).unwrap());
        assert!(!sub.tick().unwrap());
        assert_eq!(sub.sequence, 2);
        assert!(sub.tick().unwrap());
        assert_eq!(sub.sequence, 3);
        assert!(!sub.snapshot.messaging.audit_failed);
        assert!(!sub.tick().unwrap());
        drop(sub);
        assert_eq!(
            peer.read(&mut [0u8; 1]).unwrap(),
            0,
            "dropping subscription closes stream without another RPC"
        );
    }

    #[test]
    fn c15_tick_rejects_bad_envelopes_instances_and_eof_without_replacing_state() {
        let _no_forks = goblins_unix::no_forks();
        let good = c15_change(2, json!(c15_snapshot()));
        let mut cases = Vec::new();
        for field in ["jsonrpc", "method"] {
            let mut bad = good.clone();
            bad[field] = json!("wrong");
            cases.push((bad, "subscription gap; reconnect for a fresh snapshot"));
        }
        for (field, value) in [
            ("subscription", json!("other")),
            ("sequence", json!(1)),
            ("sequence", json!(3)),
            ("sequence", json!(2.0)),
        ] {
            let mut bad = good.clone();
            bad["params"][field] = value;
            cases.push((bad, "subscription gap; reconnect for a fresh snapshot"));
        }
        let mut changed = good.clone();
        changed["params"]["snapshot"]["instance"] = json!("other");
        cases.push((changed, "daemon instance changed; reconnect"));
        let mut missing = good.clone();
        missing["params"]["snapshot"]
            .as_object_mut()
            .unwrap()
            .remove("permissions");
        cases.push((missing, "missing field `permissions`"));
        for (frame, expected) in cases {
            let (mut sub, _peer) = c15_subscription();
            sub.input.extend(rpc::encode(&frame).unwrap());
            assert_eq!(sub.tick().unwrap_err().to_string(), expected, "{frame}");
            assert_eq!(sub.sequence, 1);
            assert_eq!(json!(sub.snapshot), json!(c15_snapshot()));
        }
        let (mut sub, peer) = c15_subscription();
        sub.input.extend(rpc::encode(&good).unwrap());
        drop(peer);
        assert_eq!(
            sub.tick().unwrap_err().to_string(),
            "subscription disconnected; reconnect for a fresh snapshot"
        );
        assert_eq!(
            sub.sequence, 1,
            "EOF is observed before buffered input processing"
        );
    }

    #[test]
    fn c15_tick_keeps_committed_replacements_before_a_later_error() {
        let _no_forks = goblins_unix::no_forks();
        let (mut sub, _peer) = c15_subscription();
        let mut replacement = json!(c15_snapshot());
        replacement["messaging"]["raid_audit_failed"] = json!(true);
        sub.input
            .extend(rpc::encode(&c15_change(2, replacement.clone())).unwrap());
        sub.input
            .extend(rpc::encode(&c15_change(4, json!(c15_snapshot()))).unwrap());
        assert_eq!(
            sub.tick().unwrap_err().to_string(),
            "subscription gap; reconnect for a fresh snapshot"
        );
        assert_eq!(sub.sequence, 2);
        assert_eq!(json!(sub.snapshot), replacement);
    }

}
