use super::*;
use goblins_unix as unix;
use std::{
    fs,
    io::{Read, Write},
    path::Path,
};

fn host() -> Source {
    Source::Host {
        configuration: "/config.json".into(),
        name: "shell".into(),
        scope: Some("shared".into()),
        dev_shell: Some("/flake#dev".into()),
        workspace: Some("/workspace".into()),
        cwd: Some("/workspace/sub".into()),
        state: "/state".into(),
        history: Some("/history".into()),
        recreate: Some(Recreate {
            packages: vec!["hello".into()],
            auto_approve: vec!["jq".into()],
            docker: true,
            variables: [("session".into(), "abc".into())].into(),
        }),
        sequence: 23,
    }
}
fn startup(source: Source, handoff: Option<Handoff>) -> Message {
    Message::SupervisorStart(SupervisorStart {
        supervisor_api: API,
        source,
        directory: "/state/session/resources".into(),
        dimensions: (37, 123),
        detached: true,
        payload_nofile: None,
        legacy_listener: None,
        handoff: handoff.map(Box::new),
    })
}

#[test]
fn diagnostic_records_preserve_opaque_descriptor_looking_data_without_capabilities() {
    let fields = json!({"$fd":0,"nested":[{"$fd":1}],"text":"line\nquote\""});
    let mut encoded = Encoder::default()
        .prepare(Message::Command(Command::RecordEvent {
            event: "exec-started".into(),
            fields: fields.clone(),
        }))
        .unwrap();
    assert!(!encoded.needs_adoption());
    assert_eq!(encoded.frames.len(), 1);
    assert!(encoded.frames[0].fds.is_empty());
    assert!(encoded.frames[0].value["params"]["fields"].is_string());
    let received = decode(&mut encoded, &mut Decoder::default());
    assert!(received.adopted.is_none());
    assert!(
        matches!(received.message, Message::Command(Command::RecordEvent { event, fields: actual })
        if event=="exec-started" && actual==fields)
    );
}
fn decode(prepared: &mut Prepared, decoder: &mut Decoder) -> Received {
    let mut result = None;
    while let Some(frame) = prepared.frames.pop_front() {
        if let Some(message) = decoder.receive(frame).unwrap() {
            assert!(result.is_none());
            result = Some(message);
        }
    }
    result.expect("one complete message")
}
fn roundtrip(message: Message) {
    let mut prepared = Encoder::default().prepare(message).unwrap();
    let expected: Vec<_> = prepared.frames.iter().map(|f| f.value.clone()).collect();
    let received = decode(&mut prepared, &mut Decoder::default());
    assert!(received.adopted.is_none());
    let repeated = Encoder::default().prepare(received.message).unwrap();
    let actual: Vec<_> = repeated.frames.iter().map(|f| f.value.clone()).collect();
    assert_eq!(actual, expected);
}
fn null() -> Fd {
    OwnedFd::from(fs::File::open("/dev/null").unwrap()).into()
}
fn pipe() -> (Fd, fs::File) {
    let (a, b) = unix::pipe().unwrap();
    (a.into(), b.into())
}
pub(super) fn handoff(root: &Path, count: usize) -> Handoff {
    let leases = (0..count)
        .map(|n| {
            let path = root.join(format!("r{n}"));
            unix::private_directory(&path).unwrap();
            fs::write(path.join("held"), n.to_string()).unwrap();
            ResourceLease::new(path).unwrap()
        })
        .collect();
    Handoff {
        leases,
        workspace: OwnedFd::from(fs::File::open(root).unwrap()).into(),
        working_directory: Some(crate::WorkingDirectory {
            fd: OwnedFd::from(fs::File::open(root).unwrap()).into(),
            source: root.into(),
            destination: "/workspace".into(),
        }),
        dev_shell: None,
        docker: true,
    }
}
#[test]
fn all_commands_roundtrip_their_parameters_and_stdio_shapes() {
    let _no_forks = unix::no_forks();
    let commands = vec![
        Command::Attach {
            terminal: TerminalId::Process(11),
            stream: null(),
            rows: 25,
            cols: 95,
        },
        Command::Detach {
            terminal: TerminalId::Main,
        },
        Command::Resize {
            terminal: TerminalId::Main,
            rows: 35,
            cols: 90,
        },
        Command::SandboxSend {
            connection: 12,
            message: json!({"$fd":0,"result":{"$fd":"literal"}}),
        },
        Command::SandboxClose { connection: 12 },
        Command::ScopeFailed,
        Command::ScopeMembership { result: Ok(null()) },
        Command::ScopeMembership {
            result: Err("denied".into()),
        },
        Command::Preview {
            approval: 1,
            package: "hello".into(),
        },
        Command::Decide {
            request: goblins_protocol::Request {
                id: "r".into(),
                kind: "package".into(),
                package: "hello".into(),
                reason: "needed".into(),
            },
            approved: true,
            output: Some("/nix/store/output".into()),
        },
        Command::Refresh {
            approval: 3,
            changes: crate::devshell::Changes {
                update: Some(vec!["nixpkgs".into()]),
                lock: true,
            },
        },
        Command::Run {
            serial: 4,
            app: "demo".into(),
        },
        Command::Diff {
            serial: 5,
            changes: Default::default(),
        },
        Command::Cancel(3),
        Command::Spawn {
            id: 11,
            kind: crate::SpawnKind::Exec,
            argv: Some(vec!["printf".into(), "x".into()]),
            cwd: Some("/workspace".into()),
            env: vec![("A".into(), "B".into())],
            stdio: Stdio::Fds([null(), null(), null()]),
        },
        Command::Spawn {
            id: 12,
            kind: crate::SpawnKind::Attach,
            argv: None,
            cwd: None,
            env: vec![],
            stdio: Stdio::Tty(null()),
        },
        Command::Spawn {
            id: 13,
            kind: crate::SpawnKind::Attach,
            argv: None,
            cwd: None,
            env: vec![],
            stdio: Stdio::NewPty { rows: 20, cols: 80 },
        },
        Command::Signal {
            id: 11,
            signal: libc::SIGINT,
        },
        Command::CancelProcess {
            id: 11,
            everything: true,
        },
        Command::KillAll,
        Command::Export {
            member: "child".into(),
        },
        Command::PrepareHostJob {
            request: "host".into(),
            cwd: "/workspace".into(),
        },
        Command::RunHostJob {
            request: "host".into(),
            argv: vec!["true".into()],
            env: vec![("K".into(), "V".into())],
            timeout: Some(30),
        },
        Command::CancelHostJob {
            request: "host".into(),
        },
        Command::Stop,
    ];
    for command in commands {
        roundtrip(Message::Command(command));
    }
}
#[test]
fn all_descriptor_free_events_preserve_private_and_public_data() {
    let summary = crate::policy::DevShellSummary {
        identity: "generation".into(),
        lock: json!({"$fd":0}),
        generation: 2,
    };
    let mut policy = crate::policy::Policy::fixture("shell", &[]);
    policy.description = "runtime".into();
    let events = vec![
        Event::ScopeFailed {
            name: "scope".into(),
        },
        Event::ScopeNeeded {
            name: "scope".into(),
        },
        Event::Preview {
            approval: 4,
            result: Ok(crate::catalog::Preview {
                output_path: Some("/private/output".into()),
                in_store: true,
                download: Some("7 MiB".into()),
                build_required: false,
            }),
        },
        Event::Preview {
            approval: 4,
            result: Err("not found".into()),
        },
        Event::Started {
            docker_enabled: true,
            policy: Box::new(policy),
            support: crate::policy::Support::linux(),
            dev_shell: Some(summary.clone()),
            identity: crate::Identity {
                pid: 42,
                helper_userns: 1,
                mount_owner: 2,
                source_mountns: 3,
                mountns: 4,
            },
            recreated: Some(Box::new(crate::Recreated {
                packages: vec![("hello".into(), "/output".into())],
                auto_approve: vec![("jq".into(), "/jq".into())],
                skipped: vec![crate::history::Skipped::new("old", "missing")],
                fresh: Some("fresh".into()),
            })),
            auto: Box::new(crate::Auto {
                rooted: vec![("hello".into(), "/output".into())],
                skipped: vec![],
            }),
        },
        Event::Progress {
            request: "r".into(),
            state: "mounting".into(),
        },
        Event::Progress {
            request: "r".into(),
            state: "future-state".into(),
        },
        Event::Mode { server: true },
        Event::Refresh {
            approval: 7,
            result: Ok((json!({"$fd":99}), "preview".into())),
        },
        Event::Refresh {
            approval: 8,
            result: Err("cancelled".into()),
        },
        Event::Call {
            serial: 8,
            result: Ok(json!({"result":{"$fd":0}})),
        },
        Event::Call {
            serial: 9,
            result: Err("failed".into()),
        },
        Event::DevShell { summary },
        Event::Exported {
            member: "child".into(),
            result: Err("owner stopped".into()),
        },
        Event::Granted {
            reply: goblins_protocol::Reply::new(Some("r".into()), "granted", Some("ready".into())),
            detail: Some("note".into()),
            output: Some("/output".into()),
        },
        Event::Spawned { id: 1, pid: 90 },
        Event::Terminal {
            terminal: TerminalId::Process(1),
            status: TerminalStatus {
                eof: true,
                complete: false,
                interrupted: true,
                attached: false,
                detached: true,
                connected: true,
            },
            error: Some("diagnostic".into()),
        },
        Event::SpawnFailed {
            id: 2,
            code: Some(127),
            message: "missing".into(),
        },
        Event::Exited {
            id: 1,
            code: None,
            signal: Some(1),
        },
        Event::InitLost,
        Event::PayloadExited,
        Event::SandboxOpened { connection: 1 },
        Event::SandboxCall {
            connection: 1,
            call: goblins_jsonrpc::Call {
                jsonrpc: "2.0".into(),
                id: Some(json!({"$fd":0})),
                method: "inbox.status".into(),
                params: json!({"nested":{"$fd":"literal"}}),
            },
        },
        Event::SandboxClosed { connection: 1 },
        Event::HostJobPrepared {
            request: "h".into(),
            result: Ok("/host".into()),
        },
        Event::HostJobPrepared {
            request: "h".into(),
            result: Err("not shared".into()),
        },
        Event::HostJobFailed {
            request: "h".into(),
            error: "failed".into(),
        },
        Event::HostJobFinished {
            request: "h".into(),
            outcome: Ok(HostOutcome {
                exit_code: Some(4),
                signal: Some(2),
                spawn_error: Some("message".into()),
                cancelled: true,
                timed_out: true,
                truncated: true,
            }),
        },
        Event::HostJobFinished {
            request: "h".into(),
            outcome: Err("lost result".into()),
        },
        Event::Failed("launch failed".into()),
        Event::Stopped { exit_code: Some(4) },
    ];
    for event in events {
        roundtrip(Message::Event(event));
    }
    for message in [
        Message::Stopping,
        Message::Released,
        Message::Adopted { transfer: 8 },
    ] {
        roundtrip(message);
    }
    roundtrip(startup(host(), None));
}
#[test]
fn stdio_capabilities_reach_their_exact_streams_and_unused_or_duplicate_fds_fail() {
    let _no_forks = unix::no_forks();
    let (a, mut aw) = pipe();
    let (b, mut bw) = pipe();
    let (c, mut cw) = pipe();
    aw.write_all(b"A").unwrap();
    bw.write_all(b"B").unwrap();
    cw.write_all(b"C").unwrap();
    let message = Message::Command(Command::Spawn {
        id: 1,
        kind: crate::SpawnKind::Exec,
        argv: None,
        cwd: None,
        env: vec![],
        stdio: Stdio::Fds([a, b, c]),
    });
    let mut encoded = Encoder::default().prepare(message).unwrap();
    let Received {
        message:
            Message::Command(Command::Spawn {
                stdio: Stdio::Fds(fds),
                ..
            }),
        ..
    } = decode(&mut encoded, &mut Decoder::default())
    else {
        panic!("spawn");
    };
    for (fd, expected) in fds.into_iter().zip([b'A', b'B', b'C']) {
        let mut byte = [0];
        fs::File::from(OwnedFd::from(fd))
            .read_exact(&mut byte)
            .unwrap();
        assert_eq!(byte, [expected]);
    }
    let mut encoded = Encoder::default()
        .prepare(Message::Command(Command::Attach {
            terminal: TerminalId::Main,
            stream: null(),
            rows: 24,
            cols: 80,
        }))
        .unwrap();
    let mut frame = encoded.frames.pop_front().unwrap();
    frame.fds.push(fs::File::open("/dev/null").unwrap().into());
    assert!(Decoder::default().receive(frame).is_err());
    let mut encoded = Encoder::default()
        .prepare(Message::Command(Command::Spawn {
            id: 1,
            kind: crate::SpawnKind::Exec,
            argv: None,
            cwd: None,
            env: vec![],
            stdio: Stdio::Fds([null(), null(), null()]),
        }))
        .unwrap();
    let mut frame = encoded.frames.pop_front().unwrap();
    frame.value["params"]["stdio"]["fds"][1] = json!({"$fd":0});
    assert!(Decoder::default().receive(frame).is_err());
}
#[test]
fn handoffs_batch_over_240_descriptors_and_preserve_scope_and_directory_authority() {
    let _no_forks = unix::no_forks();
    let root = unix::temp_directory().unwrap();
    let (scope, mut scope_peer) = std::os::unix::net::UnixStream::pair().unwrap();
    scope_peer.write_all(b"S").unwrap();
    let source = Source::Member {
        scope: Some(OwnedFd::from(scope).into()),
        owner_scope: Some("scope".into()),
        manifest: "/manifest".into(),
        configuration: "child".into(),
        auto: [("hello".into(), "/output".into())].into(),
        state: "/state".into(),
        history: Some("/history".into()),
    };
    let Message::SupervisorStart(mut start) =
        startup(source, Some(handoff(&root, channel::MAX_FDS + 1)))
    else {
        unreachable!()
    };
    start.payload_nofile = Some(128);
    start.directory = root.join("resources");
    let path = root.join("terminal.sock");
    start.legacy_listener =
        Some(OwnedFd::from(std::os::unix::net::UnixListener::bind(&path).unwrap()).into());
    let message = Message::SupervisorStart(start);
    let mut prepared = Encoder::default().prepare(message).unwrap();
    assert!(prepared.frames.len() > 1);
    assert!(
        prepared
            .frames
            .iter()
            .all(|f| f.fds.len() <= channel::MAX_FDS)
    );
    let Received {
        message: Message::SupervisorStart(start),
        ..
    } = decode(&mut prepared, &mut Decoder::default())
    else {
        panic!("start");
    };
    assert_eq!(start.payload_nofile, Some(128));
    let listener =
        std::os::unix::net::UnixListener::from(OwnedFd::from(start.legacy_listener.unwrap()));
    let peer = std::os::unix::net::UnixStream::connect(&path).unwrap();
    let (accepted, _) = listener.accept().unwrap();
    drop((peer, accepted));
    let Source::Member {
        scope: Some(scope),
        owner_scope,
        ..
    } = start.source
    else {
        panic!("member");
    };
    assert_eq!(owner_scope.as_deref(), Some("scope"));
    let mut byte = [0];
    fs::File::from(OwnedFd::from(scope))
        .read_exact(&mut byte)
        .unwrap();
    assert_eq!(&byte, b"S");
    let h = start.handoff.unwrap();
    assert_eq!(h.leases.len(), channel::MAX_FDS + 1);
    assert!(h.docker);
    assert_eq!(
        h.working_directory.as_ref().unwrap().destination,
        Path::new("/workspace")
    );
    drop(prepared);
    for lease in &h.leases {
        assert!(lease.directory().join("held").exists());
        assert!(
            crate::lease::resource_exclusive(lease.directory())
                .unwrap()
                .is_none()
        );
    }
    drop(h);
    drop(listener);
    assert!(std::os::unix::net::UnixStream::connect(&path).is_err());
    assert!(!root.join("r0").exists());
    assert!(!root.join(format!("r{}", channel::MAX_FDS)).exists());
    fs::remove_dir_all(root).unwrap();
}
#[test]
fn startup_listener_is_closed_with_partial_handoff_on_eof_or_invalid_continuation() {
    let _no_forks = unix::no_forks();
    for invalid_continuation in [false, true] {
        let root = unix::temp_directory().unwrap();
        let source = Source::Member {
            scope: None,
            owner_scope: None,
            manifest: "/manifest".into(),
            configuration: "child".into(),
            auto: Default::default(),
            state: root.clone(),
            history: None,
        };
        let Message::SupervisorStart(mut start) =
            startup(source, Some(handoff(&root, channel::MAX_FDS + 1)))
        else {
            unreachable!()
        };
        let path = root.join("terminal.sock");
        start.directory = root.join("resources");
        start.legacy_listener =
            Some(OwnedFd::from(std::os::unix::net::UnixListener::bind(&path).unwrap()).into());
        let mut prepared = Encoder::default()
            .prepare(Message::SupervisorStart(start))
            .unwrap();
        let mut decoder = Decoder::default();
        assert!(
            decoder
                .receive(prepared.frames.pop_front().unwrap())
                .unwrap()
                .is_none()
        );
        let continuation = prepared.frames.pop_front().unwrap();
        drop(std::os::unix::net::UnixStream::connect(&path).unwrap());
        if invalid_continuation {
            let mut frame = continuation;
            frame.value["params"]["transfer"] = json!(999);
            assert!(decoder.receive(frame).is_err());
        } else {
            drop(continuation);
            assert!(decoder.finish().is_err());
        }
        // Failed adoption drops the receiver; the typed sender still retains
        // its listener and every lease until teardown, just like Handoff.
        drop(std::os::unix::net::UnixStream::connect(&path).unwrap());
        drop(prepared);
        assert!(std::os::unix::net::UnixStream::connect(&path).is_err());
        assert!(!root.join("r0").exists());
        assert!(!root.join(format!("r{}", channel::MAX_FDS)).exists());
        fs::remove_dir_all(root).unwrap();
    }
}
#[test]
fn start_and_export_handoffs_roundtrip_and_partial_or_invalid_batches_release_every_lease() {
    let _no_forks = unix::no_forks();
    for exported in [false, true] {
        let root = unix::temp_directory().unwrap();
        let h = Box::new(handoff(&root, channel::MAX_FDS + 1));
        let message = if exported {
            Message::Event(Event::Exported {
                member: "child".into(),
                result: Ok(h),
            })
        } else {
            Message::Command(Command::Start(h))
        };
        let mut prepared = Encoder::default().prepare(message).unwrap();
        let mut decoder = Decoder::default();
        assert!(
            decoder
                .receive(prepared.frames.pop_front().unwrap())
                .unwrap()
                .is_none()
        );
        let mut cancel = Encoder::default()
            .prepare(Message::Command(Command::Cancel(9)))
            .unwrap();
        assert!(matches!(
            decode(&mut cancel, &mut decoder).message,
            Message::Command(Command::Cancel(9))
        ));
        let received = decode(&mut prepared, &mut decoder);
        let h = match received.message {
            Message::Command(Command::Start(h)) => h,
            Message::Event(Event::Exported {
                member,
                result: Ok(h),
            }) => {
                assert_eq!(member, "child");
                h
            }
            _ => panic!("handoff"),
        };
        drop(prepared);
        assert!(root.join("r0/held").exists());
        drop(h);
        fs::remove_dir_all(root).unwrap();
    }
    for bad in [false, true] {
        let root = unix::temp_directory().unwrap();
        let mut prepared = Encoder::default()
            .prepare(Message::Command(Command::Start(Box::new(handoff(
                &root,
                channel::MAX_FDS + 1,
            )))))
            .unwrap();
        let mut decoder = Decoder::default();
        assert!(
            decoder
                .receive(prepared.frames.pop_front().unwrap())
                .unwrap()
                .is_none()
        );
        if bad {
            let mut frame = prepared.frames.pop_front().unwrap();
            frame.value["params"]["transfer"] = json!(999);
            assert!(decoder.receive(frame).is_err());
        } else {
            assert!(decoder.finish().is_err());
        }
        drop(prepared);
        drop(decoder);
        assert!(!root.join("r0").exists());
        assert!(!root.join(format!("r{}", channel::MAX_FDS)).exists());
        fs::remove_dir_all(root).unwrap();
    }
}
fn manifest(root: &Path) -> Message {
    let path = root.join("manifest.json");
    fs::write(&path, "{}").unwrap();
    let gc = root.join("gc-root");
    fs::write(&gc, "rooted").unwrap();
    Message::Event(Event::Manifest {
        sequence: 17,
        lease: ManifestLease::adopt(path, Some(gc)),
    })
}
#[test]
fn manifest_adoption_survives_lost_ack_and_rejected_generation_cleans_the_correct_root() {
    let _no_forks = unix::no_forks();
    for receiver_first in [false, true] {
        let root = unix::temp_directory().unwrap();
        let mut prepared = Encoder::default().prepare(manifest(&root)).unwrap();
        let received = decode(&mut prepared, &mut Decoder::default());
        let id = received.adopted.unwrap();
        assert_eq!(id, prepared.transfer());
        assert!(prepared.needs_adoption());
        assert!(root.join("gc-root").exists());
        if receiver_first {
            drop(received);
            assert!(root.join("gc-root").exists());
            prepared.adopted(id).unwrap();
            assert!(!root.join("gc-root").exists());
            drop(prepared);
        } else {
            // Sender EOF before Adopted must not unroot the live daemon.
            drop(prepared);
            assert!(root.join("gc-root").exists());
            drop(received);
            assert!(!root.join("gc-root").exists());
        }
        fs::remove_dir_all(root).unwrap();
    }
    let root = unix::temp_directory().unwrap();
    let mut prepared = Encoder::default().prepare(manifest(&root)).unwrap();
    let received = decode(&mut prepared, &mut Decoder::default());
    let id = received.adopted.unwrap();
    assert!(prepared.adopted(id + 1).is_err());
    prepared.adopted(id).unwrap();
    assert!(!prepared.needs_adoption());
    assert!(prepared.adopted(id).is_err());
    drop(prepared);
    assert!(root.join("gc-root").exists());
    drop(received);
    assert!(!root.join("gc-root").exists());
    fs::remove_dir_all(root).unwrap();
}
#[test]
fn manifest_decode_failure_or_never_sent_event_does_not_leak_the_gc_root() {
    let _no_forks = unix::no_forks();
    let root = unix::temp_directory().unwrap();
    let prepared = Encoder::default().prepare(manifest(&root)).unwrap();
    drop(prepared);
    assert!(!root.join("gc-root").exists());
    let mut prepared = Encoder::default().prepare(manifest(&root)).unwrap();
    let mut frame = prepared.frames.pop_front().unwrap();
    frame.fds.push(fs::File::open("/dev/null").unwrap().into());
    assert!(Decoder::default().receive(frame).is_err());
    assert!(root.join("gc-root").exists());
    drop(prepared);
    assert!(!root.join("gc-root").exists());
    fs::remove_dir_all(root).unwrap();
}
#[test]
fn startup_version_and_handoff_shape_fail_before_any_launch_side_effect() {
    let mut prepared = Encoder::default().prepare(startup(host(), None)).unwrap();
    let mut frame = prepared.frames.pop_front().unwrap();
    frame.value["params"]["supervisor_api"] = json!(API + 1);
    let error = match Decoder::default().receive(frame) {
        Err(e) => e,
        Ok(_) => panic!("version accepted"),
    };
    assert!(error.to_string().contains("supervisor API"));
    let Message::SupervisorStart(mut start) = startup(host(), None) else {
        unreachable!()
    };
    start.supervisor_api = API + 1;
    assert!(
        Encoder::default()
            .prepare(Message::SupervisorStart(start))
            .is_err()
    );
    let source = Source::Member {
        scope: None,
        owner_scope: None,
        manifest: "/m".into(),
        configuration: "child".into(),
        auto: Default::default(),
        state: "/s".into(),
        history: None,
    };
    assert!(Encoder::default().prepare(startup(source, None)).is_err());
}
#[test]
fn raw_json_fd_literals_pass_the_actual_transport_without_creating_capabilities() {
    let _no_forks = unix::no_forks();
    let (a, b) = std::os::unix::net::UnixStream::pair().unwrap();
    let mut sender = channel::Channel::new(a).unwrap();
    let mut receiver = channel::Channel::new(b).unwrap();
    let value = json!({"$fd":0,"nested":[{"$fd":"data"}],"result":{"$fd":{"$fd":1}}});
    let mut prepared = Encoder::default()
        .prepare(Message::Command(Command::SandboxSend {
            connection: 8,
            message: value.clone(),
        }))
        .unwrap();
    let mut frame = prepared.frames.pop_front().unwrap();
    sender.queue(&frame.value, &mut frame.fds).unwrap();
    sender.flush().unwrap();
    let frame = receiver.receive().unwrap().expect("framed message");
    assert!(frame.fds.is_empty());
    let Some(Received {
        message:
            Message::Command(Command::SandboxSend {
                connection,
                message,
            }),
        ..
    }) = Decoder::default().receive(frame).unwrap()
    else {
        panic!("reply");
    };
    assert_eq!(connection, 8);
    assert_eq!(message, value);
}

