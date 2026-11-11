//! Shared synchronous one-shot control; never called from the daemon event pump.
use super::{Child, EXIT_GRACE, Executable};
use crate::Result;
use goblins_protocol::contract::supervisor_phases::{MaintenanceResult, ManifestResult};
use goblins_runtime::{
    Event,
    channel::Channel,
    supervisor::{Decoder, Encoder, Message, Received, outbound::Writer},
};
use std::{path::PathBuf, time::Instant};
pub(super) enum Expected {
    Manifest { path: PathBuf, sequence: u64 },
    Maintenance,
}
impl Expected {
    fn allows(&self, method: &str) -> bool {
        match self {
            Self::Manifest { .. } => ManifestResult::allows(method),
            Self::Maintenance => MaintenanceResult::allows(method),
        }
    }
    fn error(&self, before: &str, after: &str) -> Box<dyn std::error::Error + Send + Sync> {
        let name = match self {
            Self::Manifest { .. } => "preflight",
            Self::Maintenance => "maintenance",
        };
        if before.is_empty() {
            format!("{name} {after}").into()
        } else {
            format!("{before} {name} {after}").into()
        }
    }
}
pub(super) fn request(
    executable: &Executable,
    message: Message,
    expected: Expected,
    cancel: impl Fn() -> bool,
) -> Result<Message> {
    if cancel() {
        return Err("session cancelled".into());
    }
    let (child, socket) = match expected {
        Expected::Maintenance => Child::start_maintenance(executable.path()?)?,
        Expected::Manifest { .. } => Child::start(executable.path()?)?,
    };
    let mut attempt = Attempt {
        child: Some(child),
        channel: Channel::new(socket)?,
        decoder: Decoder::default(),
        encoder: Encoder::default(),
        writer: Writer::default(),
        result: None,
        extra: None,
        deadline: None,
        expected,
    };
    attempt
        .writer
        .begin(attempt.encoder.prepare(message)?)
        .map_err(|_| attempt.expected.error("", "writer busy"))?;
    attempt.exchange(cancel)?;
    Ok(attempt.result.take().unwrap())
}
struct Attempt {
    child: Option<Child>,
    channel: Channel,
    decoder: Decoder,
    encoder: Encoder,
    writer: Writer,
    // Full typed messages survive malformed transcripts/EOF until confirmed reap.
    result: Option<Message>,
    extra: Option<Message>,
    deadline: Option<Instant>,
    expected: Expected,
}
impl Attempt {
    fn receive_result(&mut self) -> Result<Option<Received>> {
        let frame = match self.channel.receive() {
            Ok(Some(frame)) => frame,
            Ok(None) => {
                if self.channel.closed() {
                    self.decoder.finish()?;
                }
                return Ok(None);
            }
            // Preserve the common codec's EOF handling after Channel has
            // disposed raw descriptors and closed the reset transport.
            Err(error) if error.kind() == std::io::ErrorKind::ConnectionReset => {
                self.decoder.finish()?;
                return Ok(None);
            }
            Err(error) => return Err(error.into()),
        };
        if !matches!(
            frame.value.get("method").and_then(|v| v.as_str()),
            Some(method) if self.expected.allows(method)
        ) {
            // Both legal results are single-frame. Reject a wrong direction
            // before the general decoder could retain a partial Handoff and
            // leave us waiting indefinitely for a complete logical result.
            drop(frame);
            self.decoder.close(&mut self.channel);
            return Err(self.expected.error("invalid", "result method"));
        }
        let received = self
            .decoder
            .receive(frame)?
            .ok_or_else(|| self.expected.error("incomplete", "result"))?;
        Ok(Some(received))
    }
    fn exchange(&mut self, cancel: impl Fn() -> bool) -> Result<()> {
        loop {
            if cancel() {
                return Err("session cancelled".into());
            }
            if self.deadline.is_some_and(|d| Instant::now() >= d) {
                return Err(self
                    .expected
                    .error("", "did not finish within 2 seconds after its result"));
            }
            // EOF can precede the child's final destructors and reap. Do not
            // flush a closed transport while waiting; the drained check below
            // still rejects any acknowledgement that did not actually flush.
            if !self.channel.closed() {
                self.writer.pump(&mut self.channel)?;
            }
            if let Some(received) = self.receive_result()? {
                if self.result.is_some() {
                    self.extra = Some(received.message);
                    return Err(self.expected.error("extra", "result"));
                }
                self.deadline = Some(Instant::now() + EXIT_GRACE);
                self.result = Some(received.message);
                match (&self.expected, self.result.as_ref().unwrap()) {
                    (
                        Expected::Manifest { path, sequence },
                        Message::Event(Event::Manifest {
                            sequence: actual,
                            lease,
                            ..
                        }),
                    ) if actual == sequence && &lease.path == path => {
                        let transfer = received
                            .adopted
                            .ok_or_else(|| self.expected.error("", "manifest lacks adoption"))?;
                        self.writer
                            .begin(self.encoder.prepare(Message::Adopted { transfer })?)
                            .map_err(|_| self.expected.error("", "acknowledgement writer busy"))?;
                    }
                    (Expected::Manifest { .. }, Message::PreflightFailed { .. })
                    | (Expected::Maintenance, Message::MaintenanceFinished { .. })
                        if received.adopted.is_none() =>
                    {
                        ()
                    }
                    _ => return Err(self.expected.error("invalid", "result")),
                }
            }
            let reaped = self.child.as_mut().unwrap().reap()?;
            if self.channel.closed() {
                if self.result.is_none() {
                    return Err(self.expected.error("", "closed without a result"));
                }
                if !self.writer.drained(&self.channel) {
                    let what = match self.expected {
                        Expected::Manifest { .. } => "acknowledgement",
                        Expected::Maintenance => "request",
                    };
                    return Err(self
                        .expected
                        .error("", &format!("closed before {what} flushed")));
                }
                if self.deadline.is_some_and(|d| Instant::now() >= d) {
                    return Err(self
                        .expected
                        .error("", "did not finish within 2 seconds after its result"));
                }
                if reaped {
                    if !self.child.as_ref().unwrap().successful() {
                        return Err(self.expected.error("", "supervisor exited unsuccessfully"));
                    }
                    self.child.take(); // already reaped
                    return Ok(());
                }
            }
            wait(&self.channel);
        }
    }
}
impl Drop for Attempt {
    fn drop(&mut self) {
        // Dispose partial/raw capabilities before closing cancellation control.
        self.decoder.close(&mut self.channel);
        if let Some(mut child) = self.child.take() {
            // A decoded result's fixed deadline is never reset or extended.
            let deadline = self.deadline.unwrap_or_else(|| Instant::now() + EXIT_GRACE);
            while Instant::now() < deadline {
                match child.reap() {
                    Ok(true) => break,
                    Err(_) => break,
                    Ok(false) => std::thread::sleep(std::time::Duration::from_millis(10)),
                }
            }
            drop(child); // kill if necessary, then confirmed reap
        }
        self.writer.close(&mut self.channel);
        // result/extra owners are dropped only after the child above.
    }
}
fn wait(channel: &Channel) {
    if let Some(fd) = channel.fd() {
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
            libc::poll(&mut poll, 1, 10);
        }
    } else {
        // EOF can precede process destructors; the pidfd is sampled next turn.
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
}
#[cfg(test)]
mod c16c_tests {
    use super::*;
    use goblins_runtime::{channel::Frame, config::ManifestLease};
    use serde_json::json;
    use std::{
        fs,
        io::Read,
        os::fd::AsRawFd,
        os::unix::net::UnixStream,
    };

