//! Exercise the production actor with real control framing and a stuck worker.
use super::*;
use crate::supervisor_process::Actor;
use goblins_runtime::{
    channel::Channel,
    supervisor::{Decoder, Encoder, Message, Received},
};
use std::os::unix::net::UnixStream;

struct Peer {
    channel: Channel,
    encoder: Encoder,
    decoder: Decoder,
}
impl Peer {
    fn pair() -> (Channel, Self) {
        let (actor, peer) = UnixStream::pair().unwrap();
        (
            Channel::new(actor).unwrap(),
            Self {
                channel: Channel::new(peer).unwrap(),
                encoder: Encoder::default(),
                decoder: Decoder::default(),
            },
        )
    }
    fn send(&mut self, message: Message) {
        let mut prepared = self.encoder.prepare(message).unwrap();
        assert!(
            !prepared.needs_adoption(),
            "fixture sends descriptor-free controls"
        );
        while let Some(mut frame) = prepared.frames.pop_front() {
            self.channel.queue(&frame.value, &mut frame.fds).unwrap();
        }
        self.channel.flush().unwrap();
        assert!(!self.channel.wants_write());
    }
    fn receive(&mut self) -> Vec<Received> {
        let mut received = vec![];
        while let Some(message) = self.decoder.receive_channel(&mut self.channel).unwrap() {
            received.push(message);
        }
        received
    }
}
fn diff(serial: u64) -> Command {
    Command::Diff {
        serial,
        changes: goblins_runtime::devshell::Changes {
            update: None,
            lock: false,
        },
    }
}

#[test]
fn a_full_worker_refuses_one_request_without_delaying_cancel_or_stop() {
    let (worker, work, _results) = worker();
    let token = worker.cancel.clone();
    let mut engine = ready(worker);
    let (policy, support) = engine.capabilities.as_mut().unwrap();
    policy.dev_shell = Some(goblins_runtime::policy::DevShellPolicy {
        reference: "/fixture/flake".into(),
        path: "/fixture/flake".into(),
    });
    assert!(
        (goblins_runtime::policy::Capabilities { policy, support })
            .check(&goblins_runtime::policy::Operation::DevShellDiff)
            .is_ok()
    );
    for serial in 0..4 {
        engine.send(diff(serial)).unwrap();
    }
    assert!(
        engine.send(diff(4)).is_err(),
        "the worker must be full before the actor reads commands"
    );
    assert!(
        engine.events.is_empty(),
        "authorized work must not produce a policy failure"
    );
    assert!(
        token.check().is_ok(),
        "the worker must still be live before Cancel"
    );
    let mut actor = Actor::new(engine);
    let (mut control, mut peer) = Peer::pair();
    let mut decoder = Decoder::default();
    peer.send(Message::Command(diff(5)));
    peer.send(Message::Command(Command::Cancel(0)));
    let mut messages = vec![];
    for _ in 0..32 {
        actor.tick(&mut control, &mut decoder).unwrap();
        assert!(actor.output.len() <= EVENTS);
        messages.extend(peer.receive().into_iter().map(|r| r.message));
    }
    assert!(
        token.check().is_ok(),
        "Cancel must not stop the whole worker"
    );
    let accepted: Vec<_> = work.try_iter().collect();
    assert_eq!(accepted.len(), 4);
    assert!(accepted[0].1.check().is_err());
    assert!(
        accepted[1..]
            .iter()
            .all(|(_, cancel)| cancel.check().is_ok())
    );
    peer.send(Message::Command(Command::Stop));
    for _ in 0..32 {
        actor.tick(&mut control, &mut decoder).unwrap();
        assert!(actor.output.len() <= EVENTS);
        messages.extend(peer.receive().into_iter().map(|r| r.message));
    }
    assert!(token.check().is_err());
    assert!(accepted.iter().all(|(_, cancel)| cancel.check().is_err()));
    assert_eq!(
        messages
            .iter()
            .filter(|m| matches!(m, Message::Stopping))
            .count(),
        1
    );
    assert_eq!(messages.iter().filter(|m| matches!(m, Message::Event(Event::Call { serial: 5, result: Err(e) }) if e == "resource limit reached")).count(), 1);
    assert!(
        !messages
            .iter()
            .any(|m| matches!(m, Message::Event(Event::Failed(_))))
    );
}