#[test]
fn a_handoff_keeps_the_published_dev_shell_generation_and_pinned_directories() {
    use std::os::unix::fs::MetadataExt;
    let _no_forks = unix::no_forks();
    let root = unix::temp_directory().unwrap();
    let mut h = handoff(&root, 1);
    let generation = crate::devshell::DevShell {
        reference: "/flake#dev".into(),
        path: "/flake".into(),
        attr: "dev".into(),
        generation: 9,
        source: "/source".into(),
        flake: "/source/sub".into(),
        lock: json!({"nodes":{"$fd":0}}),
        profile: "/profile".into(),
        variables: vec![("NAME".into(), "value".into())],
        script: "export NAME=value".into(),
        summary: crate::devshell::Summary {
            packages: [("hello".into(), ["/hello".into()].into())].into(),
            env: [("NAME".into(), "value".into())].into(),
            derivation: "/drv".into(),
        },
    };
    let expected = serde_json::to_value(&generation).unwrap();
    h.dev_shell = Some(generation);
    let mut prepared = Encoder::default()
        .prepare(Message::Command(Command::Start(Box::new(h))))
        .unwrap();
    let Received {
        message: Message::Command(Command::Start(h)),
        ..
    } = decode(&mut prepared, &mut Decoder::default())
    else {
        panic!("handoff");
    };
    assert_eq!(serde_json::to_value(&h.dev_shell).unwrap(), expected);
    assert_eq!(h.working_directory.as_ref().unwrap().source, root);
    let Handoff {
        workspace,
        working_directory,
        leases,
        ..
    } = *h;
    let expected = fs::metadata(&root).unwrap();
    for fd in [workspace, working_directory.unwrap().fd] {
        let actual = fs::File::from(OwnedFd::from(fd)).metadata().unwrap();
        assert_eq!(
            (actual.dev(), actual.ino()),
            (expected.dev(), expected.ino())
        );
    }
    drop(prepared);
    drop(leases);
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn malformed_handoff_counts_and_capabilities_do_not_acquire_unrelated_resources() {
    let _no_forks = unix::no_forks();
    for fault in 0..4 {
        let root = unix::temp_directory().unwrap();
        let unrelated = root.join("unrelated");
        unix::private_directory(&unrelated).unwrap();
        fs::write(unrelated.join("keep"), "unrelated").unwrap();
        let mut prepared = Encoder::default()
            .prepare(Message::Command(Command::Start(Box::new(handoff(
                &root, 1,
            )))))
            .unwrap();
        let mut frame = prepared.frames.pop_front().unwrap();
        let header = &mut frame.value["params"]["handoff"];
        match fault {
            0 => header["count"] = json!(0),
            1 => header["more"] = json!(true),
            2 => header["transfer"] = json!(0),
            3 => header["leases"][0]["directory"] = json!(unrelated),
            _ => unreachable!(),
        }
        assert!(Decoder::default().receive(frame).is_err());
        drop(prepared);
        assert!(!root.join("r0").exists());
        assert_eq!(
            fs::read_to_string(unrelated.join("keep")).unwrap(),
            "unrelated"
        );
        fs::remove_dir_all(root).unwrap();
    }
}

#[test]
fn strict_envelopes_and_logical_byte_budget_fail_closed() {
    for fault in 0..4 {
        let mut prepared = Encoder::default()
            .prepare(Message::Command(Command::Stop))
            .unwrap();
        let mut frame = prepared.frames.pop_front().unwrap();
        match fault {
            0 => frame.value["jsonrpc"] = json!("1.0"),
            1 => frame.value["method"] = json!("Command.Future"),
            2 => frame.value["params"]["unknown"] = json!(true),
            3 => frame.value["id"] = json!(7),
            _ => unreachable!(),
        }
        assert!(Decoder::default().receive(frame).is_err());
    }
    let oversized = "x".repeat(channel::MAX_BODY);
    assert!(
        Encoder::default()
            .prepare(Message::Command(Command::SandboxSend {
                connection: 1,
                message: json!(oversized)
            }))
            .is_err()
    );
    let mut frame = Encoder::default()
        .prepare(Message::Command(Command::Stop))
        .unwrap()
        .frames
        .pop_front()
        .unwrap();
    frame.value["params"]["oversized"] = json!(oversized);
    assert!(Decoder::default().receive(frame).is_err());
}

#[test]
fn manifest_ownership_rejects_a_wrong_lock_and_unrooted_manifests_need_no_descriptor() {
    let _no_forks = unix::no_forks();
    let root = unix::temp_directory().unwrap();
    let mut prepared = Encoder::default().prepare(manifest(&root)).unwrap();
    assert!(prepared.adopted(prepared.transfer()).is_err());
    let mut frame = prepared.frames.pop_front().unwrap();
    frame.fds[0] = fs::File::open("/dev/null").unwrap().into();
    assert!(Decoder::default().receive(frame).is_err());
    drop(prepared);
    assert!(!root.join("gc-root").exists());
    let mut prepared = Encoder::default()
        .prepare(Message::Event(Event::Manifest {
            sequence: 10,
            lease: ManifestLease::adopt(root.join("manifest.json"), None),
        }))
        .unwrap();
    assert!(prepared.frames.front().unwrap().fds.is_empty());
    let received = decode(&mut prepared, &mut Decoder::default());
    prepared.adopted(received.adopted.unwrap()).unwrap();
    drop(received);
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn batched_handoff_byte_budget_is_cumulative_and_failure_releases_pending_leases() {
    let _no_forks = unix::no_forks();
    let root = unix::temp_directory().unwrap();
    let mut prepared = Encoder::default()
        .prepare(Message::Command(Command::Start(Box::new(handoff(
            &root,
            channel::MAX_FDS + 1,
        )))))
        .unwrap();
    let mut header = prepared.frames.pop_front().unwrap();
    let bytes = serde_json::to_vec(&header.value).unwrap().len();
    // Valid JSON whitespace keeps this a null generation and this frame
    // below MAX_BODY; the following lease frame exceeds the logical budget.
    header.value["params"]["handoff"]["dev_shell"] =
        json!(format!("{}null", " ".repeat(channel::MAX_BODY - bytes - 1)));
    assert!(serde_json::to_vec(&header.value).unwrap().len() < channel::MAX_BODY);
    let mut decoder = Decoder::default();
    assert!(decoder.receive(header).unwrap().is_none());
    let error = match decoder.receive(prepared.frames.pop_front().unwrap()) {
        Err(error) => error,
        Ok(_) => panic!("cumulative budget accepted"),
    };
    assert!(error.to_string().contains("budget"));
    drop(prepared);
    drop(decoder);
    assert!(!root.join("r0").exists());
    assert!(!root.join(format!("r{}", channel::MAX_FDS)).exists());
    fs::remove_dir_all(root).unwrap();
}

fn transmitted(mut prepared: Prepared, sender: &mut channel::Channel) -> PendingTransfer {
    while let Some(mut frame) = prepared.frames.pop_front() {
        sender.queue(&frame.value, &mut frame.fds).unwrap();
        sender.flush().unwrap();
        assert!(
            !sender.wants_write(),
            "small fixture frame completely transmitted"
        );
    }
    // Consumes Prepared; the independent pending value keeps every original
    // typed lease until an actor observes adoption or channel teardown.
    prepared.into_pending().unwrap().unwrap()
}
fn closed_sender(sender: &mut channel::Channel) {
    // Exercise the production supervisor outcome, including Linux reset on
    // peer close with unread rights, before releasing any retained leases.
    assert!(
        Decoder::default()
            .receive_channel(sender)
            .unwrap()
            .is_none()
    );
    assert!(sender.closed());
}
fn owned_message(root: &Path, rooted: bool) -> Message {
    if rooted {
        manifest(root)
    } else {
        Message::Command(Command::Start(Box::new(handoff(root, 1))))
    }
}
fn assert_owned_gone(root: &Path, rooted: bool) {
    assert!(!root.join(if rooted { "gc-root" } else { "r0" }).exists());
}

#[test]
fn sender_first_typed_rejection_or_abandonment_closes_channel_then_cleans_original_ownership() {
    let _no_forks = unix::no_forks();
    for rooted in [false, true] {
        for fault in 0..6 {
            let root = unix::temp_directory().unwrap();
            let unrelated = root.join("unrelated");
            unix::private_directory(&unrelated).unwrap();
            fs::write(unrelated.join("keep"), "unrelated").unwrap();
            let other_root = root.join("other-root");
            fs::write(&other_root, "other root").unwrap();
            let mut prepared = Encoder::default()
                .prepare(owned_message(&root, rooted))
                .unwrap();
            let frame = prepared.frames.front_mut().unwrap();
            match fault {
                0 => frame.value["jsonrpc"] = json!("1.0"),
                1 if rooted => frame.value["params"]["root"] = json!(other_root),
                1 => frame.value["params"]["handoff"]["count"] = json!(0),
                2 if rooted => frame.fds[0] = fs::File::open("/dev/null").unwrap().into(),
                2 => frame.value["params"]["handoff"]["dev_shell"] = json!("not JSON"),
                3 => (), // Complete typed message rejected by its receiver.
                4 if rooted => frame.value["params"]["transfer"] = json!(0),
                4 => frame.value["params"]["handoff"]["leases"][0]["directory"] = json!(unrelated),
                5 => {
                    let index = frame.fds.len() - 1;
                    frame.fds[index] = fs::File::open(unrelated.join("keep")).unwrap().into();
                }
                _ => unreachable!(),
            }
            let (a, b) = std::os::unix::net::UnixStream::pair().unwrap();
            let mut sender = channel::Channel::new(a).unwrap();
            let mut receiver = channel::Channel::new(b).unwrap();
            let pending = transmitted(prepared, &mut sender);
            let mut decoder = Decoder::default();
            if fault == 3 {
                let received = decoder.receive_channel(&mut receiver).unwrap().unwrap();
                assert_eq!(received.adopted, Some(pending.transfer()));
                // Stale launch/generation: dispose its complete typed state
                // before closing the channel; there is no Rejected reply.
                drop(received);
                decoder.close(&mut receiver);
            } else {
                assert!(decoder.receive_channel(&mut receiver).is_err());
                assert!(receiver.closed());
            }
            closed_sender(&mut sender);
            drop(pending);
            assert_owned_gone(&root, rooted);
            assert_eq!(
                fs::read_to_string(unrelated.join("keep")).unwrap(),
                "unrelated"
            );
            assert_eq!(fs::read_to_string(&other_root).unwrap(), "other root");
            fs::remove_dir_all(root).unwrap();
        }
    }
}

#[test]
fn sender_first_unread_scm_rights_are_discarded_before_eof_releases_the_pending_leases() {
    let _no_forks = unix::no_forks();
    for rooted in [false, true] {
        let root = unix::temp_directory().unwrap();
        let prepared = Encoder::default()
            .prepare(owned_message(&root, rooted))
            .unwrap();
        let (a, b) = std::os::unix::net::UnixStream::pair().unwrap();
        let mut sender = channel::Channel::new(a).unwrap();
        let mut receiver = channel::Channel::new(b).unwrap();
        let pending = transmitted(prepared, &mut sender);
        // Never receive the descriptor-bearing frame. Socket close must
        // discard the kernel's unread SCM_RIGHTS copies too.
        Decoder::default().close(&mut receiver);
        closed_sender(&mut sender);
        drop(pending);
        assert_owned_gone(&root, rooted);
        fs::remove_dir_all(root).unwrap();
    }
}

#[test]
fn sender_first_invalid_continuation_drops_decoded_and_unread_batches_before_cleanup() {
    let _no_forks = unix::no_forks();
    let root = unix::temp_directory().unwrap();
    let mut prepared = Encoder::default()
        .prepare(Message::Command(Command::Start(Box::new(handoff(
            &root,
            channel::MAX_FDS * 2 + 1,
        )))))
        .unwrap();
    prepared.frames[1].value["params"]["transfer"] = json!(999);
    let (a, b) = std::os::unix::net::UnixStream::pair().unwrap();
    let mut sender = channel::Channel::new(a).unwrap();
    let mut receiver = channel::Channel::new(b).unwrap();
    let pending = transmitted(prepared, &mut sender);
    let mut decoder = Decoder::default();
    assert!(decoder.receive_channel(&mut receiver).unwrap().is_none());
    assert!(decoder.receive_channel(&mut receiver).is_err());
    closed_sender(&mut sender);
    drop(pending);
    for n in 0..=channel::MAX_FDS * 2 {
        assert!(!root.join(format!("r{n}")).exists());
    }
    fs::remove_dir_all(root).unwrap();
}

fn send_truncated_rights(stream: &std::os::unix::net::UnixStream, frame: &Frame) {
    use std::mem;
    let bytes = format!(
        "Content-Length: 100\r\nDescriptor-Count: {}\r\n\r\n{{",
        frame.fds.len()
    )
    .into_bytes();
    let size = unsafe { libc::CMSG_SPACE((frame.fds.len() * mem::size_of::<i32>()) as _) } as usize;
    let mut ancillary = vec![0usize; size.div_ceil(mem::size_of::<usize>())];
    let mut iov = libc::iovec {
        iov_base: bytes.as_ptr().cast_mut().cast(),
        iov_len: bytes.len(),
    };
    let mut msg: libc::msghdr = unsafe { mem::zeroed() };
    msg.msg_iov = &mut iov;
    msg.msg_iovlen = 1;
    msg.msg_control = ancillary.as_mut_ptr().cast();
    msg.msg_controllen = size;
    unsafe {
        let header = libc::CMSG_FIRSTHDR(&msg);
        (*header).cmsg_level = libc::SOL_SOCKET;
        (*header).cmsg_type = libc::SCM_RIGHTS;
        (*header).cmsg_len = libc::CMSG_LEN((frame.fds.len() * mem::size_of::<i32>()) as _) as _;
        let data = libc::CMSG_DATA(header).cast::<i32>();
        for (index, fd) in frame.fds.iter().enumerate() {
            data.add(index).write(fd.as_raw_fd());
        }
        assert_eq!(
            libc::sendmsg(stream.as_raw_fd(), &msg, libc::MSG_NOSIGNAL),
            bytes.len() as isize
        );
    }
}

#[test]
fn truncated_frame_before_typed_adoption_releases_sender_pending_ownership_after_eof() {
    let _no_forks = unix::no_forks();
    for rooted in [false, true] {
        let root = unix::temp_directory().unwrap();
        let mut prepared = Encoder::default()
            .prepare(owned_message(&root, rooted))
            .unwrap();
        let frame = prepared.frames.pop_front().unwrap();
        let (a, b) = std::os::unix::net::UnixStream::pair().unwrap();
        // Simulate a faulty sender that attached the correct capabilities
        // but died during its body; only its write half is closed here so
        // this fixture can observe receiver EOF and release the retained lease.
        send_truncated_rights(&a, &frame);
        drop(frame);
        a.shutdown(std::net::Shutdown::Write).unwrap();
        let pending = prepared.into_pending().unwrap().unwrap();
        let mut sender = channel::Channel::new(a).unwrap();
        let mut receiver = channel::Channel::new(b).unwrap();
        assert!(Decoder::default().receive_channel(&mut receiver).is_err());
        assert!(receiver.closed());
        closed_sender(&mut sender);
        drop(pending);
        assert_owned_gone(&root, rooted);
        fs::remove_dir_all(root).unwrap();
    }
}

#[test]
fn typed_adoption_disarms_pending_sender_and_lost_ack_keeps_live_receiver() {
    let _no_forks = unix::no_forks();
    for (rooted, acknowledge) in [(false, false), (false, true), (true, false), (true, true)] {
        let root = unix::temp_directory().unwrap();
        let prepared = Encoder::default()
            .prepare(owned_message(&root, rooted))
            .unwrap();
        let (a, b) = std::os::unix::net::UnixStream::pair().unwrap();
        let mut sender = channel::Channel::new(a).unwrap();
        let mut receiver = channel::Channel::new(b).unwrap();
        let mut pending = transmitted(prepared, &mut sender);
        let received = Decoder::default()
            .receive_channel(&mut receiver)
            .unwrap()
            .unwrap();
        let transfer = received.adopted.unwrap();
        assert_eq!(transfer, pending.transfer());
        if acknowledge {
            assert!(pending.adopted(transfer + 1).is_err());
            pending.adopted(transfer).unwrap();
            assert!(pending.adopted(transfer).is_err());
        }
        drop(pending);
        assert!(
            root.join(if rooted { "gc-root" } else { "r0/held" })
                .exists()
        );
        drop(received);
        assert_owned_gone(&root, rooted);
        fs::remove_dir_all(root).unwrap();
    }
}

#[test]
fn member_startup_version_rejection_keeps_sender_ownership_until_receiver_eof() {
    let _no_forks = unix::no_forks();
    let root = unix::temp_directory().unwrap();
    let source = Source::Member {
        scope: None,
        owner_scope: None,
        manifest: "/manifest".into(),
        configuration: "child".into(),
        auto: Default::default(),
        state: "/state".into(),
        history: None,
    };
    let mut prepared = Encoder::default()
        .prepare(startup(source, Some(handoff(&root, 1))))
        .unwrap();
    prepared.frames[0].value["params"]["supervisor_api"] = json!(API + 1);
    let (a, b) = std::os::unix::net::UnixStream::pair().unwrap();
    let mut sender = channel::Channel::new(a).unwrap();
    let mut receiver = channel::Channel::new(b).unwrap();
    let pending = transmitted(prepared, &mut sender);
    assert!(Decoder::default().receive_channel(&mut receiver).is_err());
    closed_sender(&mut sender);
    drop(pending);
    assert!(!root.join("r0").exists());
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn preflight_scalar_vocabulary_is_strict_and_preserves_relative_manifest_paths() {
    let request = || Message::Preflight {
        supervisor_api: API,
        path: "relative/config.json".into(),
        directory: "/state/manifests".into(),
        sequence: 17,
    };
    roundtrip(request());
    roundtrip(Message::PreflightFailed {
        error: "original operation diagnostic".into(),
    });
    for (key, value) in [
        ("supervisor_api", json!(2)),
        ("directory", json!("relative")),
        ("unexpected", json!(true)),
    ] {
        let mut prepared = Encoder::default().prepare(request()).unwrap();
        let mut frame = prepared.frames.pop_front().unwrap();
        frame.value["params"][key] = value;
        assert!(Decoder::default().receive(frame).is_err(), "{key}");
    }
    assert!(
        Encoder::default()
            .prepare(Message::Preflight {
                supervisor_api: API,
                path: "/config".into(),
                directory: "relative".into(),
                sequence: 0,
            })
            .is_err()
    );
    let mut prepared = Encoder::default().prepare(request()).unwrap();
    let mut frame = prepared.frames.pop_front().unwrap();
    frame.fds.push(fs::File::open("/dev/null").unwrap().into());
    assert!(Decoder::default().receive(frame).is_err());
}

fn maintenance_request() -> Message {
    let writable =
        || OwnedFd::from(fs::File::options().write(true).open("/dev/null").unwrap()).into();
    Message::Maintenance(Maintenance {
        supervisor_api: API,
        operation: MaintenanceOperation::Gc {
            runtime: None,
            state: "relative/state".into(),
            dry_run: true,
        },
        stdout: writable(),
        stderr: writable(),
    })
}
#[test]
fn maintenance_scalar_contract_preserves_paths_flags_and_has_no_ownership_ack() {
    let mut encoded = Encoder::default().prepare(maintenance_request()).unwrap();
    assert!(!encoded.needs_adoption());
    assert_eq!(encoded.frames.len(), 1);
    assert_eq!(encoded.frames[0].fds.len(), 2);
    let received = decode(&mut encoded, &mut Decoder::default());
    assert!(received.adopted.is_none());
    assert!(
        matches!(received.message, Message::Maintenance(Maintenance {
        operation: MaintenanceOperation::Gc { state, runtime: None, dry_run: true }, ..
    }) if state == Path::new("relative/state"))
    );
    roundtrip(maintenance_request());
    roundtrip(Message::MaintenanceFinished { result: Ok(()) });
    roundtrip(Message::MaintenanceFinished {
        result: Err("original cleanup error".into()),
    });
    let writable =
        || OwnedFd::from(fs::File::options().write(true).open("/dev/null").unwrap()).into();
    roundtrip(Message::Maintenance(Maintenance {
        supervisor_api: API,
        operation: MaintenanceOperation::ClearScope {
            runtime: Some("relative/runtime".into()),
            name: Some("work".into()),
            dry_run: false,
        },
        stdout: writable(),
        stderr: writable(),
    }));
}
#[test]
fn maintenance_rejects_bad_version_mode_fields_and_nonwritable_output_capabilities() {
    for (key, value) in [
        ("supervisor_api", json!(2)),
        ("operation", json!({"kind":"Unknown","dry_run":true})),
        (
            "operation",
            json!({"kind":"Gc","runtime":null,"state":"/state","dry_run":true,"extra":true}),
        ),
        ("extra", json!(true)),
    ] {
        let mut prepared = Encoder::default().prepare(maintenance_request()).unwrap();
        let mut frame = prepared.frames.pop_front().unwrap();
        frame.value["params"][key] = value;
        assert!(Decoder::default().receive(frame).is_err());
    }
    let mut prepared = Encoder::default().prepare(maintenance_request()).unwrap();
    let mut frame = prepared.frames.pop_front().unwrap();
    frame.fds[0] = fs::File::open("/dev/null").unwrap().into();
    assert!(Decoder::default().receive(frame).is_err());
    let mut prepared = Encoder::default().prepare(maintenance_request()).unwrap();
    let mut frame = prepared.frames.pop_front().unwrap();
    frame.fds.push(fs::File::open("/dev/null").unwrap().into());
    assert!(Decoder::default().receive(frame).is_err());
}

#[test]
fn startup_payload_limit_is_required_nullable_and_strict() {
    for limit in [None, Some(128)] {
        let Message::SupervisorStart(mut start) = startup(host(), None) else {
            unreachable!()
        };
        start.payload_nofile = limit;
        roundtrip(Message::SupervisorStart(start));
    }
    for value in [
        json!(0),
        json!(u64::MAX),
        json!(-1),
        json!(1.5),
        json!("128"),
        json!(true),
        json!([]),
    ] {
        let mut prepared = Encoder::default().prepare(startup(host(), None)).unwrap();
        let mut frame = prepared.frames.pop_front().unwrap();
        frame.value["params"]["payload_nofile"] = value;
        assert!(Decoder::default().receive(frame).is_err());
    }
    let mut prepared = Encoder::default().prepare(startup(host(), None)).unwrap();
    let mut frame = prepared.frames.pop_front().unwrap();
    frame.value["params"]
        .as_object_mut()
        .unwrap()
        .remove("payload_nofile");
    assert!(Decoder::default().receive(frame).is_err());
    for limit in [0, u64::MAX] {
        let Message::SupervisorStart(mut start) = startup(host(), None) else {
            unreachable!()
        };
        start.payload_nofile = Some(limit);
        assert!(
            Encoder::default()
                .prepare(Message::SupervisorStart(start))
                .is_err()
        );
    }
}