    fn attempt(expected: Expected) -> (Channel, Attempt) {
        let (left, right) = UnixStream::pair().unwrap();
        (
            Channel::new(left).unwrap(),
            Attempt {
                child: None,
                channel: Channel::new(right).unwrap(),
                decoder: Decoder::default(),
                encoder: Encoder::default(),
                writer: Writer::default(),
                result: None,
                extra: None,
                deadline: None,
                expected,
            },
        )
    }
    fn expected(manifest: bool) -> Expected {
        if manifest {
            Expected::Manifest {
                path: "/phase-test-manifest".into(),
                sequence: 7,
            }
        } else {
            Expected::Maintenance
        }
    }
    fn frame(message: Message) -> Frame {
        let mut prepared = Encoder::default().prepare(message).unwrap();
        assert_eq!(prepared.frames.len(), 1);
        prepared.frames.pop_front().unwrap()
    }
    fn send(sender: &mut Channel, frame: &mut Frame) {
        sender.queue(&frame.value, &mut frame.fds).unwrap();
        sender.flush().unwrap();
        assert!(!sender.wants_write());
    }
    fn manifest() -> Message {
        Message::Event(Event::Manifest {
            sequence: 7,
            lease: ManifestLease::adopt("/phase-test-manifest".into(), None),
            catalogue: Default::default(),
        })
    }