#[test]
fn stopped_waits_for_typed_export_adoption_without_blocking_control_reads() {
    let _no_forks = unix::no_forks();
    let directory = unix::temp_directory().unwrap();
    let session = directory.join("session");
    unix::private_directory(&session).unwrap();
    let resources = session.join("resources");
    unix::private_directory(&resources).unwrap();
    let (worker, _work, results) = worker();
    let engine = ready(worker);
    *engine.published.lock().unwrap() =
        Some(Inheritance::fixture_in("shell", &[], resources.clone()));
    let mut actor = Actor::new(engine);
    let (mut control, mut peer) = Peer::pair();
    let mut decoder = Decoder::default();
    peer.send(Message::Command(Command::Export {
        member: "member".into(),
    }));
    peer.send(Message::Command(Command::Stop));
    results
        .try_send(Event::Stopped { exit_code: Some(0) })
        .unwrap_or_else(|_| panic!());
    let mut adoption = None;
    let mut stopped = 0;
    for _ in 0..64 {
        actor.tick(&mut control, &mut decoder).unwrap();
        assert!(actor.output.len() <= EVENTS);
        for received in peer.receive() {
            match received.message {
                Message::Event(Event::Exported {
                    member,
                    result: Ok(handoff),
                }) => {
                    assert_eq!(member, "member");
                    assert!(adoption.replace(received.adopted.unwrap()).is_none());
                    drop(handoff); // Sender ownership must outlive this lost ACK.
                }
                Message::Event(Event::Stopped { .. }) | Message::Released => stopped += 1,
                _ => (),
            }
        }
    }
    assert_eq!(stopped, 0);
    assert!(resources.exists());
    peer.send(Message::Adopted {
        transfer: adoption.unwrap(),
    });
    let mut released = 0;
    for _ in 0..64 {
        actor.tick(&mut control, &mut decoder).unwrap();
        for received in peer.receive() {
            match received.message {
                Message::Event(Event::Stopped { exit_code: Some(0) }) => stopped += 1,
                Message::Released => released += 1,
                _ => (),
            }
        }
    }
    assert_eq!(stopped, 1);
    assert_eq!(released, 1);
    assert!(!resources.exists());
    assert!(
        !session.exists(),
        "last lease also removes its empty session parent"
    );
    assert!(
        directory.exists(),
        "outer fixture is independent of session cleanup"
    );
    drop(actor);
    std::fs::remove_dir_all(directory).unwrap();
}

#[test]
fn full_control_output_backpressures_the_engine_without_losing_results() {
    let (worker, _work, results) = worker();
    let token = worker.cancel.clone();
    let mut actor = Actor::new(ready(worker));
    let (mut control, mut peer) = Peer::pair();
    let mut decoder = Decoder::default();
    let size: libc::c_int = 4096;
    assert_eq!(
        unsafe {
            libc::setsockopt(
                control.fd().unwrap(),
                libc::SOL_SOCKET,
                libc::SO_SNDBUF,
                (&size as *const libc::c_int).cast(),
                std::mem::size_of_val(&size) as _,
            )
        },
        0
    );
    for serial in 0..EVENTS {
        actor.output.push_back(Message::Event(Event::Call {
            serial: serial as _,
            result: Ok(serde_json::json!("x".repeat(512 * 1024))),
        }));
        results
            .try_send(Event::Call {
                serial: (EVENTS + serial) as _,
                result: Ok(serde_json::json!(serial)),
            })
            .unwrap_or_else(|_| panic!());
    }
    for _ in 0..32 {
        actor.tick(&mut control, &mut decoder).unwrap();
        assert!(actor.output.len() <= EVENTS);
    }
    assert!(
        control.wants_write(),
        "must exercise real partial-frame backpressure"
    );
    assert_eq!(
        actor.output.len(),
        EVENTS,
        "the actor queue must be saturated too"
    );
    peer.send(Message::Command(Command::Stop));
    let mut seen = std::collections::BTreeSet::new();
    let mut stopping = 0;
    for _ in 0..4096 {
        actor.tick(&mut control, &mut decoder).unwrap();
        assert!(actor.output.len() <= EVENTS);
        for received in peer.receive() {
            match received.message {
                Message::Event(Event::Call {
                    serial,
                    result: Ok(value),
                }) => {
                    assert!(seen.insert(serial));
                    if serial < EVENTS as u64 {
                        assert_eq!(value.as_str().unwrap().len(), 512 * 1024);
                        assert!(value.as_str().unwrap().bytes().all(|byte| byte == b'x'));
                    }
                }
                Message::Stopping => stopping += 1,
                _ => (),
            }
        }
        if seen.len() == 2 * EVENTS && stopping == 1 {
            break;
        }
    }
    assert_eq!(seen, (0..2 * EVENTS as u64).collect());
    assert_eq!(stopping, 1);
    assert!(token.check().is_err());
}