    #[test]
    fn c16c_expected_allowlists_use_actual_producer_identities() {
        let cases = vec![
            (manifest(), true, false),
            (
                Message::PreflightFailed {
                    error: "failed".into(),
                },
                true,
                false,
            ),
            (Message::MaintenanceFinished { result: Ok(()) }, false, true),
            (Message::Adopted { transfer: 1 }, false, false),
            (Message::Stopping, false, false),
            (Message::Released, false, false),
            (
                Message::Preflight {
                    supervisor_api: 1,
                    path: "manifest".into(),
                    directory: "/state".into(),
                    sequence: 7,
                },
                false,
                false,
            ),
        ];
        for (message, for_manifest, for_maintenance) in cases {
            let frame = frame(message);
            let method = frame.value["method"].as_str().unwrap();
            assert_eq!(
                expected(true).allows(method),
                for_manifest,
                "Manifest phase {method}"
            );
            assert_eq!(
                expected(false).allows(method),
                for_maintenance,
                "Maintenance phase {method}"
            );
        }
        for method in [
            "Setup",
            "Leases",
            "Command.Start",
            "Event.Exported",
            "SupervisorStart",
            "",
            "Event.Manifest.extra",
            "maintenancefinished",
        ] {
            assert!(!expected(true).allows(method), "{method}");
            assert!(!expected(false).allows(method), "{method}");
        }
    }

    #[test]
    fn c16c_receive_result_accepts_each_actual_result_identity() {
        for case in 0..4 {
            let message = match case {
                0 => manifest(),
                1 => Message::PreflightFailed {
                    error: "failed\n雪".into(),
                },
                2 => Message::MaintenanceFinished { result: Ok(()) },
                3 => Message::MaintenanceFinished {
                    result: Err("failed".into()),
                },
                _ => unreachable!(),
            };
            let (mut sender, mut attempt) = attempt(expected(case < 2));
            let mut frame = frame(message);
            send(&mut sender, &mut frame);
            let received = attempt.receive_result().unwrap().unwrap();
            assert!(!attempt.channel.closed());
            assert!(attempt.result.is_none());
            assert!(attempt.deadline.is_none());
            assert_eq!(received.adopted, if case == 0 { Some(1) } else { None });
            match received.message {
                Message::Event(Event::Manifest {
                    sequence, lease, ..
                }) => {
                    assert_eq!(sequence, 7);
                    assert_eq!(lease.path, PathBuf::from("/phase-test-manifest"));
                }
                Message::PreflightFailed { error } => assert_eq!(error, "failed\n雪"),
                Message::MaintenanceFinished { result } => assert_eq!(
                    result,
                    if case == 2 {
                        Ok(())
                    } else {
                        Err("failed".into())
                    }
                ),
                _ => panic!("wrong logical result"),
            }
            attempt.decoder.finish().unwrap();
        }
    }

    #[test]
    fn c16c_receive_result_rejects_wrong_direction_before_pending_and_closes_received_fds() {
        let _no_forks = goblins_unix::no_forks();
        for manifest in [false, true] {
            for method in ["Command.Start", "Event.Exported", "SupervisorStart"] {
                let (mut sender, mut attempt) = attempt(expected(manifest));
                let (reader, writer) = goblins_unix::pipe().unwrap();
                let mut reader = fs::File::from(reader);
                // This header is structurally valid and would put the common
                // decoder into pending state if the actor guard let it through.
                let handoff = json!({"transfer":1,"count":1,"workspace":{"$fd":0},"working_directory":null,
                    "dev_shell":"null","docker":false,"leases":[],"more":true});
                let params = match method {
                    "Command.Start" => json!({"handoff":handoff}),
                    "Event.Exported" => json!({"member":"child","result":{"Ok":handoff}}),
                    "SupervisorStart" => {
                        json!({"supervisor_api":1,"source":{"kind":"Member","manifest":"/manifest","configuration":"child","auto":{},"state":"/state"},
                        "directory":"/state","dimensions":[24,80],"detached":false,"payload_nofile":null,"legacy_listener":null,"handoff":handoff})
                    }
                    _ => unreachable!(),
                };
                // Demonstrate the exact payload really is an incomplete common
                // codec transfer, not merely a malformed setup fixture.
                let value = json!({"jsonrpc":"2.0","method":method,"params":params});
                let mut reference = Decoder::default();
                assert!(
                    reference
                        .receive(Frame {
                            value: value.clone(),
                            fds: vec![writer.try_clone().unwrap()]
                        })
                        .unwrap()
                        .is_none()
                );
                assert_eq!(
                    reference.finish().unwrap_err().to_string(),
                    "EOF during handoff transfer"
                );
                let mut frame = Frame {
                    value,
                    fds: vec![writer],
                };
                send(&mut sender, &mut frame);
                let error = attempt
                    .receive_result()
                    .err()
                    .expect("wrong direction must fail immediately");
                assert_eq!(
                    error.to_string(),
                    if manifest {
                        "invalid preflight result method"
                    } else {
                        "invalid maintenance result method"
                    }
                );
                assert!(attempt.channel.closed());
                attempt.decoder.finish().unwrap();
                assert!(
                    attempt.result.is_none()
                        && attempt.extra.is_none()
                        && attempt.deadline.is_none()
                );
                assert!(goblins_unix::readable(reader.as_raw_fd(), 100).unwrap());
                assert_eq!(
                    reader.read(&mut [0]).unwrap(),
                    0,
                    "received raw descriptor leaked"
                );
            }
        }
    }

    #[test]
    fn c16c_allowed_method_preserves_decoder_diagnostic_and_raw_fd_cleanup() {
        let _no_forks = goblins_unix::no_forks();
        let (mut sender, mut attempt) = attempt(Expected::Maintenance);
        let (reader, writer) = goblins_unix::pipe().unwrap();
        let mut reader = fs::File::from(reader);
        let invalid = json!({"Unknown":null});
        let expected = serde_json::from_value::<std::result::Result<(), String>>(invalid.clone())
            .unwrap_err()
            .to_string();
        let mut frame = Frame {
            value: json!({"jsonrpc":"2.0","method":"MaintenanceFinished","params":{"result":invalid,"raw":{"$fd":0}}}),
            fds: vec![writer],
        };
        send(&mut sender, &mut frame);
        let error = attempt.receive_result().err().unwrap();
        assert_eq!(error.to_string(), expected);
        // This method passed the phase guard; ordinary decoder failure retains
        // the actor's existing transport teardown point at Attempt::drop.
        assert!(!attempt.channel.closed());
        attempt.decoder.finish().unwrap();
        assert!(goblins_unix::readable(reader.as_raw_fd(), 100).unwrap());
        assert_eq!(reader.read(&mut [0]).unwrap(), 0);
        drop(attempt);
        assert!(sender.receive().unwrap().is_none());
        assert!(sender.closed());
    }
}
