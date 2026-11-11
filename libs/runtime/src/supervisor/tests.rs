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
            worktree: None,
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
        Command::Spawn {
            id: 14,
            kind: crate::SpawnKind::Terminal { exit_report: Some(15) },
            argv: Some(vec!["fish".into()]),
            cwd: None,
            env: vec![],
            stdio: Stdio::NewPty { rows: 40, cols: 120 },
        },
        Command::KillAll,
        Command::TerminalCall {
            request: 21,
            terminal: 14,
            call: crate::terminal::Call::Read {
                source: crate::terminal::Source::Recent,
                lines: Some(50),
            },
        },
        Command::TerminalCall {
            request: 22,
            terminal: 14,
            call: crate::terminal::Call::Send {
                text: Some("ls\n".into()),
                keys: vec!["enter".into(), "ctrl+c".into()],
            },
        },
        Command::TerminalCall {
            request: 23,
            terminal: 14,
            call: crate::terminal::Call::Wait {
                condition: crate::terminal::Condition::Regex("^done$".into()),
                timeout: 30_000,
                lines: None,
            },
        },
        Command::TerminalCancel { request: 23 },
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
            worktrees: Default::default(),
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
        Event::TerminalResult {
            request: 21,
            result: Ok(json!({"text":"{\"$fd\":0}","alternate_screen":false})),
        },
        Event::TerminalResult {
            request: 22,
            result: Err("no such terminal".into()),
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
        worktree: None,
        arguments: Some(vec!["--model".into(), "x={a}".into()]),
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
        arguments,
        ..
    } = start.source
    else {
        panic!("member");
    };
    assert_eq!(owner_scope.as_deref(), Some("scope"));
    // Chosen launch options cross verbatim, braces included.
    assert_eq!(arguments.unwrap(), ["--model", "x={a}"]);
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
            worktree: None,
            arguments: None,
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
        catalogue: fd_named_catalogue(),
    })
}
/// Manifest names are raw data: one spelled like a descriptor reference
/// must cross as a name.
fn fd_named_catalogue() -> crate::config::Catalogue {
    let entry = crate::config::CatalogueEntry {
        description: String::new(),
        args: vec![],
        scope_args: std::collections::BTreeMap::from([("$fd".into(), vec![])]),
        launch_options: Default::default(),
    };
    crate::config::Catalogue {
        goblins: std::collections::BTreeMap::from([("$fd".into(), entry)]),
    }
}
#[test]
fn manifest_adoption_survives_lost_ack_and_rejected_generation_cleans_the_correct_root() {
    let _no_forks = unix::no_forks();
    for receiver_first in [false, true] {
        let root = unix::temp_directory().unwrap();
        let mut prepared = Encoder::default().prepare(manifest(&root)).unwrap();
        // An opaque string: a structured value with a "$fd" key would be
        // taken for a descriptor reference by the channel.
        assert!(prepared.frames.front().unwrap().value["params"]["catalogue"].is_string());
        let received = decode(&mut prepared, &mut Decoder::default());
        let Message::Event(Event::Manifest { catalogue, .. }) = &received.message else {
            panic!("manifest");
        };
        assert_eq!(catalogue, &fd_named_catalogue());
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
        worktree: None,
        arguments: None,
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
            catalogue: Default::default(),
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
        worktree: None,
        arguments: None,
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

// Frozen pre-C16 decoder: keep independent of the production wire declaration.
mod c16a_original {
    use super::*;
    #[derive(Debug, Serialize, Deserialize)]
    #[serde(tag = "kind", deny_unknown_fields)]
    pub enum MaintenanceOperation {
        Gc {
            runtime: Option<PathBuf>,
            state: PathBuf,
            dry_run: bool,
        },
        ClearScope {
            runtime: Option<PathBuf>,
            name: Option<String>,
            dry_run: bool,
        },
    }
}

#[test]
fn c16a_maintenance_operation_preserves_original_json_and_diagnostics() {
    let cases = [
        json!({"kind":"Gc","state":"relative/雪\u{0000}","dry_run":true}),
        json!({"kind":"Gc","runtime":null,"state":"","dry_run":false}),
        json!({"kind":"Gc","runtime":"../runtime","state":"/state","dry_run":true}),
        json!({"kind":"ClearScope","dry_run":false}),
        json!({"kind":"ClearScope","runtime":null,"name":null,"dry_run":true}),
        json!({"kind":"ClearScope","runtime":"","name":"雪\n","dry_run":false}),
        json!(null),
        json!([]),
        json!("Gc"),
        json!({}),
        json!({"kind":null}),
        json!({"kind":42}),
        json!({"kind":"gc"}),
        json!({"kind":"Gc","dry_run":true}),
        json!({"kind":"Gc","state":null,"dry_run":true}),
        json!({"kind":"Gc","state":[],"dry_run":true}),
        json!({"kind":"Gc","state":"x"}),
        json!({"kind":"Gc","state":"x","dry_run":null}),
        json!({"kind":"Gc","state":"x","dry_run":0}),
        json!({"kind":"Gc","state":"x","dry_run":true,"extra":1}),
        json!({"kind":"ClearScope","name":false,"dry_run":true}),
        json!({"kind":"ClearScope","runtime":{},"dry_run":true}),
        json!({"kind":"ClearScope","dry_run":true,"state":"x"}),
    ];
    for input in cases {
        let expected = serde_json::from_value::<c16a_original::MaintenanceOperation>(input.clone())
            .map(|v| serde_json::to_value(v).unwrap())
            .map_err(|e| e.to_string());
        let actual = serde_json::from_value::<MaintenanceOperation>(input.clone())
            .map(|v| serde_json::to_value(v).unwrap())
            .map_err(|e| e.to_string());
        assert_eq!(actual, expected, "input {input}");
    }
    assert_eq!(
        serde_json::to_value(MaintenanceOperation::Gc {
            runtime: None,
            state: "relative/state".into(),
            dry_run: true,
        })
        .unwrap(),
        json!({"kind":"Gc","runtime":null,"state":"relative/state","dry_run":true})
    );
    assert_eq!(
        serde_json::to_value(MaintenanceOperation::ClearScope {
            runtime: Some("../runtime".into()),
            name: None,
            dry_run: false,
        })
        .unwrap(),
        json!({"kind":"ClearScope","runtime":"../runtime","name":null,"dry_run":false})
    );
}

fn c16a_frame(message: Message) -> Frame {
    let mut prepared = Encoder::default().prepare(message).unwrap();
    assert_eq!(prepared.frames.len(), 1);
    prepared.frames.pop_front().unwrap()
}
fn c16a_error(frame: Frame, expected: &str) {
    let error = match Decoder::default().receive(frame) {
        Err(error) => error,
        Ok(_) => panic!("accepted malformed frame; expected {expected}"),
    };
    assert_eq!(error.kind(), io::ErrorKind::InvalidData);
    assert_eq!(error.to_string(), expected);
}

#[test]
fn c16a_scalar_producers_pin_envelopes_and_transfer_metadata() {
    let cases = vec![
        (Message::Stopping, "Stopping", json!({})),
        (Message::Released, "Released", json!({})),
        (
            Message::Adopted { transfer: 0 },
            "Adopted",
            json!({"transfer":0}),
        ),
        (
            Message::Adopted { transfer: u64::MAX },
            "Adopted",
            json!({"transfer":u64::MAX}),
        ),
        (
            Message::MaintenanceFinished { result: Ok(()) },
            "MaintenanceFinished",
            json!({"result":{"Ok":null}}),
        ),
        (
            Message::MaintenanceFinished {
                result: Err("雪\nfailed".into()),
            },
            "MaintenanceFinished",
            json!({"result":{"Err":"雪\nfailed"}}),
        ),
        (
            Message::Preflight {
                supervisor_api: API,
                path: "relative/manifest".into(),
                directory: "/state".into(),
                sequence: u64::MAX,
            },
            "Preflight",
            json!({"supervisor_api":1,"path":"relative/manifest","directory":"/state","sequence":u64::MAX}),
        ),
        (
            Message::PreflightFailed {
                error: "failed\n".into(),
            },
            "PreflightFailed",
            json!({"error":"failed\n"}),
        ),
    ];
    let mut encoder = Encoder::default();
    for (index, (message, method, params)) in cases.into_iter().enumerate() {
        let mut prepared = encoder.prepare(message).unwrap();
        assert_eq!(prepared.transfer(), index as u64 + 1);
        assert!(!prepared.needs_adoption());
        assert_eq!(prepared.frames.len(), 1);
        assert!(prepared.frames[0].fds.is_empty());
        assert_eq!(
            prepared.frames[0].value,
            json!({"jsonrpc":"2.0","method":method,"params":params})
        );
        assert!(
            decode(&mut prepared, &mut Decoder::default())
                .adopted
                .is_none()
        );
    }
    let mut exhausted = Encoder { next: u64::MAX };
    assert_eq!(
        exhausted
            .prepare(Message::Stopping)
            .err()
            .unwrap()
            .to_string(),
        "transfer id exhausted"
    );
    let mut encoder = Encoder::default();
    assert!(
        encoder
            .prepare(Message::Preflight {
                supervisor_api: 2,
                path: "x".into(),
                directory: "/".into(),
                sequence: 0
            })
            .is_err()
    );
    assert_eq!(encoder.prepare(Message::Stopping).unwrap().transfer(), 2);
}

#[test]
fn c16a_bootstrap_and_maintenance_pin_exact_frames_and_fd_indexes() {
    let mut prepared = Encoder::default().prepare(startup(host(), None)).unwrap();
    let expected = json!({"jsonrpc":"2.0","method":"SupervisorStart","params":{
        "supervisor_api":1,"directory":"/state/session/resources","dimensions":[37,123],"detached":true,
        "payload_nofile":null,"legacy_listener":null,"handoff":null,
        "source":{"kind":"Host","configuration":"/config.json","name":"shell","scope":"shared",
            "dev_shell":"/flake#dev","workspace":"/workspace","cwd":"/workspace/sub","state":"/state",
            "history":"/history","sequence":23,"recreate":{"packages":["hello"],"auto_approve":["jq"],
            "docker":true,"variables":{"session":"abc"},"worktree":null}}
    }});
    assert_eq!(prepared.frames[0].value, expected);
    assert!(prepared.frames[0].fds.is_empty());
    assert!(!prepared.needs_adoption());
    assert!(
        decode(&mut prepared, &mut Decoder::default())
            .adopted
            .is_none()
    );
    let mut start = match startup(host(), None) {
        Message::SupervisorStart(s) => s,
        _ => unreachable!(),
    };
    start.legacy_listener = Some(null());
    start.payload_nofile = Some(128);
    let mut prepared = Encoder::default()
        .prepare(Message::SupervisorStart(start))
        .unwrap();
    let mut expected = expected;
    expected["params"]["legacy_listener"] = json!({"$fd":0});
    expected["params"]["payload_nofile"] = json!(128);
    assert_eq!(prepared.frames[0].value, expected);
    assert_eq!(prepared.frames[0].fds.len(), 1);
    assert!(!prepared.needs_adoption());
    assert!(
        decode(&mut prepared, &mut Decoder::default())
            .adopted
            .is_none()
    );
    let mut prepared = Encoder::default().prepare(maintenance_request()).unwrap();
    assert_eq!(
        prepared.frames[0].value,
        json!({"jsonrpc":"2.0","method":"Maintenance","params":{
            "supervisor_api":1,"operation":{"kind":"Gc","runtime":null,"state":"relative/state","dry_run":true},
            "stdout":{"$fd":0},"stderr":{"$fd":1}
        }})
    );
    assert_eq!(prepared.frames[0].fds.len(), 2);
    assert!(!prepared.needs_adoption());
    assert!(
        decode(&mut prepared, &mut Decoder::default())
            .adopted
            .is_none()
    );
}

#[test]
fn c16a_control_decoders_pin_missing_fields_and_validation_precedence() {
    for (message, fields) in [
        (
            startup(host(), None),
            vec![
                "supervisor_api",
                "source",
                "directory",
                "dimensions",
                "detached",
                "payload_nofile",
                "handoff",
            ],
        ),
        (
            maintenance_request(),
            vec!["supervisor_api", "operation", "stdout", "stderr"],
        ),
        (
            Message::Preflight {
                supervisor_api: 1,
                path: "relative".into(),
                directory: "/state".into(),
                sequence: 0,
            },
            vec!["supervisor_api", "path", "directory", "sequence"],
        ),
        (
            Message::PreflightFailed { error: "x".into() },
            vec!["error"],
        ),
        (
            Message::MaintenanceFinished { result: Ok(()) },
            vec!["result"],
        ),
        (Message::Adopted { transfer: 1 }, vec!["transfer"]),
    ] {
        let mut prepared = Encoder::default().prepare(message).unwrap();
        let original = prepared.frames.pop_front().unwrap();
        for field in fields {
            let mut value = original.value.clone();
            value["params"].as_object_mut().unwrap().remove(field);
            let fds = original
                .fds
                .iter()
                .map(|fd| fd.try_clone().unwrap())
                .collect();
            c16a_error(
                Frame { value, fds },
                &format!("missing control field {field}"),
            );
        }
        let mut value = original.value.clone();
        value["params"]["extra"] = json!(true);
        c16a_error(
            Frame {
                value,
                fds: original.fds,
            },
            "unexpected control field",
        );
    }
    let mut start = c16a_frame(startup(host(), None));
    start.value["params"]["supervisor_api"] = json!(2);
    start.value["params"]
        .as_object_mut()
        .unwrap()
        .remove("source");
    c16a_error(start, "incompatible supervisor API");
    let mut preflight = c16a_frame(Message::Preflight {
        supervisor_api: 1,
        path: "x".into(),
        directory: "/".into(),
        sequence: 0,
    });
    preflight.value["params"]["supervisor_api"] = json!(2);
    preflight.value["params"]["extra"] = json!(true);
    c16a_error(preflight, "unexpected control field");
    let mut maintenance = c16a_frame(maintenance_request());
    maintenance.value["params"]["supervisor_api"] = json!(2);
    maintenance.value["params"]["stderr"] = json!({"$fd":0});
    c16a_error(maintenance, "invalid or duplicate descriptor reference");
    let mut legacy = c16a_frame(startup(host(), None));
    legacy.value["params"]
        .as_object_mut()
        .unwrap()
        .remove("legacy_listener");
    assert!(Decoder::default().receive(legacy).unwrap().is_some());
}

#[test]
fn c16a_control_envelopes_and_scalar_types_keep_exact_errors() {
    for (value, expected) in [
        (json!([]), "expected control object"),
        (json!({}), "missing control field jsonrpc"),
        (
            json!({"jsonrpc":"1.0"}),
            "invalid supervisor protocol envelope",
        ),
        (json!({"jsonrpc":"2.0"}), "missing control field method"),
        (
            json!({"jsonrpc":"2.0","method":"Stopping"}),
            "missing control field params",
        ),
        (
            json!({"jsonrpc":"2.0","method":"Stopping","params":null}),
            "expected control object",
        ),
        (
            json!({"jsonrpc":"2.0","method":"unknown","params":{},"extra":true}),
            "unexpected control field",
        ),
        (
            json!({"jsonrpc":"2.0","method":"unknown","params":{}}),
            "unknown supervisor message",
        ),
        (
            json!({"jsonrpc":"2.0","method":"Stopping","params":{"extra":true}}),
            "unexpected control field",
        ),
        (
            json!({"jsonrpc":"2.0","method":"Released","params":{"extra":true}}),
            "unexpected control field",
        ),
    ] {
        c16a_error(Frame { value, fds: vec![] }, expected);
    }
    for value in [
        json!(null),
        json!(-1),
        json!(1.5),
        json!("1"),
        json!(true),
        json!([]),
    ] {
        let expected = serde_json::from_value::<u64>(value.clone())
            .unwrap_err()
            .to_string();
        let mut frame = c16a_frame(Message::Adopted { transfer: 1 });
        frame.value["params"]["transfer"] = value;
        c16a_error(frame, &expected);
    }
    for value in [
        json!(null),
        json!({"Ok":1}),
        json!({"Err":null}),
        json!({"Unknown":null}),
        json!({"Ok":null,"Err":"x"}),
    ] {
        let expected = serde_json::from_value::<Result<(), String>>(value.clone())
            .unwrap_err()
            .to_string();
        let mut frame = c16a_frame(Message::MaintenanceFinished { result: Ok(()) });
        frame.value["params"]["result"] = value;
        c16a_error(frame, &expected);
    }
}

#[test]
fn c16a_rejected_descriptor_indexes_close_received_copies_but_keep_sender_ownership() {
    let _no_forks = unix::no_forks();
    for duplicate in [true, false] {
        let (reader, writer) = unix::pipe().unwrap();
        let writer_copy = writer.try_clone().unwrap();
        let mut reader = fs::File::from(reader);
        let mut prepared = Encoder::default()
            .prepare(Message::Maintenance(Maintenance {
                supervisor_api: API,
                operation: MaintenanceOperation::ClearScope {
                    runtime: None,
                    name: None,
                    dry_run: false,
                },
                stdout: writer.into(),
                stderr: writer_copy.into(),
            }))
            .unwrap();
        let mut frame = prepared.frames.pop_front().unwrap();
        frame.value["params"]["stderr"] = json!({"$fd": if duplicate { 0 } else { 2 }});
        c16a_error(frame, "invalid or duplicate descriptor reference");
        assert!(
            !unix::readable(reader.as_raw_fd(), 0).unwrap(),
            "sender must retain both original writers"
        );
        drop(prepared);
        assert!(
            unix::readable(reader.as_raw_fd(), 100).unwrap(),
            "decoder must not leak received writers"
        );
        assert_eq!(reader.read(&mut [0]).unwrap(), 0);
    }
}

#[test]
fn c16a1_preflight_float_api_matches_original_u32_diagnostic() {
    let frame = Frame {
        value: json!({"jsonrpc":"2.0","method":"Preflight","params":{
            "supervisor_api":1.0,"path":"relative/manifest","directory":"/state","sequence":0
        }}),
        fds: vec![],
    };
    let expected = serde_json::from_value::<u32>(json!(1.0)).unwrap_err().to_string();
    let error = match Decoder::default().receive(frame) {
        Err(error) => error,
        Ok(_) => panic!("Preflight accepted floating-point supervisor_api"),
    };
    assert_eq!(error.kind(), io::ErrorKind::InvalidData);
    assert_eq!(error.to_string(), expected);
}

// Independent original Descriptor declaration pins Serde's named and sequence forms.
mod c16a2_original {
    use super::*;
    #[derive(Debug, Clone, Copy, Serialize, Deserialize)]
    #[serde(deny_unknown_fields)]
    pub struct Descriptor {
        #[serde(rename = "$fd")]
        pub index: usize,
    }
}

#[test]
fn c16a2_descriptor_preserves_original_serde_forms_traits_and_exact_diagnostics() {
    fn copy_debug<T: Copy + Clone + std::fmt::Debug>() {}
    copy_debug::<Descriptor>();
    let cases = [
        "{\"$fd\":0}",
        "{\"$fd\":239}",
        "{\"$fd\":18446744073709551615}",
        "[0]",
        "[239]",
        "[18446744073709551615]",
        "[]",
        "[0,1]",
        "[null]",
        "null",
        "true",
        "0",
        "\"0\"",
        "{}",
        "{\"index\":0}",
        "{\"$fd\":null}",
        "{\"$fd\":true}",
        "{\"$fd\":\"0\"}",
        "{\"$fd\":[]}",
        "{\"$fd\":{}}",
        "{\"$fd\":-1}",
        "{\"$fd\":1.0}",
        "{\"$fd\":1.5}",
        "{\"$fd\":18446744073709551616}",
        "{\"$fd\":0,\"extra\":1}",
        "{\"extra\":1,\"$fd\":0}",
        "{\"$fd\":0,\"$fd\":1}",
    ];
    for input in cases {
        let expected = serde_json::from_str::<c16a2_original::Descriptor>(input)
            .map(|v| {
                (
                    v.index,
                    format!("{v:?}"),
                    serde_json::to_string(&v).unwrap(),
                )
            })
            .map_err(|e| e.to_string());
        let actual = serde_json::from_str::<Descriptor>(input)
            .map(|v| {
                (
                    v.index,
                    format!("{v:?}"),
                    serde_json::to_string(&v).unwrap(),
                )
            })
            .map_err(|e| e.to_string());
        assert_eq!(actual, expected, "raw JSON {input}");
        let value: Value = serde_json::from_str(input).unwrap();
        let expected = serde_json::from_value::<c16a2_original::Descriptor>(value.clone())
            .map(|v| v.index)
            .map_err(|e| e.to_string());
        let actual = serde_json::from_value::<Descriptor>(value)
            .map(|v| v.index)
            .map_err(|e| e.to_string());
        assert_eq!(actual, expected, "Value input {input}");
    }
    for index in [0, 239, 240, usize::MAX] {
        let d = Descriptor::new(index);
        assert_eq!(d.index, index);
        assert_eq!(serde_json::to_value(d).unwrap(), json!({"$fd":index}));
    }
}

#[test]
fn c16a2_bootstrap_scalar_types_ranges_and_error_precedence() {
    fn error<T: DeserializeOwned>(value: Value) -> String {
        serde_json::from_value::<T>(value)
            .err()
            .unwrap()
            .to_string()
    }
    let cases = vec![
        ("supervisor_api", json!(1.0), error::<u32>(json!(1.0))),
        (
            "supervisor_api",
            json!(4294967296_u64),
            error::<u32>(json!(4294967296_u64)),
        ),
        ("directory", json!(null), error::<PathBuf>(json!(null))),
        (
            "directory",
            json!("relative"),
            "invalid supervisor startup parameters".into(),
        ),
        ("dimensions", json!(null), error::<(u16, u16)>(json!(null))),
        ("dimensions", json!([1]), error::<(u16, u16)>(json!([1]))),
        (
            "dimensions",
            json!([1, 2, 3]),
            error::<(u16, u16)>(json!([1, 2, 3])),
        ),
        (
            "dimensions",
            json!([1.0, 2]),
            error::<(u16, u16)>(json!([1.0, 2])),
        ),
        (
            "dimensions",
            json!([65536, 2]),
            error::<(u16, u16)>(json!([65536, 2])),
        ),
        (
            "dimensions",
            json!([0, 2]),
            "invalid supervisor startup parameters".into(),
        ),
        (
            "dimensions",
            json!([1, 1001]),
            "invalid supervisor startup parameters".into(),
        ),
        ("detached", json!(null), error::<bool>(json!(null))),
        ("detached", json!(0), error::<bool>(json!(0))),
        (
            "payload_nofile",
            json!(1.0),
            error::<Option<u64>>(json!(1.0)),
        ),
        (
            "payload_nofile",
            json!(0),
            "invalid payload descriptor limit".into(),
        ),
        (
            "payload_nofile",
            json!(u64::MAX),
            "invalid payload descriptor limit".into(),
        ),
    ];
    for (field, value, expected) in cases {
        let mut frame = c16a_frame(startup(host(), None));
        frame.value["params"][field] = value;
        c16a_error(frame, &expected);
    }
    // Dimension decoding precedes detached decoding; unknown fields precede
    // semantic dimensions/payload validation, exactly as in the original reader.
    let mut frame = c16a_frame(startup(host(), None));
    frame.value["params"]["dimensions"] = json!([1]);
    frame.value["params"]["detached"] = json!(null);
    c16a_error(frame, &error::<(u16, u16)>(json!([1])));
    let mut frame = c16a_frame(startup(host(), None));
    frame.value["params"]["dimensions"] = json!([0, 0]);
    frame.value["params"]["payload_nofile"] = json!(0);
    frame.value["params"]["extra"] = json!(true);
    c16a_error(frame, "unexpected control field");
    for dimensions in [(1, 1), (1000, 1000)] {
        for payload in [None, Some(1), Some(u64::MAX - 1)] {
            let Message::SupervisorStart(mut start) = startup(host(), None) else {
                unreachable!()
            };
            start.dimensions = dimensions;
            start.detached = false;
            start.payload_nofile = payload;
            let mut prepared = Encoder::default()
                .prepare(Message::SupervisorStart(start))
                .unwrap();
            let received = decode(&mut prepared, &mut Decoder::default());
            assert!(received.adopted.is_none());
            assert!(
                matches!(received.message, Message::SupervisorStart(s) if s.dimensions==dimensions && s.payload_nofile==payload && !s.detached)
            );
        }
    }
}

#[test]
fn c16a2_listener_optional_and_descriptor_forms_preserve_original_acceptance() {
    for value in [
        None,
        Some(json!(null)),
        Some(json!({"$fd":0})),
        Some(json!([0])),
    ] {
        let present = value.as_ref().is_some_and(|v| !v.is_null());
        let mut frame = c16a_frame(startup(host(), None));
        match value {
            None => {
                frame.value["params"]
                    .as_object_mut()
                    .unwrap()
                    .remove("legacy_listener");
            }
            Some(value) => frame.value["params"]["legacy_listener"] = value,
        }
        if present {
            frame.fds.push(fs::File::open("/dev/null").unwrap().into());
        }
        let received = Decoder::default().receive(frame).unwrap().unwrap();
        assert!(received.adopted.is_none());
        let Message::SupervisorStart(start) = received.message else {
            unreachable!()
        };
        assert_eq!(start.legacy_listener.is_some(), present);
        let prepared = Encoder::default()
            .prepare(Message::SupervisorStart(start))
            .unwrap();
        assert!(!prepared.needs_adoption());
        assert_eq!(prepared.frames[0].fds.len(), usize::from(present));
        assert_eq!(
            prepared.frames[0].value["params"]["legacy_listener"],
            if present {
                json!({"$fd":0})
            } else {
                json!(null)
            }
        );
    }
    for value in [
        json!({}),
        json!({"$fd":1.0}),
        json!({"$fd":-1}),
        json!({"$fd":null}),
        json!({"$fd":0,"extra":1}),
        json!([]),
    ] {
        let expected = serde_json::from_value::<Option<c16a2_original::Descriptor>>(value.clone())
            .unwrap_err()
            .to_string();
        let mut frame = c16a_frame(startup(host(), None));
        frame.value["params"]["legacy_listener"] = value;
        c16a_error(frame, &expected);
    }
    for index in [0, 1, usize::MAX] {
        let mut frame = c16a_frame(startup(host(), None));
        frame.value["params"]["legacy_listener"] = json!({"$fd":index});
        c16a_error(frame, "invalid or duplicate descriptor reference");
    }
    let mut frame = c16a_frame(startup(host(), None));
    frame.fds.push(fs::File::open("/dev/null").unwrap().into());
    c16a_error(frame, "unreferenced control descriptor");
}

#[test]
fn c16a2_maintenance_preserves_optional_operations_and_exact_stream_capabilities() {
    let _no_forks = unix::no_forks();
    for positional in [false, true] {
        let (out_read, out_write) = unix::pipe().unwrap();
        let (err_read, err_write) = unix::pipe().unwrap();
        let mut out_read = fs::File::from(out_read);
        let mut err_read = fs::File::from(err_read);
        let mut prepared = Encoder::default()
            .prepare(Message::Maintenance(Maintenance {
                supervisor_api: API,
                operation: MaintenanceOperation::ClearScope {
                    runtime: None,
                    name: None,
                    dry_run: false,
                },
                stdout: out_write.into(),
                stderr: err_write.into(),
            }))
            .unwrap();
        let mut frame = prepared.frames.pop_front().unwrap();
        frame.value["params"]["operation"]
            .as_object_mut()
            .unwrap()
            .remove("runtime");
        frame.value["params"]["operation"]
            .as_object_mut()
            .unwrap()
            .remove("name");
        if positional {
            frame.value["params"]["stdout"] = json!([1]);
            frame.value["params"]["stderr"] = json!([0]);
        }
        let received = Decoder::default().receive(frame).unwrap().unwrap();
        assert!(received.adopted.is_none());
        let Message::Maintenance(request) = received.message else {
            unreachable!()
        };
        assert!(matches!(
            request.operation,
            MaintenanceOperation::ClearScope {
                runtime: None,
                name: None,
                dry_run: false
            }
        ));
        let mut stdout = fs::File::from(OwnedFd::from(request.stdout));
        let mut stderr = fs::File::from(OwnedFd::from(request.stderr));
        stdout.write_all(b"stdout").unwrap();
        stderr.write_all(b"stderr").unwrap();
        let mut out = [0; 6];
        let mut err = [0; 6];
        out_read.read_exact(&mut out).unwrap();
        err_read.read_exact(&mut err).unwrap();
        assert_eq!(&out, if positional { b"stderr" } else { b"stdout" });
        assert_eq!(&err, if positional { b"stdout" } else { b"stderr" });
    }
    let mut frame = c16a_frame(maintenance_request());
    frame.value["params"]["operation"] = json!({"kind":"Gc","state":"relative","dry_run":true});
    let received = Decoder::default().receive(frame).unwrap().unwrap();
    let mut repeated = Encoder::default().prepare(received.message).unwrap();
    assert_eq!(
        repeated.frames[0].value["params"]["operation"],
        json!({"kind":"Gc","runtime":null,"state":"relative","dry_run":true})
    );
    assert!(
        decode(&mut repeated, &mut Decoder::default())
            .adopted
            .is_none()
    );
}

#[test]
fn c16a2_bootstrap_rejection_closes_listener_after_original_error_order() {
    let _no_forks = unix::no_forks();
    for (extra, expected) in [
        (false, "invalid supervisor startup parameters"),
        (true, "unexpected control field"),
    ] {
        let (reader, writer) = unix::pipe().unwrap();
        let mut reader = fs::File::from(reader);
        let Message::SupervisorStart(mut start) = startup(host(), None) else {
            unreachable!()
        };
        start.legacy_listener = Some(writer.into());
        let mut prepared = Encoder::default()
            .prepare(Message::SupervisorStart(start))
            .unwrap();
        let mut frame = prepared.frames.pop_front().unwrap();
        frame.value["params"]["dimensions"] = json!([0, 0]);
        frame.value["params"]["payload_nofile"] = json!(0);
        if extra {
            frame.value["params"]["extra"] = json!(true);
        }
        c16a_error(frame, expected);
        assert!(!unix::readable(reader.as_raw_fd(), 0).unwrap());
        drop(prepared);
        assert!(unix::readable(reader.as_raw_fd(), 100).unwrap());
        assert_eq!(reader.read(&mut [0]).unwrap(), 0);
    }
    let mut frame = c16a_frame(maintenance_request());
    frame.value["params"]["operation"] = json!({"kind":"Gc","dry_run":true});
    frame.value["params"]["stdout"] = json!({"$fd":usize::MAX});
    let expected = serde_json::from_value::<c16a_original::MaintenanceOperation>(
        json!({"kind":"Gc","dry_run":true}),
    )
    .unwrap_err()
    .to_string();
    c16a_error(frame, &expected);
}

#[test]
fn c16a2_maintenance_descriptor_diagnostics_and_api_validation_order() {
    for field in ["stdout", "stderr"] {
        for value in [
            json!({}),
            json!({"$fd":1.0}),
            json!({"$fd":-1}),
            json!({"$fd":null}),
            json!({"$fd":0,"extra":true}),
            json!([]),
        ] {
            let expected = serde_json::from_value::<c16a2_original::Descriptor>(value.clone())
                .unwrap_err()
                .to_string();
            let mut frame = c16a_frame(maintenance_request());
            frame.value["params"][field] = value;
            c16a_error(frame, &expected);
        }
    }
    for api in [
        json!(-1),
        json!(1.0),
        json!(4294967296_u64),
        json!(null),
        json!(true),
    ] {
        let expected = serde_json::from_value::<u32>(api.clone())
            .unwrap_err()
            .to_string();
        let mut frame = c16a_frame(maintenance_request());
        frame.value["params"]["supervisor_api"] = api;
        c16a_error(frame, &expected);
    }
    let mut frame = c16a_frame(maintenance_request());
    frame.value["params"]["supervisor_api"] = json!(2);
    frame.value["params"]["extra"] = json!(true);
    c16a_error(frame, "unexpected control field");
    let mut frame = c16a_frame(maintenance_request());
    frame.value["params"]["supervisor_api"] = json!(2);
    c16a_error(frame, "incompatible supervisor API");
    for message in [maintenance_request(), startup(host(), None)] {
        let mut frame = c16a_frame(message);
        frame.value["params"] = json!([]);
        c16a_error(frame, "expected control object");
    }
}
mod c16b1_original {
    use super::*;
    #[derive(Serialize, Deserialize)]
    #[serde(tag = "kind", deny_unknown_fields)]
    pub enum WireStdio {
        Fds { fds: [Descriptor; 3] },
        Tty { slave: Descriptor },
        NewPty { rows: u16, cols: u16 },
    }
    #[derive(Serialize, Deserialize)]
    pub struct Recreate {
        packages: Vec<String>,
        auto_approve: Vec<String>,
        docker: bool,
        variables: std::collections::BTreeMap<String, String>,
        #[serde(default)]
        worktree: Option<Worktree>,
    }
    #[derive(Serialize, Deserialize)]
    #[serde(deny_unknown_fields)]
    pub struct Worktree {
        name: String,
        path: PathBuf,
    }
    #[derive(Serialize, Deserialize)]
    #[serde(tag = "kind", deny_unknown_fields)]
    pub enum WireSource {
        Host {
            configuration: PathBuf,
            name: String,
            scope: Option<String>,
            dev_shell: Option<String>,
            workspace: Option<PathBuf>,
            cwd: Option<PathBuf>,
            state: PathBuf,
            history: Option<PathBuf>,
            recreate: Option<Recreate>,
            sequence: u64,
        },
        Member {
            scope: Option<Descriptor>,
            owner_scope: Option<String>,
            manifest: PathBuf,
            configuration: String,
            auto: std::collections::BTreeMap<String, PathBuf>,
            state: PathBuf,
            history: Option<PathBuf>,
            #[serde(default)]
            worktree: Option<PathBuf>,
            #[serde(default)]
            arguments: Option<Vec<String>>,
        },
    }
}

#[test]
fn c16b1_stdio_original_variants_and_diagnostics_at_actual_codec() {
    let mut cases = vec![
        json!({"kind":"Fds","fds":[{"$fd":0},{"$fd":1},{"$fd":2}]}),
        json!({"kind":"Fds","fds":[[0],[1],[2]]}),
        json!({"kind":"Tty","slave":{"$fd":0}}),
        json!({"kind":"NewPty","rows":0,"cols":65535}),
        json!({"kind":"NewPty","rows":1.0,"cols":2}),
        json!({"kind":"NewPty","rows":65536,"cols":2}),
        json!({"kind":"NewPty","rows":1}),
        json!({"kind":"Tty"}),
        json!({"kind":"Tty","slave":null}),
        json!({"kind":"Fds","fds":[]}),
        json!({"kind":"Fds","fds":[{"$fd":0},{"$fd":1}]}),
        json!({"kind":"Fds","fds":[{"$fd":0},{"$fd":1},{"$fd":2},{"$fd":3}]}),
        json!({"kind":"unknown"}),
        json!({"kind":null}),
        json!({}),
        json!(null),
        json!([]),
    ];
    for value in cases.clone().into_iter().take(4) {
        let mut value = value;
        value["extra"] = json!(true);
        cases.push(value);
    }
    for input in cases {
        let original = serde_json::from_value::<c16b1_original::WireStdio>(input.clone());
        let count = match input["kind"].as_str() {
            Some("Fds") => 3,
            Some("Tty") => 1,
            _ => 0,
        };
        let fds = (0..count)
            .map(|_| fs::File::open("/dev/null").unwrap().into())
            .collect();
        let mut descriptors = Descriptors::new(fds).unwrap();
        match original {
            Err(expected) => {
                let error = decode_stdio(input.clone(), &mut descriptors).err().unwrap();
                assert_eq!(error.kind(), io::ErrorKind::InvalidData);
                assert_eq!(error.to_string(), expected.to_string(), "input {input}");
            }
            Ok(original) => {
                let actual = decode_stdio(input.clone(), &mut descriptors).unwrap();
                descriptors.finish().unwrap();
                let mut builder = Builder::new("test", 1);
                assert_eq!(
                    encode_stdio(&actual, &mut builder).unwrap(),
                    serde_json::to_value(original).unwrap(),
                    "input {input}"
                );
                assert_eq!(builder.fds.len(), count);
            }
        }
    }
}

fn c16b1_member() -> Source {
    Source::Member {
        scope: None,
        owner_scope: None,
        manifest: "relative/manifest".into(),
        configuration: "child".into(),
        auto: [("hello".into(), "/output".into())].into(),
        state: "/state".into(),
        history: None,
        worktree: None,
        arguments: None,
    }
}

#[test]
fn c16b1_source_original_variants_defaults_and_exact_errors_at_decoder() {
    let root = unix::temp_directory().unwrap();
    let mut builder = Builder::new("test", 1);
    let host = encode_source(&host(), &mut builder).unwrap();
    let member = encode_source(&c16b1_member(), &mut builder).unwrap();
    assert_eq!(
        member,
        json!({"kind":"Member","scope":null,"owner_scope":null,
        "manifest":"relative/manifest","configuration":"child","auto":{"hello":"/output"},
        "state":"/state","history":null,"worktree":null,"arguments":null})
    );
    let mut cases = vec![
        host.clone(),
        member.clone(),
        json!(null),
        json!([]),
        json!({}),
        json!({"kind":"unknown"}),
    ];
    for (base, required, optional) in [
        (
            &host,
            vec!["configuration", "name", "state", "sequence"],
            vec![
                "scope",
                "dev_shell",
                "workspace",
                "cwd",
                "history",
                "recreate",
            ],
        ),
        (
            &member,
            vec!["manifest", "configuration", "auto", "state"],
            vec!["scope", "owner_scope", "history", "worktree", "arguments"],
        ),
    ] {
        for field in required {
            let mut v = base.clone();
            v.as_object_mut().unwrap().remove(field);
            cases.push(v);
            let mut v = base.clone();
            v[field] = json!(null);
            cases.push(v);
        }
        let mut v = base.clone();
        for field in optional {
            v.as_object_mut().unwrap().remove(field);
        }
        cases.push(v);
        let mut v = base.clone();
        v["extra"] = json!(true);
        cases.push(v);
    }
    for (field, value) in [
        ("sequence", json!(1.0)),
        ("sequence", json!(-1)),
        ("sequence", json!(u64::MAX)),
        ("configuration", json!(0)),
    ] {
        let mut v = host.clone();
        v[field] = value;
        cases.push(v);
    }
    for (field, value) in [
        ("auto", json!([])),
        ("auto", json!({"hello":null})),
        ("arguments", json!([1])),
        ("arguments", json!([])),
        ("worktree", json!("relative/雪")),
        ("scope", json!({"$fd":1.0})),
    ] {
        let mut v = member.clone();
        v[field] = value;
        cases.push(v);
    }
    for input in cases {
        let expected = serde_json::from_value::<c16b1_original::WireSource>(input.clone());
        let mut frame = if input["kind"] == "Member" {
            c16a_frame(startup(c16b1_member(), Some(handoff(&root, 0))))
        } else {
            c16a_frame(startup(self::host(), None))
        };
        frame.value["params"]["source"] = input.clone();
        match expected {
            Err(error) => c16a_error(frame, &error.to_string()),
            Ok(expected) => {
                let received = Decoder::default().receive(frame).unwrap().unwrap();
                assert_eq!(
                    received.adopted,
                    if input["kind"] == "Member" {
                        Some(1)
                    } else {
                        None
                    }
                );
                let Message::SupervisorStart(start) = received.message else {
                    unreachable!()
                };
                let mut builder = Builder::new("test", 1);
                assert_eq!(
                    encode_source(&start.source, &mut builder).unwrap(),
                    serde_json::to_value(expected).unwrap(),
                    "input {input}"
                );
            }
        }
    }
}

#[test]
fn c16b1_recreate_worktree_domain_conversion_defaults_additive_and_tuple_errors() {
    let base = json!({"packages":["hello"],"auto_approve":["jq"],"docker":true,"variables":{"session":"abc"}});
    let mut cases = vec![
        base.clone(),
        json!([[], [], false, {}]),
        json!([[], [], false, {}, null]),
        json!([[], [], false, {}, ["branch", "relative/tree"]]),
    ];
    for (field, value) in [
        ("worktree", json!(null)),
        ("worktree", json!({"name":"branch","path":"relative/雪"})),
        ("worktree", json!({"name":"branch","path":"x","extra":true})),
        ("worktree", json!({"name":"branch"})),
        ("worktree", json!(["branch"])),
        ("worktree", json!(["branch", "x", "extra"])),
        ("worktree", json!(false)),
        ("extra", json!({"$fd":999})),
        ("packages", json!([1])),
        ("variables", json!({"x":null})),
    ] {
        let mut v = base.clone();
        v[field] = value;
        cases.push(v);
    }
    for field in ["packages", "auto_approve", "docker", "variables"] {
        let mut v = base.clone();
        v.as_object_mut().unwrap().remove(field);
        cases.push(v);
    }
    for input in cases {
        let mut frame = c16a_frame(startup(host(), None));
        frame.value["params"]["source"]["recreate"] = input.clone();
        let expected = serde_json::from_value::<c16b1_original::WireSource>(
            frame.value["params"]["source"].clone(),
        )
        .map(|v| serde_json::to_value(v).unwrap()["recreate"].clone())
        .map_err(|e| e.to_string());
        match expected {
            Err(error) => c16a_error(frame, &error),
            Ok(expected) => {
                let received = Decoder::default().receive(frame).unwrap().unwrap();
                let Message::SupervisorStart(start) = received.message else {
                    unreachable!()
                };
                let mut builder = Builder::new("test", 1);
                assert_eq!(
                    encode_source(&start.source, &mut builder).unwrap()["recreate"],
                    expected,
                    "input {input}"
                );
            }
        }
    }
}

#[test]
fn c16b1_source_scope_precedes_other_capabilities_and_rejection_closes_copies() {
    let _no_forks = unix::no_forks();
    let root = unix::temp_directory().unwrap();
    for invalid in [false, true] {
        let (reader, writer) = unix::pipe().unwrap();
        let mut reader = fs::File::from(reader);
        let mut member = c16b1_member();
        let Source::Member { scope, .. } = &mut member else {
            unreachable!()
        };
        *scope = Some(writer.into());
        let Message::SupervisorStart(mut start) = startup(member, Some(handoff(&root, 0))) else {
            unreachable!()
        };
        start.legacy_listener = Some(null());
        let mut prepared = Encoder::default()
            .prepare(Message::SupervisorStart(start))
            .unwrap();
        assert!(prepared.needs_adoption());
        assert_eq!(
            prepared.frames[0].value["params"]["source"]["scope"],
            json!({"$fd":0})
        );
        assert_eq!(
            prepared.frames[0].value["params"]["legacy_listener"],
            json!({"$fd":1})
        );
        assert_eq!(
            prepared.frames[0].value["params"]["handoff"]["workspace"],
            json!({"$fd":2})
        );
        assert_eq!(
            prepared.frames[0].value["params"]["handoff"]["working_directory"]["fd"],
            json!({"$fd":3})
        );
        let mut frame = prepared.frames.pop_front().unwrap();
        if invalid {
            frame.value["params"]["source"]["scope"] = json!({"$fd":99});
            frame.value["params"]["legacy_listener"] = json!({});
            c16a_error(frame, "invalid or duplicate descriptor reference");
        } else {
            let received = Decoder::default().receive(frame).unwrap().unwrap();
            assert_eq!(received.adopted, Some(1));
            drop(received);
        }
        assert!(!unix::readable(reader.as_raw_fd(), 0).unwrap());
        drop(prepared);
        assert!(unix::readable(reader.as_raw_fd(), 100).unwrap());
        assert_eq!(reader.read(&mut [0]).unwrap(), 0);
    }
}
mod c16b2_original {
    use super::*;
    #[derive(Serialize, Deserialize)]
    #[serde(deny_unknown_fields)]
    pub struct WireLease {
        pub directory: PathBuf,
        pub lock: Descriptor,
    }
    #[derive(Serialize, Deserialize)]
    #[serde(deny_unknown_fields)]
    pub struct WireDirectory {
        pub fd: Descriptor,
        pub source: PathBuf,
        pub destination: PathBuf,
    }
    #[derive(Serialize, Deserialize)]
    #[serde(deny_unknown_fields)]
    pub struct WireHandoff {
        pub transfer: u64,
        pub count: usize,
        pub workspace: Descriptor,
        pub working_directory: Option<WireDirectory>,
        pub dev_shell: String,
        pub docker: bool,
        pub leases: Vec<WireLease>,
        pub more: bool,
    }
}
fn c16b2_start(root: &Path, count: usize) -> Prepared {
    Encoder::default()
        .prepare(Message::Command(Command::Start(Box::new(handoff(
            root, count,
        )))))
        .unwrap()
}
fn c16b2_decoder_error(decoder: &mut Decoder, frame: Frame, expected: &str) {
    let error = decoder
        .receive(frame)
        .err()
        .expect("reject malformed handoff");
    assert_eq!(error.kind(), io::ErrorKind::InvalidData);
    assert_eq!(error.to_string(), expected);
    decoder.finish().unwrap(); // A rejected frame must settle pending ownership.
}

#[test]
fn c16b2_handoff_records_original_serde_names_order_optional_and_sequence_forms() {
    let root = unix::temp_directory().unwrap();
    let base = c16b2_start(&root, 0).frames[0].value["params"]["handoff"].clone();
    assert_eq!(
        base,
        json!({"transfer":1,"count":0,"workspace":{"$fd":0},
        "working_directory":{"fd":{"$fd":1},"source":root,"destination":"/workspace"},
        "dev_shell":"null","docker":true,"leases":[],"more":false})
    );
    let mut cases = vec![
        base.clone(),
        json!([1,0,{"$fd":0},null,"null",true,[],false]),
        json!([]),
        json!(null),
    ];
    for field in [
        "transfer",
        "count",
        "workspace",
        "dev_shell",
        "docker",
        "leases",
        "more",
    ] {
        let mut v = base.clone();
        v.as_object_mut().unwrap().remove(field);
        cases.push(v);
        let mut v = base.clone();
        v[field] = json!(null);
        cases.push(v);
    }
    let mut v = base.clone();
    v.as_object_mut().unwrap().remove("working_directory");
    cases.push(v);
    for (field, value) in [
        ("transfer", json!(1.0)),
        ("count", json!(1.0)),
        ("count", json!(-1)),
        ("workspace", json!({})),
        ("dev_shell", json!({"$fd":999})),
        ("extra", json!(true)),
        ("working_directory", json!([{"$fd":1},root,"/workspace"])),
    ] {
        let mut v = base.clone();
        v[field] = value;
        cases.push(v);
    }
    let directory = base["working_directory"].clone();
    for field in ["fd", "source", "destination"] {
        let mut v = base.clone();
        v["working_directory"]
            .as_object_mut()
            .unwrap()
            .remove(field);
        cases.push(v);
    }
    for (field, value) in [
        ("extra", json!(true)),
        ("source", json!(null)),
        ("fd", json!({"$fd":1.0})),
    ] {
        let mut d = directory.clone();
        d[field] = value;
        let mut v = base.clone();
        v["working_directory"] = d;
        cases.push(v);
    }
    for lease in [
        json!({"directory":"/unused"}),
        json!({"lock":{"$fd":0}}),
        json!({"directory":null,"lock":{"$fd":0}}),
        json!({"directory":"/unused","lock":{"$fd":0},"extra":true}),
        json!(["/unused"]),
    ] {
        let mut v = base.clone();
        v["leases"] = json!([lease]);
        cases.push(v);
    }
    for value in cases {
        let expected = serde_json::from_value::<c16b2_original::WireHandoff>(value.clone());
        let mut prepared = c16b2_start(&root, 0);
        let mut frame = prepared.frames.pop_front().unwrap();
        frame.value["params"]["handoff"] = value.clone();
        match expected {
            Err(error) => c16a_error(frame, &error.to_string()),
            Ok(expected) => {
                if expected.working_directory.is_none() {
                    frame.fds.pop();
                }
                let received = Decoder::default().receive(frame).unwrap().unwrap();
                assert_eq!(received.adopted, Some(1));
                let repeated = Encoder::default().prepare(received.message).unwrap();
                assert_eq!(
                    repeated.frames[0].value["params"]["handoff"],
                    serde_json::to_value(expected).unwrap(),
                    "input {value}"
                );
            }
        }
    }
}

#[test]
fn c16b2_multipart_producer_pins_header_and_continuation_indexes_and_adoption() {
    let _no_forks = unix::no_forks();
    let root = unix::temp_directory().unwrap();
    let count = channel::MAX_FDS + 1;
    let first = channel::MAX_FDS - 2;
    let mut prepared = c16b2_start(&root, count);
    assert_eq!(prepared.transfer(), 1);
    assert!(prepared.needs_adoption());
    assert_eq!(prepared.frames.len(), 2);
    let first_leases: Vec<_> = (0..first)
        .map(|i| json!({"directory":root.join(format!("r{i}")),"lock":{"$fd":i+2}}))
        .collect();
    let rest: Vec<_> = (first..count)
        .enumerate()
        .map(|(index, i)| json!({"directory":root.join(format!("r{i}")),"lock":{"$fd":index}}))
        .collect();
    assert_eq!(
        prepared.frames[0].value,
        json!({"jsonrpc":"2.0","method":"Command.Start","params":{"handoff":{
        "transfer":1,"count":count,"workspace":{"$fd":0},"working_directory":{"fd":{"$fd":1},"source":root,"destination":"/workspace"},
        "dev_shell":"null","docker":true,"leases":first_leases,"more":true}}})
    );
    assert_eq!(
        prepared.frames[1].value,
        json!({"jsonrpc":"2.0","method":"Leases","params":{"transfer":1,"leases":rest,"more":false}})
    );
    assert_eq!(prepared.frames[0].fds.len(), channel::MAX_FDS);
    assert_eq!(prepared.frames[1].fds.len(), 3);
    let mut decoder = Decoder::default();
    assert!(
        decoder
            .receive(prepared.frames.pop_front().unwrap())
            .unwrap()
            .is_none()
    );
    let received = decoder
        .receive(prepared.frames.pop_front().unwrap())
        .unwrap()
        .unwrap();
    assert_eq!(received.adopted, Some(1));
    prepared.adopted(1).unwrap();
    assert!(root.join("r0").exists());
    drop(received);
    assert!(!root.join("r0").exists());
    assert!(!root.join(format!("r{}", count - 1)).exists());
    let mut encoder = Encoder { next: u64::MAX - 1 };
    let mut exported = encoder
        .prepare(Message::Event(Event::Exported {
            member: "child".into(),
            result: Ok(Box::new(handoff(&root, 0))),
        }))
        .unwrap();
    assert_eq!(exported.frames[0].value["method"], "Event.Exported");
    assert_eq!(
        exported.frames[0].value["params"]["result"]["Ok"]["transfer"],
        json!(u64::MAX)
    );
    assert_eq!(
        decode(&mut exported, &mut Decoder::default()).adopted,
        Some(u64::MAX)
    );
    assert_eq!(
        encoder
            .prepare(Message::Stopping)
            .err()
            .unwrap()
            .to_string(),
        "transfer id exhausted"
    );
}

#[test]
fn c16b2_handoff_counts_and_opaque_generation_keep_semantic_error_order() {
    let root = unix::temp_directory().unwrap();
    for (transfer, count, more, expected) in [
        (0, 0, false, Some("invalid handoff descriptor count")),
        (1, 0, true, Some("invalid handoff descriptor count")),
        (1, 1, false, Some("invalid handoff descriptor count")),
        (
            1,
            usize::MAX,
            true,
            Some("invalid handoff descriptor count"),
        ),
        (u64::MAX, 0, false, None),
        (1, channel::MAX_BODY, true, None),
    ] {
        let mut prepared = c16b2_start(&root, 0);
        let mut frame = prepared.frames.pop_front().unwrap();
        let h = &mut frame.value["params"]["handoff"];
        h["transfer"] = json!(transfer);
        h["count"] = json!(count);
        h["more"] = json!(more);
        let mut decoder = Decoder::default();
        if let Some(error) = expected {
            frame.value["params"]["handoff"]["workspace"] = json!({"$fd":99});
            c16b2_decoder_error(&mut decoder, frame, error);
        } else if more {
            assert!(decoder.receive(frame).unwrap().is_none());
            assert_eq!(
                decoder.finish().err().unwrap().to_string(),
                "EOF during handoff transfer"
            );
        } else {
            assert_eq!(
                decoder.receive(frame).unwrap().unwrap().adopted,
                Some(transfer)
            );
        }
    }
    let mut frame = c16b2_start(&root, 0).frames.pop_front().unwrap();
    frame.value["params"]["handoff"]["dev_shell"] = json!("{");
    let expected = io::Error::from(
        serde_json::from_str::<Option<crate::devshell::DevShell>>("{").unwrap_err(),
    );
    let error = Decoder::default().receive(frame).err().unwrap();
    assert_eq!(error.kind(), expected.kind());
    assert_eq!(error.to_string(), expected.to_string());
    let mut frame = c16b2_start(&root, 0).frames.pop_front().unwrap();
    frame.value["params"]["handoff"]["workspace"] = json!({"$fd":99});
    frame.value["params"]["handoff"]["dev_shell"] = json!("{");
    c16a_error(frame, "invalid or duplicate descriptor reference");
    let generation = json!({"reference":"/flake#dev","path":"/flake","attr":"dev","generation":1,
        "source":"/source","flake":"/source/sub","lock":{"$fd":999,"nested":[{"$fd":0}]},
        "profile":"/profile","variables":[["NAME","value"]],"script":"export NAME=value",
        "summary":{"packages":{},"env":{},"derivation":"/drv"}});
    let mut h = handoff(&root, 0);
    h.dev_shell = Some(serde_json::from_value(generation.clone()).unwrap());
    let mut prepared = Encoder::default()
        .prepare(Message::Command(Command::Start(Box::new(h))))
        .unwrap();
    let mut frame = prepared.frames.pop_front().unwrap();
    let opaque = frame.value["params"]["handoff"]["dev_shell"]
        .as_str()
        .unwrap();
    assert_eq!(serde_json::from_str::<Value>(opaque).unwrap(), generation);
    let (left, right) = std::os::unix::net::UnixStream::pair().unwrap();
    let mut sender = channel::Channel::new(left).unwrap();
    let mut receiver = channel::Channel::new(right).unwrap();
    sender.queue(&frame.value, &mut frame.fds).unwrap();
    sender.flush().unwrap();
    assert!(!sender.wants_write());
    let received = Decoder::default()
        .receive_channel(&mut receiver)
        .unwrap()
        .unwrap();
    assert_eq!(received.adopted, Some(1));
    let Message::Command(Command::Start(h)) = received.message else {
        unreachable!()
    };
    assert_eq!(serde_json::to_value(h.dev_shell).unwrap(), generation);
}

fn c16b2_partial(root: &Path) -> (Prepared, Decoder, Frame) {
    let mut prepared = c16b2_start(root, 1);
    let mut header = prepared.frames.pop_front().unwrap();
    let lease = header.value["params"]["handoff"]["leases"][0].clone();
    let lock = header.fds.pop().unwrap();
    header.value["params"]["handoff"]["leases"] = json!([]);
    header.value["params"]["handoff"]["more"] = json!(true);
    let mut decoder = Decoder::default();
    assert!(decoder.receive(header).unwrap().is_none());
    let mut lease = lease;
    lease["lock"] = json!({"$fd":0});
    let continuation = Frame {
        value: json!({"jsonrpc":"2.0","method":"Leases","params":{"transfer":1,"leases":[lease],"more":false}}),
        fds: vec![lock],
    };
    (prepared, decoder, continuation)
}

#[test]
fn c16b2_continuation_field_errors_precede_pending_state_and_unknown_fields() {
    for (params, expected) in [
        (json!({}), "missing control field transfer".into()),
        (json!({"transfer":1}), "missing control field leases".into()),
        (
            json!({"transfer":1,"leases":[]}),
            "missing control field more".into(),
        ),
        (
            json!({"transfer":1,"leases":[],"more":false,"extra":true}),
            "unexpected control field".into(),
        ),
        (
            json!({"transfer":1,"leases":[],"more":false}),
            "unexpected handoff batch".into(),
        ),
        (
            json!({"transfer":1.0,"leases":[],"more":false}),
            serde_json::from_value::<u64>(json!(1.0))
                .unwrap_err()
                .to_string(),
        ),
        (
            json!({"transfer":1,"leases":[{}],"more":false}),
            serde_json::from_value::<Vec<c16b2_original::WireLease>>(json!([{}]))
                .err()
                .unwrap()
                .to_string(),
        ),
    ] {
        c16a_error(
            Frame {
                value: json!({"jsonrpc":"2.0","method":"Leases","params":params}),
                fds: vec![],
            },
            &expected,
        );
    }
}

#[test]
fn c16b2_continuation_counts_overlap_and_cleanup_keep_exact_precedence() {
    let _no_forks = unix::no_forks();
    for fault in 0..7 {
        let root = unix::temp_directory().unwrap();
        let (prepared, mut decoder, mut frame) = c16b2_partial(&root);
        let expected = match fault {
            0 => {
                frame.value["params"]["transfer"] = json!(2);
                "invalid handoff batch"
            }
            1 => {
                frame.value["params"]["leases"] = json!([]);
                "invalid handoff batch"
            }
            2 => {
                let lease = frame.value["params"]["leases"][0].clone();
                frame.value["params"]["leases"] = json!([lease.clone(), lease]);
                "handoff exceeds declared budget"
            }
            3 => {
                frame.value["params"]["more"] = json!(true);
                "invalid handoff continuation"
            }
            4 => {
                frame.value["params"]["leases"][0]["lock"] = json!({"$fd":1});
                "invalid or duplicate descriptor reference"
            }
            5 => {
                frame.value =
                    json!({"jsonrpc":"2.0","method":"Command.Start","params":{"handoff":null}});
                "overlapping handoff transfers"
            }
            6 => {
                frame.value["params"]["extra"] = json!(true);
                "unexpected control field"
            }
            _ => unreachable!(),
        };
        c16b2_decoder_error(&mut decoder, frame, expected);
        assert!(root.join("r0").exists());
        drop(prepared);
        assert!(!root.join("r0").exists());
    }
    // Legacy positional WireLease is accepted by the original typed decoder.
    let root = unix::temp_directory().unwrap();
    let (mut prepared, mut decoder, mut frame) = c16b2_partial(&root);
    frame.value["params"]["leases"] = json!([[root.join("r0"),{"$fd":0}]]);
    let received = decoder.receive(frame).unwrap().unwrap();
    assert_eq!(received.adopted, Some(1));
    prepared.adopted(1).unwrap();
    assert!(root.join("r0").exists());
    drop(received);
    assert!(!root.join("r0").exists());
}
fn c17a_unrooted() -> Message {
    Message::Event(Event::Manifest {
        sequence: u64::MAX,
        lease: ManifestLease::adopt("relative/manifest".into(), None),
        catalogue: fd_named_catalogue(),
    })
}
#[test]
fn c17a_manifest_and_export_failure_pin_exact_envelopes_and_opaque_transport() {
    let _no_forks = unix::no_forks();
    let catalogue = "{\"goblins\":{\"$fd\":{\"description\":\"\",\"args\":[],\"scope_args\":{\"$fd\":[]},\"launch_options\":{}}}}";
    for rooted in [false, true] {
        let root = unix::temp_directory().unwrap();
        let mut encoder = Encoder { next: u64::MAX - 1 };
        let mut prepared = encoder
            .prepare(if rooted {
                manifest(&root)
            } else {
                c17a_unrooted()
            })
            .unwrap();
        assert!(prepared.needs_adoption());
        assert_eq!(prepared.transfer(), u64::MAX);
        assert_eq!(prepared.frames.len(), 1);
        let mut frame = prepared.frames.pop_front().unwrap();
        assert_eq!(
            frame.value,
            json!({"jsonrpc":"2.0","method":"Event.Manifest","params":{
            "sequence":if rooted {17} else {u64::MAX},"path":if rooted {root.join("manifest.json")} else {PathBuf::from("relative/manifest")},
            "root":if rooted {Some(root.join("gc-root"))} else {None},"catalogue":catalogue,
            "owner":if rooted {json!({"$fd":0})} else {json!(null)},"transfer":u64::MAX}})
        );
        assert_eq!(frame.fds.len(), usize::from(rooted));
        let (left, right) = std::os::unix::net::UnixStream::pair().unwrap();
        let mut sender = channel::Channel::new(left).unwrap();
        let mut receiver = channel::Channel::new(right).unwrap();
        sender.queue(&frame.value, &mut frame.fds).unwrap();
        sender.flush().unwrap();
        assert!(!sender.wants_write());
        let received = Decoder::default()
            .receive_channel(&mut receiver)
            .unwrap()
            .unwrap();
        assert_eq!(received.adopted, Some(u64::MAX));
        let Message::Event(Event::Manifest {
            sequence,
            lease,
            catalogue,
        }) = &received.message
        else {
            unreachable!()
        };
        assert_eq!(*sequence, if rooted { 17 } else { u64::MAX });
        assert_eq!(catalogue, &fd_named_catalogue());
        assert_eq!(
            lease.path,
            if rooted {
                root.join("manifest.json")
            } else {
                PathBuf::from("relative/manifest")
            }
        );
        assert_eq!(lease.root().is_some(), rooted);
        prepared.adopted(u64::MAX).unwrap();
        if rooted {
            assert!(root.join("gc-root").exists());
        }
        drop(received);
        assert!(!root.join("gc-root").exists());
        fs::remove_dir_all(root).unwrap();
    }
    let mut prepared = Encoder::default()
        .prepare(Message::Event(Event::Exported {
            member: "".into(),
            result: Err("failed\n雪".into()),
        }))
        .unwrap();
    assert_eq!(
        prepared.frames[0].value,
        json!({"jsonrpc":"2.0","method":"Event.Exported","params":{"member":"","result":{"Err":"failed\n雪"}}})
    );
    assert!(prepared.frames[0].fds.is_empty());
    assert!(!prepared.needs_adoption());
    let received = decode(&mut prepared, &mut Decoder::default());
    assert!(received.adopted.is_none());
    assert!(
        matches!(received.message,Message::Event(Event::Exported {member,result:Err(error)}) if member.is_empty() && error=="failed\n雪")
    );
}

#[test]
fn c17a_root_params_missing_unknown_and_type_errors_keep_original_order() {
    let root = unix::temp_directory().unwrap();
    let cases = vec![
        (c16b2_start(&root, 0), vec!["handoff"]),
        (
            Encoder::default()
                .prepare(Message::Event(Event::Exported {
                    member: "child".into(),
                    result: Err("failed".into()),
                }))
                .unwrap(),
            vec!["member", "result"],
        ),
        (
            Encoder::default().prepare(c17a_unrooted()).unwrap(),
            vec!["sequence", "path", "root", "catalogue", "owner", "transfer"],
        ),
    ];
    for (mut prepared, fields) in cases {
        let original = prepared.frames.pop_front().unwrap();
        for field in fields {
            let mut value = original.value.clone();
            value["params"].as_object_mut().unwrap().remove(field);
            c16a_error(
                Frame {
                    value,
                    fds: original
                        .fds
                        .iter()
                        .map(|fd| fd.try_clone().unwrap())
                        .collect(),
                },
                &format!("missing control field {field}"),
            );
        }
        let mut value = original.value.clone();
        value["params"]["extra"] = json!(true);
        c16a_error(
            Frame {
                value,
                fds: original.fds,
            },
            "unexpected control field",
        );
    }
    let mut frame = c16b2_start(&root, 0).frames.pop_front().unwrap();
    frame.value["params"]["handoff"] = json!(null);
    frame.value["params"]["extra"] = json!(true);
    c16a_error(frame, "unexpected control field");
    let mut frame = c16b2_start(&root, 0).frames.pop_front().unwrap();
    frame.value["params"]["handoff"] = json!(null);
    let expected = serde_json::from_value::<c16b2_original::WireHandoff>(json!(null))
        .err()
        .unwrap()
        .to_string();
    c16a_error(frame, &expected);
    for value in [
        json!(null),
        json!({}),
        json!({"Err":null}),
        json!({"Other":null}),
        json!({"Ok":{},"Err":"x"}),
    ] {
        let mut frame = c16a_frame(Message::Event(Event::Exported {
            member: "child".into(),
            result: Err("x".into()),
        }));
        frame.value["params"]["result"] = value.clone();
        let expected = serde_json::from_value::<Result<Value, String>>(value)
            .err()
            .unwrap()
            .to_string();
        c16a_error(frame, &expected);
    }
    let mut frame = c16a_frame(Message::Event(Event::Exported {
        member: "child".into(),
        result: Err("x".into()),
    }));
    frame.value["params"]["member"] = json!(null);
    frame.value["params"]["result"] = json!(null);
    c16a_error(
        frame,
        &serde_json::from_value::<String>(json!(null))
            .unwrap_err()
            .to_string(),
    );
    let mut frame = c16a_frame(Message::Event(Event::Exported {
        member: "child".into(),
        result: Err("x".into()),
    }));
    frame.value["params"]["result"] = json!({"Ok":null});
    frame.value["params"]["extra"] = json!(true);
    c16a_error(frame, "unexpected control field");
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn c17a_manifest_scalar_and_inner_catalogue_decoding_preserve_exact_errors_and_legacy_forms() {
    for (field, value, expected) in [
        (
            "sequence",
            json!(1.0),
            serde_json::from_value::<u64>(json!(1.0))
                .unwrap_err()
                .to_string(),
        ),
        (
            "sequence",
            json!(-1),
            serde_json::from_value::<u64>(json!(-1))
                .unwrap_err()
                .to_string(),
        ),
        (
            "path",
            json!(null),
            serde_json::from_value::<PathBuf>(json!(null))
                .unwrap_err()
                .to_string(),
        ),
        (
            "root",
            json!([]),
            serde_json::from_value::<Option<PathBuf>>(json!([]))
                .unwrap_err()
                .to_string(),
        ),
        (
            "catalogue",
            json!({}),
            serde_json::from_value::<String>(json!({}))
                .unwrap_err()
                .to_string(),
        ),
        (
            "owner",
            json!({"$fd":1.0}),
            serde_json::from_value::<Option<c16a2_original::Descriptor>>(json!({"$fd":1.0}))
                .unwrap_err()
                .to_string(),
        ),
        (
            "transfer",
            json!(1.0),
            serde_json::from_value::<u64>(json!(1.0))
                .unwrap_err()
                .to_string(),
        ),
    ] {
        let mut frame = c16a_frame(c17a_unrooted());
        frame.value["params"][field] = value;
        c16a_error(frame, &expected);
    }
    for opaque in [
        "{",
        "null",
        "[]",
        "{}",
        "{\"goblins\":null}",
        "{\"goblins\":{},\"extra\":true}",
        "{\"goblins\":{}}",
        "[{}]",
    ] {
        let expected = serde_json::from_str::<crate::config::Catalogue>(opaque);
        let mut frame = c16a_frame(c17a_unrooted());
        frame.value["params"]["catalogue"] = json!(opaque);
        match expected {
            Err(error) => c16a_error(frame, &error.to_string()),
            Ok(expected) => {
                let received = Decoder::default().receive(frame).unwrap().unwrap();
                let Message::Event(Event::Manifest { catalogue, .. }) = received.message else {
                    unreachable!()
                };
                assert_eq!(catalogue, expected);
            }
        }
    }
}

#[test]
fn c17a_manifest_owner_root_transfer_and_catalogue_error_precedence_closes_raw_fds() {
    let _no_forks = unix::no_forks();
    for fault in 0..5 {
        let (reader, writer) = unix::pipe().unwrap();
        let mut reader = fs::File::from(reader);
        let mut frame = c16a_frame(c17a_unrooted());
        frame.fds.push(writer);
        frame.value["params"]["owner"] = json!({"$fd":0});
        let expected = match fault {
            0 => "manifest root ownership descriptor missing or unexpected".into(),
            1 => {
                frame.value["params"]["transfer"] = json!(0);
                "invalid ownership transfer id".into()
            }
            2 => {
                frame.value["params"]["transfer"] = json!(0);
                frame.value["params"]["extra"] = json!(true);
                "unexpected control field".into()
            }
            3 => {
                frame.value["params"]["owner"] = json!({"$fd":99});
                frame.value["params"]["transfer"] = json!(1.0);
                "invalid or duplicate descriptor reference".into()
            }
            4 => {
                frame.value["params"]["owner"] = json!({"$fd":99});
                frame.value["params"]["catalogue"] = json!("{");
                serde_json::from_str::<crate::config::Catalogue>("{")
                    .unwrap_err()
                    .to_string()
            }
            _ => unreachable!(),
        };
        c16a_error(frame, &expected);
        assert!(unix::readable(reader.as_raw_fd(), 100).unwrap());
        assert_eq!(reader.read(&mut [0]).unwrap(), 0);
    }
    let mut frame = c16a_frame(c17a_unrooted());
    frame.value["params"]["root"] = json!("/root-without-owner");
    c16a_error(
        frame,
        "manifest root ownership descriptor missing or unexpected",
    );
    for wrong_root in [false, true] {
        let root = unix::temp_directory().unwrap();
        let unrelated = root.join("unrelated-root");
        fs::write(&unrelated, "keep").unwrap();
        fs::write(root.join("unrelated-root.ownership"), "").unwrap();
        let mut prepared = Encoder::default().prepare(manifest(&root)).unwrap();
        let mut frame = prepared.frames.pop_front().unwrap();
        if wrong_root {
            frame.value["params"]["root"] = json!(unrelated);
        } else {
            frame.fds[0] = fs::File::open("/dev/null").unwrap().into();
        }
        c16a_error(frame, "invalid manifest ownership descriptor");
        assert!(root.join("gc-root").exists());
        drop(prepared);
        assert!(!root.join("gc-root").exists());
        assert_eq!(fs::read_to_string(&unrelated).unwrap(), "keep");
        fs::remove_dir_all(root).unwrap();
    }
}
mod c17b1_original {
    use super::*;
    #[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
    pub enum SpawnKind {
        Exec,
        Attach,
        Terminal { exit_report: Option<u64> },
    }
}
#[test]
fn c17b1_spawn_kind_original_raw_value_forms_traits_and_exact_diagnostics() {
    fn required<T: Copy + Clone + std::fmt::Debug + Eq>() {}
    required::<crate::SpawnKind>();
    let cases = [
        "\"Exec\"",
        "\"Attach\"",
        "{\"Exec\":null}",
        "{\"Attach\":null}",
        "{\"Terminal\":{}}",
        "{\"Terminal\":{\"exit_report\":null}}",
        "{\"Terminal\":{\"exit_report\":0}}",
        "{\"Terminal\":{\"exit_report\":18446744073709551615}}",
        "{\"Terminal\":{\"exit_report\":1.0}}",
        "{\"Terminal\":{\"exit_report\":-1}}",
        "{\"Terminal\":{\"exit_report\":18446744073709551616}}",
        "{\"Terminal\":{\"exit_report\":true}}",
        "{\"Terminal\":{\"exit_report\":\"1\"}}",
        "{\"Terminal\":{\"extra\":true}}",
        "{\"Terminal\":{\"exit_report\":1,\"extra\":[]}}",
        "{\"Terminal\":[]}",
        "{\"Terminal\":[null]}",
        "{\"Terminal\":[1]}",
        "{\"Terminal\":[1,2]}",
        "{\"Terminal\":null}",
        "\"Terminal\"",
        "{\"Exec\":{}}",
        "{\"Exec\":[]}",
        "{\"Terminal\":{\"exit_report\":1,\"exit_report\":2}}",
        "{\"Exec\":null,\"Attach\":null}",
        "\"exec\"",
        "\"Unknown\"",
        "{}",
        "null",
        "[]",
        "0",
        "true",
    ];
    for input in cases {
        let old = serde_json::from_str::<c17b1_original::SpawnKind>(input)
            .map(|v| (format!("{v:?}"), serde_json::to_string(&v).unwrap()))
            .map_err(|e| e.to_string());
        let actual = serde_json::from_str::<crate::SpawnKind>(input)
            .map(|v| (format!("{v:?}"), serde_json::to_string(&v).unwrap()))
            .map_err(|e| e.to_string());
        assert_eq!(actual, old, "raw {input}");
        let value: Value = serde_json::from_str(input).unwrap();
        let old = serde_json::from_value::<c17b1_original::SpawnKind>(value.clone())
            .map(|v| (format!("{v:?}"), serde_json::to_value(v).unwrap()))
            .map_err(|e| e.to_string());
        let actual = serde_json::from_value::<crate::SpawnKind>(value)
            .map(|v| (format!("{v:?}"), serde_json::to_value(v).unwrap()))
            .map_err(|e| e.to_string());
        assert_eq!(actual, old, "Value {input}");
    }
    let value = crate::SpawnKind::Terminal {
        exit_report: Some(u64::MAX),
    };
    assert_eq!(value, value.clone());
    assert_ne!(crate::SpawnKind::Exec, crate::SpawnKind::Attach);
    assert_eq!(
        serde_json::to_value(crate::SpawnKind::Terminal { exit_report: None }).unwrap(),
        json!({"Terminal":{"exit_report":null}})
    );
}

fn c17b1_spawn() -> Message {
    Message::Command(Command::Spawn {
        id: u64::MAX,
        kind: crate::SpawnKind::Terminal { exit_report: None },
        argv: None,
        cwd: None,
        env: vec![("NAME".into(), "雪\n".into())],
        stdio: Stdio::NewPty {
            rows: 0,
            cols: u16::MAX,
        },
    })
}
fn c17b1_process_cases() -> Vec<(Message, &'static str, Value, Vec<&'static str>)> {
    vec![
        (
            c17b1_spawn(),
            "Command.Spawn",
            json!({"id":u64::MAX,"kind":{"Terminal":{"exit_report":null}},"argv":null,"cwd":null,"env":[["NAME","雪\n"]],"stdio":{"kind":"NewPty","rows":0,"cols":65535}}),
            vec!["id", "kind", "argv", "cwd", "env", "stdio"],
        ),
        (
            Message::Command(Command::Signal {
                id: 0,
                signal: i32::MIN,
            }),
            "Command.Signal",
            json!({"id":0,"signal":i32::MIN}),
            vec!["id", "signal"],
        ),
        (
            Message::Command(Command::Signal {
                id: u64::MAX,
                signal: i32::MAX,
            }),
            "Command.Signal",
            json!({"id":u64::MAX,"signal":i32::MAX}),
            vec!["id", "signal"],
        ),
        (
            Message::Command(Command::CancelProcess {
                id: u64::MAX,
                everything: true,
            }),
            "Command.CancelProcess",
            json!({"id":u64::MAX,"everything":true}),
            vec!["id", "everything"],
        ),
        (
            Message::Command(Command::KillAll),
            "Command.KillAll",
            json!({}),
            vec![],
        ),
        (
            Message::Event(Event::Spawned {
                id: u64::MAX,
                pid: i32::MIN,
            }),
            "Event.Spawned",
            json!({"id":u64::MAX,"pid":i32::MIN}),
            vec!["id", "pid"],
        ),
        (
            Message::Event(Event::SpawnFailed {
                id: 0,
                code: None,
                message: "failed\n雪".into(),
            }),
            "Event.SpawnFailed",
            json!({"id":0,"code":null,"message":"failed\n雪"}),
            vec!["id", "code", "message"],
        ),
        (
            Message::Event(Event::SpawnFailed {
                id: u64::MAX,
                code: Some(i32::MAX),
                message: "".into(),
            }),
            "Event.SpawnFailed",
            json!({"id":u64::MAX,"code":i32::MAX,"message":""}),
            vec!["id", "code", "message"],
        ),
        (
            Message::Event(Event::Exited {
                id: u64::MAX,
                code: Some(i32::MIN),
                signal: Some(i32::MAX),
            }),
            "Event.Exited",
            json!({"id":u64::MAX,"code":i32::MIN,"signal":i32::MAX}),
            vec!["id", "code", "signal"],
        ),
        (
            Message::Event(Event::Exited {
                id: 0,
                code: None,
                signal: None,
            }),
            "Event.Exited",
            json!({"id":0,"code":null,"signal":null}),
            vec!["id", "code", "signal"],
        ),
    ]
}
#[test]
fn c17b1_process_producers_and_required_nullable_root_fields_are_exact() {
    for (message, method, params, fields) in c17b1_process_cases() {
        let mut prepared = Encoder::default().prepare(message).unwrap();
        assert!(!prepared.needs_adoption());
        assert_eq!(prepared.frames.len(), 1);
        let original = prepared.frames.pop_front().unwrap();
        assert!(original.fds.is_empty());
        assert_eq!(
            original.value,
            json!({"jsonrpc":"2.0","method":method,"params":params})
        );
        for field in &fields {
            let mut value = original.value.clone();
            value["params"].as_object_mut().unwrap().remove(*field);
            c16a_error(
                Frame { value, fds: vec![] },
                &format!("missing control field {field}"),
            );
        }
        if let Some(field) = fields.first() {
            let mut value = original.value.clone();
            value["params"] = json!({});
            c16a_error(
                Frame { value, fds: vec![] },
                &format!("missing control field {field}"),
            );
        }
        let mut value = original.value.clone();
        value["params"]["extra"] = json!(true);
        c16a_error(Frame { value, fds: vec![] }, "unexpected control field");
        let mut value = original.value.clone();
        value["params"] = json!([]);
        c16a_error(Frame { value, fds: vec![] }, "expected control object");
        let expected = original.value.clone();
        let received = Decoder::default().receive(original).unwrap().unwrap();
        assert!(received.adopted.is_none());
        assert_eq!(
            Encoder::default().prepare(received.message).unwrap().frames[0].value,
            expected
        );
    }
    for kind in [
        crate::SpawnKind::Exec,
        crate::SpawnKind::Attach,
        crate::SpawnKind::Terminal {
            exit_report: Some(u64::MAX),
        },
    ] {
        let frame = c16a_frame(Message::Command(Command::Spawn {
            id: 0,
            kind,
            argv: Some(vec![]),
            cwd: Some("".into()),
            env: vec![],
            stdio: Stdio::NewPty { rows: 1, cols: 1 },
        }));
        assert_eq!(frame.value["params"]["argv"], json!([]));
        assert_eq!(frame.value["params"]["cwd"], json!(""));
        assert_eq!(
            frame.value["params"]["kind"],
            serde_json::to_value(kind).unwrap()
        );
        assert!(Decoder::default().receive(frame).unwrap().is_some());
    }
}

#[test]
fn c17b1_process_scalar_ranges_and_field_error_order_match_original() {
    fn error<T: DeserializeOwned>(v: Value) -> String {
        serde_json::from_value::<T>(v).err().unwrap().to_string()
    }
    for (method, field, values, ty) in [
        (
            "Command.Signal",
            "id",
            vec![json!(-1), json!(1.0), json!(null)],
            "u64",
        ),
        (
            "Command.Signal",
            "signal",
            vec![
                json!(i32::MIN as i64 - 1),
                json!(i32::MAX as i64 + 1),
                json!(1.0),
                json!(null),
            ],
            "i32",
        ),
        (
            "Command.CancelProcess",
            "everything",
            vec![json!(0), json!(null)],
            "bool",
        ),
        (
            "Event.Spawned",
            "pid",
            vec![json!(i32::MAX as i64 + 1), json!(1.0)],
            "i32",
        ),
        (
            "Event.SpawnFailed",
            "code",
            vec![json!(i32::MIN as i64 - 1), json!(1.0), json!(false)],
            "Option<i32>",
        ),
        (
            "Event.Exited",
            "signal",
            vec![json!(i32::MAX as i64 + 1), json!(1.0)],
            "Option<i32>",
        ),
    ] {
        for v in values {
            let mut frame = c17b1_process_cases()
                .into_iter()
                .find(|(_, name, _, _)| *name == method)
                .map(|(m, _, _, _)| c16a_frame(m))
                .unwrap();
            let expected = match ty {
                "u64" => error::<u64>(v.clone()),
                "i32" => error::<i32>(v.clone()),
                "bool" => error::<bool>(v.clone()),
                "Option<i32>" => error::<Option<i32>>(v.clone()),
                _ => unreachable!(),
            };
            frame.value["params"][field] = v;
            c16a_error(frame, &expected);
        }
    }
    for (field, value, expected) in [
        ("argv", json!([1]), error::<Option<Vec<String>>>(json!([1]))),
        (
            "argv",
            json!(false),
            error::<Option<Vec<String>>>(json!(false)),
        ),
        ("cwd", json!(1), error::<Option<String>>(json!(1))),
        (
            "env",
            json!([["NAME"]]),
            error::<Vec<(String, String)>>(json!([["NAME"]])),
        ),
        (
            "env",
            json!([["NAME", null]]),
            error::<Vec<(String, String)>>(json!([["NAME", null]])),
        ),
        (
            "kind",
            json!({"Terminal":{"exit_report":1.0}}),
            error::<c17b1_original::SpawnKind>(json!({"Terminal":{"exit_report":1.0}})),
        ),
    ] {
        let mut frame = c16a_frame(c17b1_spawn());
        frame.value["params"][field] = value;
        frame.value["params"]["stdio"] = json!(null);
        c16a_error(frame, &expected);
    }
    let mut frame = c16a_frame(c17b1_spawn());
    frame.value["params"]["id"] = json!(1.0);
    frame.value["params"]["kind"] = json!("Unknown");
    c16a_error(frame, &error::<u64>(json!(1.0)));
}

#[test]
fn c17b1_spawn_fd_indexes_sender_lifetime_and_rejection_order_are_preserved() {
    let _no_forks = unix::no_forks();
    for fault in 0..6 {
        let (reader, writer) = unix::pipe().unwrap();
        let mut reader = fs::File::from(reader);
        let copies = [
            writer.try_clone().unwrap().into(),
            writer.try_clone().unwrap().into(),
            writer.into(),
        ];
        let mut prepared = Encoder::default()
            .prepare(Message::Command(Command::Spawn {
                id: 1,
                kind: crate::SpawnKind::Exec,
                argv: None,
                cwd: None,
                env: vec![],
                stdio: Stdio::Fds(copies),
            }))
            .unwrap();
        let mut frame = prepared.frames.pop_front().unwrap();
        assert_eq!(frame.fds.len(), 3);
        let unique: std::collections::BTreeSet<_> =
            frame.fds.iter().map(AsRawFd::as_raw_fd).collect();
        assert_eq!(unique.len(), 3);
        assert_eq!(
            frame.value["params"]["stdio"],
            json!({"kind":"Fds","fds":[{"$fd":0},{"$fd":1},{"$fd":2}]})
        );
        assert!(!prepared.needs_adoption());
        if fault == 0 {
            let (left, right) = std::os::unix::net::UnixStream::pair().unwrap();
            let mut sender = channel::Channel::new(left).unwrap();
            let mut receiver = channel::Channel::new(right).unwrap();
            sender.queue(&frame.value, &mut frame.fds).unwrap();
            sender.flush().unwrap();
            assert!(!sender.wants_write());
            let received = Decoder::default()
                .receive_channel(&mut receiver)
                .unwrap()
                .unwrap();
            assert!(received.adopted.is_none());
            drop(prepared);
            assert!(!unix::readable(reader.as_raw_fd(), 0).unwrap());
            drop(received);
        } else {
            frame.value["params"]["extra"] = json!(true);
            let expected = match fault {
                1 => {
                    frame.value["params"]["id"] = json!(1.0);
                    frame.value["params"]["stdio"]["fds"][1] = json!({"$fd":0});
                    serde_json::from_value::<u64>(json!(1.0))
                        .unwrap_err()
                        .to_string()
                }
                2 => {
                    frame.value["params"]["argv"] = json!([1]);
                    frame.value["params"]["stdio"]["fds"][1] = json!({"$fd":0});
                    serde_json::from_value::<Option<Vec<String>>>(json!([1]))
                        .unwrap_err()
                        .to_string()
                }
                3 => {
                    frame.value["params"]["stdio"]["fds"][1] = json!({"$fd":0});
                    "invalid or duplicate descriptor reference".into()
                }
                4 => {
                    frame.value["params"]["stdio"] = json!({"kind":"Unknown"});
                    serde_json::from_value::<c16b1_original::WireStdio>(json!({"kind":"Unknown"}))
                        .err()
                        .unwrap()
                        .to_string()
                }
                5 => "unexpected control field".into(),
                _ => unreachable!(),
            };
            // Raw typed Frame tests preserve decoder ordering separately from
            // Channel's earlier descriptor-reference framing rejection.
            c16a_error(frame, &expected);
            assert!(!unix::readable(reader.as_raw_fd(), 0).unwrap());
            drop(prepared);
        }
        assert!(unix::readable(reader.as_raw_fd(), 100).unwrap());
        assert_eq!(reader.read(&mut [0]).unwrap(), 0);
    }
}
mod c17b2_original {
    use super::*;
    #[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
    pub enum TerminalId {
        Main,
        Process(u64),
    }
    #[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
    pub struct TerminalStatus {
        pub eof: bool,
        pub complete: bool,
        pub interrupted: bool,
        pub attached: bool,
        pub detached: bool,
        pub connected: bool,
    }
    #[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
    pub enum Source {
        Visible,
        Recent,
    }
    #[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
    pub enum Condition {
        Match(String),
        Regex(String),
        Exit,
    }
    #[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
    pub enum Call {
        Read {
            source: Source,
            lines: Option<usize>,
        },
        Send {
            text: Option<String>,
            keys: Vec<String>,
        },
        Wait {
            condition: Condition,
            timeout: u64,
            lines: Option<usize>,
        },
    }
}
fn c17b2_compare_serde<
    A: Serialize + DeserializeOwned + std::fmt::Debug,
    O: Serialize + DeserializeOwned + std::fmt::Debug,
>(
    cases: &[&str],
) {
    for input in cases {
        let old = serde_json::from_str::<O>(input)
            .map(|v| (format!("{v:?}"), serde_json::to_string(&v).unwrap()))
            .map_err(|e| e.to_string());
        let actual = serde_json::from_str::<A>(input)
            .map(|v| (format!("{v:?}"), serde_json::to_string(&v).unwrap()))
            .map_err(|e| e.to_string());
        assert_eq!(actual, old, "raw {input}");
        let v: Value = serde_json::from_str(input).unwrap();
        let old = serde_json::from_value::<O>(v.clone())
            .map(|v| (format!("{v:?}"), serde_json::to_value(&v).unwrap()))
            .map_err(|e| e.to_string());
        let actual = serde_json::from_value::<A>(v)
            .map(|v| (format!("{v:?}"), serde_json::to_value(&v).unwrap()))
            .map_err(|e| e.to_string());
        assert_eq!(actual, old, "Value {input}");
    }
}
#[test]
fn c17b2_nested_terminal_metadata_original_serde_forms_names_traits_and_defaults() {
    fn copy<T: Copy + Clone + std::fmt::Debug + Eq>() {}
    fn clone_eq<T: Clone + std::fmt::Debug + Eq>() {}
    fn ordered<T: Ord>() {}
    fn default<T: Default>() {}
    copy::<TerminalId>();
    ordered::<TerminalId>();
    copy::<TerminalStatus>();
    default::<TerminalStatus>();
    copy::<crate::terminal::Source>();
    clone_eq::<crate::terminal::Condition>();
    clone_eq::<crate::terminal::Call>();
    let mut ids = [
        TerminalId::Process(u64::MAX),
        TerminalId::Main,
        TerminalId::Process(0),
    ];
    ids.sort();
    assert_eq!(
        ids,
        [
            TerminalId::Main,
            TerminalId::Process(0),
            TerminalId::Process(u64::MAX)
        ]
    );
    assert_eq!(
        serde_json::to_value(TerminalStatus::default()).unwrap(),
        json!({"eof":false,"complete":false,"interrupted":false,"attached":false,"detached":false,"connected":false})
    );
    c17b2_compare_serde::<TerminalId, c17b2_original::TerminalId>(&[
        "\"Main\"",
        "{\"Main\":null}",
        "{\"Process\":0}",
        "{\"Process\":18446744073709551615}",
        "{\"Process\":1.0}",
        "{\"Process\":-1}",
        "{\"Process\":18446744073709551616}",
        "{\"Process\":null}",
        "{\"Process\":[1]}",
        "{\"Main\":{}}",
        "\"Process\"",
        "\"main\"",
        "{}",
        "[]",
        "null",
    ]);
    c17b2_compare_serde::<crate::terminal::Source, c17b2_original::Source>(&[
        "\"Visible\"",
        "\"Recent\"",
        "{\"Visible\":null}",
        "{\"Recent\":null}",
        "{\"Recent\":{}}",
        "\"visible\"",
        "{\"Visible\":null,\"Recent\":null}",
        "[]",
        "null",
    ]);
    c17b2_compare_serde::<crate::terminal::Condition, c17b2_original::Condition>(&[
        "{\"Match\":\"\"}",
        "{\"Match\":\"雪\\n\"}",
        "{\"Regex\":\"[\"}",
        "\"Exit\"",
        "{\"Exit\":null}",
        "{\"Match\":null}",
        "{\"Regex\":[]}",
        "\"Match\"",
        "{\"Exit\":{}}",
        "{\"Match\":\"a\",\"Regex\":\"b\"}",
        "\"exit\"",
        "null",
        "[]",
    ]);
    c17b2_compare_serde::<crate::terminal::Call, c17b2_original::Call>(&[
        "{\"Read\":{\"source\":\"Visible\"}}",
        "{\"Read\":{\"source\":{\"Recent\":null},\"lines\":null,\"extra\":true}}",
        "{\"Read\":{\"source\":\"Visible\",\"lines\":18446744073709551615}}",
        "{\"Read\":[\"Visible\",null]}",
        "{\"Read\":[\"Visible\"]}",
        "{\"Read\":{}}",
        "{\"Read\":{\"source\":\"Visible\",\"lines\":1.0}}",
        "{\"Read\":{\"source\":\"Visible\",\"lines\":-1}}",
        "{\"Read\":{\"source\":\"Visible\",\"lines\":18446744073709551616}}",
        "{\"Read\":{\"source\":\"Visible\",\"lines\":1,\"lines\":2}}",
        "{\"Send\":{\"keys\":[]}}",
        "{\"Send\":{\"text\":null,\"keys\":[],\"extra\":true}}",
        "{\"Send\":[null,[]]}",
        "{\"Send\":{\"text\":\"x\"}}",
        "{\"Send\":{\"keys\":null}}",
        "{\"Wait\":{\"condition\":\"Exit\",\"timeout\":0}}",
        "{\"Wait\":{\"condition\":{\"Regex\":\"[\"},\"timeout\":18446744073709551615,\"lines\":0,\"extra\":[]}}",
        "{\"Wait\":[\"Exit\",1,null]}",
        "{\"Wait\":[\"Exit\",1]}",
        "{\"Wait\":{\"condition\":\"Exit\",\"timeout\":1.0}}",
        "{\"Wait\":{\"condition\":\"Exit\"}}",
        "{\"Wait\":{\"timeout\":1}}",
        "{\"Wait\":null}",
        "\"Read\"",
        "[]",
        "null",
    ]);
    let good = "{\"eof\":false,\"complete\":true,\"interrupted\":false,\"attached\":true,\"detached\":false,\"connected\":true}";
    c17b2_compare_serde::<TerminalStatus, c17b2_original::TerminalStatus>(&[
        good,
        "[false,true,false,true,false,true]",
        "[false,true,false,true,false]",
        "[false,true,false,true,false,true,false]",
        "{}",
        "null",
        "{\"eof\":false,\"complete\":true,\"interrupted\":false,\"attached\":true,\"detached\":false,\"connected\":true,\"extra\":true}",
        "{\"eof\":false,\"complete\":true,\"interrupted\":false,\"attached\":true,\"detached\":false,\"connected\":true,\"eof\":true}",
    ]);
    let value: Value = serde_json::from_str(good).unwrap();
    for field in [
        "eof",
        "complete",
        "interrupted",
        "attached",
        "detached",
        "connected",
    ] {
        let mut missing = value.clone();
        missing.as_object_mut().unwrap().remove(field);
        for v in [missing, {
            let mut v = value.clone();
            v[field] = json!(null);
            v
        }] {
            let old = serde_json::from_value::<c17b2_original::TerminalStatus>(v.clone())
                .unwrap_err()
                .to_string();
            assert_eq!(
                serde_json::from_value::<TerminalStatus>(v)
                    .unwrap_err()
                    .to_string(),
                old
            );
        }
    }
}

fn c17b2_frame_cases() -> Vec<(Message, &'static str, Value, Vec<&'static str>)> {
    vec![
        (
            Message::Command(Command::Attach {
                terminal: TerminalId::Main,
                stream: null(),
                rows: 0,
                cols: u16::MAX,
            }),
            "Command.Attach",
            json!({"terminal":"Main","stream":{"$fd":0},"rows":0,"cols":65535}),
            vec!["terminal", "stream", "rows", "cols"],
        ),
        (
            Message::Command(Command::Detach {
                terminal: TerminalId::Process(u64::MAX),
            }),
            "Command.Detach",
            json!({"terminal":{"Process":u64::MAX}}),
            vec!["terminal"],
        ),
        (
            Message::Command(Command::Resize {
                terminal: TerminalId::Main,
                rows: u16::MAX,
                cols: 0,
            }),
            "Command.Resize",
            json!({"terminal":"Main","rows":65535,"cols":0}),
            vec!["terminal", "rows", "cols"],
        ),
        (
            Message::Command(Command::TerminalCall {
                request: u64::MAX,
                terminal: u64::MAX,
                call: crate::terminal::Call::Wait {
                    condition: crate::terminal::Condition::Regex("[".into()),
                    timeout: u64::MAX,
                    lines: Some(0),
                },
            }),
            "Command.TerminalCall",
            json!({"request":u64::MAX,"terminal":u64::MAX,"call":{"Wait":{"condition":{"Regex":"["},"timeout":u64::MAX,"lines":0}}}),
            vec!["request", "terminal", "call"],
        ),
        (
            Message::Command(Command::TerminalCancel { request: 0 }),
            "Command.TerminalCancel",
            json!({"request":0}),
            vec!["request"],
        ),
        (
            Message::Event(Event::Terminal {
                terminal: TerminalId::Process(0),
                status: TerminalStatus {
                    eof: true,
                    complete: false,
                    interrupted: true,
                    attached: false,
                    detached: true,
                    connected: false,
                },
                error: None,
            }),
            "Event.Terminal",
            json!({"terminal":{"Process":0},"status":{"eof":true,"complete":false,"interrupted":true,"attached":false,"detached":true,"connected":false},"error":null}),
            vec!["terminal", "status", "error"],
        ),
        (
            Message::Event(Event::TerminalResult {
                request: u64::MAX,
                result: Ok(json!({"$fd":0,"nested":[true,null,"雪\n"]})),
            }),
            "Event.TerminalResult",
            json!({"request":u64::MAX,"result":"{\"Ok\":{\"$fd\":0,\"nested\":[true,null,\"雪\\n\"]}}"}),
            vec!["request", "result"],
        ),
    ]
}
#[test]
fn c17b2_seven_producer_frames_required_fields_and_nullable_error_are_exact() {
    for (message, method, params, fields) in c17b2_frame_cases() {
        let mut prepared = Encoder::default().prepare(message).unwrap();
        assert!(!prepared.needs_adoption());
        assert_eq!(prepared.frames.len(), 1);
        let original = prepared.frames.pop_front().unwrap();
        assert_eq!(original.fds.len(), usize::from(method == "Command.Attach"));
        assert_eq!(
            original.value,
            json!({"jsonrpc":"2.0","method":method,"params":params})
        );
        for field in &fields {
            let mut value = original.value.clone();
            value["params"].as_object_mut().unwrap().remove(*field);
            c16a_error(
                Frame {
                    value,
                    fds: original
                        .fds
                        .iter()
                        .map(|fd| fd.try_clone().unwrap())
                        .collect(),
                },
                &format!("missing control field {field}"),
            );
        }
        let mut value = original.value.clone();
        value["params"] = json!({});
        c16a_error(
            Frame {
                value,
                fds: original
                    .fds
                    .iter()
                    .map(|fd| fd.try_clone().unwrap())
                    .collect(),
            },
            &format!("missing control field {}", fields[0]),
        );
        let mut value = original.value.clone();
        value["params"]["extra"] = json!(true);
        c16a_error(
            Frame {
                value,
                fds: original
                    .fds
                    .iter()
                    .map(|fd| fd.try_clone().unwrap())
                    .collect(),
            },
            "unexpected control field",
        );
        let mut value = original.value.clone();
        value["params"] = json!([]);
        c16a_error(
            Frame {
                value,
                fds: original
                    .fds
                    .iter()
                    .map(|fd| fd.try_clone().unwrap())
                    .collect(),
            },
            "expected control object",
        );
        let expected = original.value.clone();
        let received = Decoder::default().receive(original).unwrap().unwrap();
        assert!(received.adopted.is_none());
        assert_eq!(
            Encoder::default().prepare(received.message).unwrap().frames[0].value,
            expected
        );
    }
    roundtrip(Message::Event(Event::Terminal {
        terminal: TerminalId::Main,
        status: TerminalStatus::default(),
        error: Some("failed\n雪".into()),
    }));
}

#[test]
fn c17b2_scalar_and_nested_terminal_errors_keep_original_field_order() {
    fn old<T: DeserializeOwned>(v: Value) -> String {
        serde_json::from_value::<T>(v).err().unwrap().to_string()
    }
    let mut frame = c16a_frame(Message::Command(Command::Resize {
        terminal: TerminalId::Main,
        rows: 1,
        cols: 1,
    }));
    frame.value["params"]["rows"] = json!(1.0);
    frame.value["params"]["cols"] = json!(-1);
    c16a_error(frame, &old::<u16>(json!(1.0)));
    for value in [json!(-1), json!(65536), json!(1.0), json!(null)] {
        let mut frame = c16a_frame(Message::Command(Command::Resize {
            terminal: TerminalId::Main,
            rows: 1,
            cols: 1,
        }));
        frame.value["params"]["cols"] = value.clone();
        c16a_error(frame, &old::<u16>(value));
    }
    for (field, value, expected) in [
        ("request", json!(1.0), old::<u64>(json!(1.0))),
        ("terminal", json!(-1), old::<u64>(json!(-1))),
        (
            "call",
            json!({"Read":{}}),
            old::<c17b2_original::Call>(json!({"Read":{}})),
        ),
    ] {
        let mut frame = c16a_frame(Message::Command(Command::TerminalCall {
            request: 0,
            terminal: 0,
            call: crate::terminal::Call::Read {
                source: crate::terminal::Source::Visible,
                lines: None,
            },
        }));
        frame.value["params"][field] = value;
        frame.value["params"]["extra"] = json!(true);
        c16a_error(frame, &expected);
    }
    let mut frame = c16a_frame(Message::Event(Event::Terminal {
        terminal: TerminalId::Main,
        status: TerminalStatus::default(),
        error: None,
    }));
    frame.value["params"]["status"] = json!({});
    frame.value["params"]["error"] = json!(1);
    c16a_error(frame, &old::<c17b2_original::TerminalStatus>(json!({})));
    let mut frame = c16a_frame(Message::Event(Event::Terminal {
        terminal: TerminalId::Main,
        status: TerminalStatus::default(),
        error: None,
    }));
    frame.value["params"]["error"] = json!(1);
    c16a_error(frame, &old::<Option<String>>(json!(1)));
}

#[test]
fn c17b2_terminal_result_opaque_json_and_extraction_order_preserve_original() {
    for result in [
        Ok(json!(null)),
        Ok(json!(false)),
        Ok(json!(-1)),
        Ok(json!(1.5)),
        Ok(json!("雪\n")),
        Ok(json!([1, true, null])),
        Ok(json!({"$fd":0,"nested":{"$fd":999}})),
        Err("failed\n雪".into()),
    ] {
        let mut prepared = Encoder::default()
            .prepare(Message::Event(Event::TerminalResult {
                request: u64::MAX,
                result: result.clone(),
            }))
            .unwrap();
        let mut frame = prepared.frames.pop_front().unwrap();
        assert!(frame.value["params"]["result"].is_string());
        assert!(frame.fds.is_empty());
        let (left, right) = std::os::unix::net::UnixStream::pair().unwrap();
        let mut sender = channel::Channel::new(left).unwrap();
        let mut receiver = channel::Channel::new(right).unwrap();
        sender.queue(&frame.value, &mut frame.fds).unwrap();
        sender.flush().unwrap();
        assert!(!sender.wants_write());
        let received = Decoder::default()
            .receive_channel(&mut receiver)
            .unwrap()
            .unwrap();
        assert!(received.adopted.is_none());
        assert!(
            matches!(received.message,Message::Event(Event::TerminalResult {request:u64::MAX,result:actual}) if actual==result)
        );
    }
    for encoded in [
        "{",
        "null",
        "{}",
        "{\"Err\":null}",
        "{\"Other\":null}",
        "{\"Ok\":null,\"Err\":\"x\"}",
    ] {
        let expected = serde_json::from_str::<Result<Value, String>>(encoded)
            .unwrap_err()
            .to_string();
        let mut frame = c16a_frame(Message::Event(Event::TerminalResult {
            request: 1,
            result: Ok(json!(null)),
        }));
        frame.value["params"]["result"] = json!(encoded);
        frame.value["params"]["extra"] = json!(true);
        c16a_error(frame, &expected);
    }
    let mut frame = c16a_frame(Message::Event(Event::TerminalResult {
        request: 1,
        result: Ok(json!(null)),
    }));
    frame.value["params"]["result"] = json!({"Ok":null});
    c16a_error(
        frame,
        &serde_json::from_value::<String>(json!({"Ok":null}))
            .unwrap_err()
            .to_string(),
    );
    let mut frame = c16a_frame(Message::Event(Event::TerminalResult {
        request: 1,
        result: Ok(json!(null)),
    }));
    frame.value["params"]["request"] = json!(1.0);
    frame.value["params"]["result"] = json!("{");
    c16a_error(
        frame,
        &serde_json::from_value::<u64>(json!(1.0))
            .unwrap_err()
            .to_string(),
    );
}

#[test]
fn c17b2_attach_capability_lifetimes_and_rejection_order_are_preserved() {
    let _no_forks = unix::no_forks();
    for fault in 0..6 {
        let (reader, writer) = unix::pipe().unwrap();
        let mut reader = fs::File::from(reader);
        let mut prepared = Encoder::default()
            .prepare(Message::Command(Command::Attach {
                terminal: TerminalId::Main,
                stream: writer.into(),
                rows: 24,
                cols: 80,
            }))
            .unwrap();
        let mut frame = prepared.frames.pop_front().unwrap();
        assert_eq!(frame.fds.len(), 1);
        assert_eq!(frame.value["params"]["stream"], json!({"$fd":0}));
        assert!(!prepared.needs_adoption());
        if fault == 0 {
            let (left, right) = std::os::unix::net::UnixStream::pair().unwrap();
            let mut sender = channel::Channel::new(left).unwrap();
            let mut receiver = channel::Channel::new(right).unwrap();
            sender.queue(&frame.value, &mut frame.fds).unwrap();
            sender.flush().unwrap();
            assert!(!sender.wants_write());
            let received = Decoder::default()
                .receive_channel(&mut receiver)
                .unwrap()
                .unwrap();
            assert!(received.adopted.is_none());
            drop(prepared);
            assert!(!unix::readable(reader.as_raw_fd(), 0).unwrap());
            drop(received);
        } else {
            frame.value["params"]["extra"] = json!(true);
            let expected = match fault {
                1 => {
                    let value = json!({"Process":1.0});
                    frame.value["params"]["terminal"] = value.clone();
                    frame.value["params"]["stream"] = json!({"$fd":99});
                    serde_json::from_value::<c17b2_original::TerminalId>(value)
                        .unwrap_err()
                        .to_string()
                }
                2 => {
                    frame.value["params"]["stream"] = json!({"$fd":99});
                    frame.value["params"]["rows"] = json!(1.0);
                    "invalid or duplicate descriptor reference".into()
                }
                3 => {
                    frame.value["params"]["rows"] = json!(1.0);
                    serde_json::from_value::<u16>(json!(1.0))
                        .unwrap_err()
                        .to_string()
                }
                4 => {
                    frame.value["params"]["stream"] = json!({});
                    serde_json::from_value::<c16a2_original::Descriptor>(json!({}))
                        .unwrap_err()
                        .to_string()
                }
                5 => "unexpected control field".into(),
                _ => unreachable!(),
            };
            // Typed-frame failures are separate from the Channel's earlier
            // framing/index validation; ownership assertions apply to both.
            c16a_error(frame, &expected);
            assert!(!unix::readable(reader.as_raw_fd(), 0).unwrap());
            drop(prepared);
        }
        assert!(unix::readable(reader.as_raw_fd(), 100).unwrap());
        assert_eq!(reader.read(&mut [0]).unwrap(), 0);
    }
}
mod c17c_original {
    use super::*;
    #[derive(Clone, Debug, Serialize, Deserialize)]
    #[serde(deny_unknown_fields)]
    pub struct Request {
        pub id: String,
        #[serde(default = "package_kind")]
        pub kind: String,
        pub package: String,
        pub reason: String,
    }
    fn package_kind() -> String {
        "package".into()
    }
    #[derive(Debug, Serialize, Deserialize)]
    #[serde(deny_unknown_fields)]
    pub struct Reply {
        pub id: Option<String>,
        pub status: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        pub message: Option<String>,
    }
    #[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
    pub struct Changes {
        pub update: Option<Vec<String>>,
        pub lock: bool,
    }
    #[derive(Clone, Debug, Serialize, Deserialize)]
    pub struct DevShellSummary {
        pub identity: String,
        pub generation: u32,
        pub lock: Value,
    }
    #[derive(Debug, Serialize, Deserialize)]
    #[serde(deny_unknown_fields)]
    pub struct WirePreview {
        pub output_path: Option<PathBuf>,
        pub in_store: bool,
        pub download: Option<String>,
        pub build_required: bool,
    }
}
#[test]
fn c17c_nested_original_serde_defaults_closedness_order_and_traits() {
    fn clone_debug<T: Clone + std::fmt::Debug>() {}
    fn default_eq<T: Default + PartialEq + Eq>() {}
    clone_debug::<goblins_protocol::Request>();
    clone_debug::<crate::devshell::Changes>();
    clone_debug::<crate::policy::DevShellSummary>();
    default_eq::<crate::devshell::Changes>();
    let changes = crate::devshell::Changes::default();
    assert_eq!(changes.update, None);
    assert!(!changes.lock);
    assert_eq!(goblins_protocol::package_kind(), "package");
    c17b2_compare_serde::<goblins_protocol::Request, c17c_original::Request>(&[
        r#"{"id":"","package":"../bad name","reason":""}"#,
        r#"{"id":"r","kind":"future","package":"","reason":"雪\n"}"#,
        r#"{"id":"r","kind":null,"package":"p","reason":"r"}"#,
        r#"{"id":"r","package":"p","reason":"r","extra":1}"#,
        r#"{}"#,
        r#"{"id":"r"}"#,
        r#"{"id":"r","package":"p"}"#,
        r#"{"id":"a","id":"b","package":"p","reason":"r"}"#,
        r#"["r","package","p","why"]"#,
        r#"["r","p","why"]"#,
        r#"["r","package","p","why",0]"#,
        r#"null"#,
    ]);
    c17b2_compare_serde::<goblins_protocol::Reply, c17c_original::Reply>(&[
        r#"{"status":"future"}"#,
        r#"{"id":null,"status":"","message":null}"#,
        r#"{"id":"r","status":"ok","message":"雪\n"}"#,
        r#"{"status":"ok","extra":0}"#,
        r#"{}"#,
        r#"{"status":null}"#,
        r#"{"id":"a","id":"b","status":"ok"}"#,
        r#"[null,"ok",null]"#,
        r#"[null,"ok"]"#,
        r#"[]"#,
        r#"{"id":1,"status":"ok"}"#,
        r#"{"status":"ok","message":false}"#,
    ]);
    c17b2_compare_serde::<crate::devshell::Changes, c17c_original::Changes>(&[
        r#"{"lock":false}"#,
        r#"{"update":null,"lock":true}"#,
        r#"{"update":[],"lock":false,"future":0}"#,
        r#"{"update":["../invalid",""],"lock":true}"#,
        r#"{}"#,
        r#"{"lock":null}"#,
        r#"{"update":[1],"lock":true}"#,
        r#"{"lock":true,"lock":false}"#,
        r#"[null,true]"#,
        r#"[[],false]"#,
        r#"[true]"#,
        r#"[null,true,0]"#,
    ]);
    c17b2_compare_serde::<crate::policy::DevShellSummary, c17c_original::DevShellSummary>(&[
        r#"{"identity":"","generation":0,"lock":null}"#,
        r#"{"identity":"relative#?","generation":4294967295,"lock":{"$fd":0},"future":0}"#,
        r#"{"identity":"x","generation":4294967296,"lock":{}}"#,
        r#"{"identity":"x","generation":-1,"lock":null}"#,
        r#"{"identity":"x","generation":1.0,"lock":null}"#,
        r#"{"identity":"x","generation":0}"#,
        r#"{}"#,
        r#"{"identity":"x","generation":1,"generation":2,"lock":null}"#,
        r#"["x",0,{"$fd":0}]"#,
        r#"["x",0]"#,
        r#"["x",0,null,0]"#,
    ]);
    let reply = goblins_protocol::Reply::new(None, "future", None);
    assert_eq!(
        serde_json::to_value(reply).unwrap(),
        json!({"id":null,"status":"future"})
    );
}
fn c17c_request() -> goblins_protocol::Request {
    goblins_protocol::Request {
        id: "".into(),
        kind: "future".into(),
        package: "../bad name".into(),
        reason: "雪\n".into(),
    }
}
fn c17c_preview() -> crate::catalog::Preview {
    crate::catalog::Preview {
        output_path: Some("relative/../output".into()),
        in_store: false,
        download: Some("not a size".into()),
        build_required: true,
    }
}
fn c17c_cases() -> Vec<(Message, &'static str, Value, Vec<&'static str>)> {
    vec![
        (
            Message::Command(Command::Cancel(u64::MAX)),
            "Command.Cancel",
            json!({"id":u64::MAX}),
            vec!["id"],
        ),
        (
            Message::Command(Command::Preview {
                approval: 0,
                package: "../bad name".into(),
            }),
            "Command.Preview",
            json!({"approval":0,"package":"../bad name"}),
            vec!["approval", "package"],
        ),
        (
            Message::Command(Command::Decide {
                request: c17c_request(),
                approved: false,
                output: None,
            }),
            "Command.Decide",
            json!({"request":{"id":"","kind":"future","package":"../bad name","reason":"雪\n"},"approved":false,"output":null}),
            vec!["request", "approved", "output"],
        ),
        (
            Message::Command(Command::Refresh {
                approval: u64::MAX,
                changes: crate::devshell::Changes {
                    update: Some(vec![]),
                    lock: true,
                },
            }),
            "Command.Refresh",
            json!({"approval":u64::MAX,"changes":{"update":[],"lock":true}}),
            vec!["approval", "changes"],
        ),
        (
            Message::Command(Command::Run {
                serial: u64::MAX,
                app: "../bad app".into(),
            }),
            "Command.Run",
            json!({"serial":u64::MAX,"app":"../bad app"}),
            vec!["serial", "app"],
        ),
        (
            Message::Command(Command::Diff {
                serial: 0,
                changes: Default::default(),
            }),
            "Command.Diff",
            json!({"serial":0,"changes":{"update":null,"lock":false}}),
            vec!["serial", "changes"],
        ),
        (
            Message::Event(Event::Preview {
                approval: 0,
                result: Ok(c17c_preview()),
            }),
            "Event.Preview",
            json!({"approval":0,"result":{"Ok":{"output_path":"relative/../output","in_store":false,"download":"not a size","build_required":true}}}),
            vec!["approval", "result"],
        ),
        (
            Message::Event(Event::Progress {
                request: "".into(),
                state: "future-state".into(),
            }),
            "Event.Progress",
            json!({"request":"","state":"future-state"}),
            vec!["request", "state"],
        ),
        (
            Message::Event(Event::Refresh {
                approval: u64::MAX,
                result: Ok((json!({"$fd":0}), "relative".into())),
            }),
            "Event.Refresh",
            json!({"approval":u64::MAX,"result":"{\"Ok\":[{\"$fd\":0},\"relative\"]}"}),
            vec!["approval", "result"],
        ),
        (
            Message::Event(Event::Call {
                serial: 0,
                result: Err("failed\n雪".into()),
            }),
            "Event.Call",
            json!({"serial":0,"result":"{\"Err\":\"failed\\n雪\"}"}),
            vec!["serial", "result"],
        ),
        (
            Message::Event(Event::DevShell {
                summary: crate::policy::DevShellSummary {
                    identity: "".into(),
                    generation: u32::MAX,
                    lock: json!({"$fd":0}),
                },
            }),
            "Event.DevShell",
            json!({"summary":"{\"identity\":\"\",\"generation\":4294967295,\"lock\":{\"$fd\":0}}"}),
            vec!["summary"],
        ),
        (
            Message::Event(Event::Granted {
                reply: goblins_protocol::Reply::new(None, "future", None),
                detail: None,
                output: None,
            }),
            "Event.Granted",
            json!({"reply":{"id":null,"status":"future"},"detail":null,"output":null}),
            vec!["reply", "detail", "output"],
        ),
    ]
}
#[test]
fn c17c_twelve_envelopes_required_nullable_roots_and_original_extraction_order() {
    for (message, method, params, fields) in c17c_cases() {
        let mut prepared = Encoder::default().prepare(message).unwrap();
        assert!(!prepared.needs_adoption());
        assert_eq!(prepared.frames.len(), 1);
        let frame = prepared.frames.pop_front().unwrap();
        assert!(frame.fds.is_empty());
        assert_eq!(
            frame.value,
            json!({"jsonrpc":"2.0","method":method,"params":params})
        );
        for field in &fields {
            let mut value = frame.value.clone();
            value["params"].as_object_mut().unwrap().remove(*field);
            c16a_error(
                Frame { value, fds: vec![] },
                &format!("missing control field {field}"),
            );
        }
        let mut value = frame.value.clone();
        value["params"] = json!({});
        c16a_error(
            Frame { value, fds: vec![] },
            &format!("missing control field {}", fields[0]),
        );
        let mut value = frame.value.clone();
        value["params"]["extra"] = json!(true);
        c16a_error(Frame { value, fds: vec![] }, "unexpected control field");
        let mut value = frame.value.clone();
        value["params"] = json!([]);
        c16a_error(Frame { value, fds: vec![] }, "expected control object");
        let expected = frame.value.clone();
        let received = Decoder::default().receive(frame).unwrap().unwrap();
        assert!(received.adopted.is_none());
        assert_eq!(c16a_frame(received.message).value, expected);
    }
    roundtrip(Message::Command(Command::Decide {
        request: c17c_request(),
        approved: true,
        output: Some("relative".into()),
    }));
    roundtrip(Message::Event(Event::Granted {
        reply: goblins_protocol::Reply::new(Some("r".into()), "ok", Some("detail".into())),
        detail: Some("more".into()),
        output: Some("relative".into()),
    }));
    roundtrip(Message::Event(Event::Preview {
        approval: u64::MAX,
        result: Err("failed".into()),
    }));
}
#[test]
fn c17c_scalar_nested_and_nullable_errors_precede_root_finish() {
    fn err<T: DeserializeOwned>(v: Value) -> String {
        serde_json::from_value::<T>(v).err().unwrap().to_string()
    }
    let bad_u64 = err::<u64>(json!(1.0));
    let bad_string = err::<String>(json!(null));
    let cases = vec![
        ("Command.Cancel", "id", json!(1.0), bad_u64.clone()),
        ("Command.Preview", "approval", json!(1.0), bad_u64.clone()),
        (
            "Command.Preview",
            "package",
            json!(null),
            bad_string.clone(),
        ),
        (
            "Command.Decide",
            "request",
            json!({}),
            err::<c17c_original::Request>(json!({})),
        ),
        (
            "Command.Decide",
            "approved",
            json!(null),
            err::<bool>(json!(null)),
        ),
        (
            "Command.Decide",
            "output",
            json!([]),
            err::<Option<PathBuf>>(json!([])),
        ),
        ("Command.Refresh", "approval", json!(1.0), bad_u64.clone()),
        (
            "Command.Refresh",
            "changes",
            json!({}),
            err::<c17c_original::Changes>(json!({})),
        ),
        ("Command.Run", "serial", json!(1.0), bad_u64.clone()),
        ("Command.Run", "app", json!(null), bad_string.clone()),
        ("Command.Diff", "serial", json!(1.0), bad_u64.clone()),
        (
            "Command.Diff",
            "changes",
            json!({}),
            err::<c17c_original::Changes>(json!({})),
        ),
        ("Event.Preview", "approval", json!(1.0), bad_u64.clone()),
        (
            "Event.Preview",
            "result",
            json!({"Ok":{}}),
            err::<Result<c17c_original::WirePreview, String>>(json!({"Ok":{}})),
        ),
        ("Event.Progress", "request", json!(null), bad_string.clone()),
        ("Event.Progress", "state", json!(null), bad_string.clone()),
        ("Event.Refresh", "approval", json!(1.0), bad_u64.clone()),
        (
            "Event.Refresh",
            "result",
            json!({}),
            err::<String>(json!({})),
        ),
        ("Event.Call", "serial", json!(1.0), bad_u64),
        ("Event.Call", "result", json!({}), err::<String>(json!({}))),
        (
            "Event.DevShell",
            "summary",
            json!({}),
            err::<String>(json!({})),
        ),
        (
            "Event.Granted",
            "reply",
            json!({}),
            err::<c17c_original::Reply>(json!({})),
        ),
        (
            "Event.Granted",
            "detail",
            json!([]),
            err::<Option<String>>(json!([])),
        ),
        (
            "Event.Granted",
            "output",
            json!([]),
            err::<Option<PathBuf>>(json!([])),
        ),
    ];
    for (method, field, bad, expected) in cases {
        let (_, _, params, fields) = c17c_cases()
            .into_iter()
            .find(|(_, name, _, _)| *name == method)
            .unwrap();
        let mut params = params;
        // Invalid current field wins over missing later fields and root extras.
        let index = fields.iter().position(|name| *name == field).unwrap();
        for later in &fields[index + 1..] {
            params.as_object_mut().unwrap().remove(*later);
        }
        params[field] = bad;
        params["extra"] = json!(true);
        c16a_error(
            Frame {
                value: json!({"jsonrpc":"2.0","method":method,"params":params}),
                fds: vec![],
            },
            &expected,
        );
    }
}
#[test]
fn c17c_preview_private_mapping_preserves_public_skip_and_original_wire_forms() {
    let preview = c17c_preview();
    assert_eq!(
        serde_json::to_value(&preview).unwrap(),
        json!({"in_store":false,"download":"not a size","build_required":true})
    );
    assert_eq!(
        encode_preview(&Ok(preview)),
        json!({"Ok":{"output_path":"relative/../output","in_store":false,"download":"not a size","build_required":true}})
    );
    for input in [
        json!({"Ok":{"output_path":"relative","in_store":false,"download":null,"build_required":true}}),
        json!({"Ok":{"in_store":true,"build_required":false}}),
        json!({"Ok":{"output_path":null,"in_store":false,"download":"arbitrary","build_required":true}}),
        json!({"Ok":[null,true,null,false]}),
        json!({"Err":"failed\n雪"}),
        json!({"Err":null}),
        json!({"Ok":{}}),
        json!({"Ok":{"in_store":true,"build_required":false,"extra":0}}),
        json!({"Ok":{"in_store":null,"build_required":false}}),
        json!({"Ok":{"in_store":true,"build_required":false,"output_path":[]}}),
        json!({"Ok":{"in_store":true,"build_required":false,"download":0}}),
        json!({"Ok":[null,true,null]}),
        json!({"Ok":[null,true,null,false,0]}),
        json!({"Other":null}),
        json!({"Ok":null}),
        json!({"Ok":{},"Err":"e"}),
    ] {
        let old=serde_json::from_value::<Result<c17c_original::WirePreview,String>>(input.clone())
            .map(|result|result.map(|p|json!({"output_path":p.output_path,"in_store":p.in_store,"download":p.download,"build_required":p.build_required})))
            .map_err(|error|error.to_string());
        let actual=decode_preview(input.clone())
            .map(|result|result.map(|p|json!({"output_path":p.output_path,"in_store":p.in_store,"download":p.download,"build_required":p.build_required})))
            .map_err(|error|error.to_string());
        assert_eq!(actual, old, "preview {input}");
        let mut frame = c16a_frame(Message::Event(Event::Preview {
            approval: 0,
            result: Err("seed".into()),
        }));
        frame.value["params"]["result"] = input;
        if let Err(expected) = old {
            frame.value["params"]["extra"] = json!(true);
            c16a_error(frame, &expected);
        } else {
            let received = Decoder::default().receive(frame).unwrap().unwrap();
            assert!(received.adopted.is_none());
            if let Message::Event(Event::Preview { result: Ok(p), .. }) = received.message {
                assert_eq!(
                    serde_json::to_value(&p).unwrap().as_object().unwrap().len(),
                    3
                );
                assert!(
                    serde_json::to_value(&p)
                        .unwrap()
                        .get("output_path")
                        .is_none()
                );
            }
        }
    }
}
#[test]
fn c17c_opaque_call_refresh_summary_preserve_json_data_and_parser_order() {
    let _no_forks = unix::no_forks();
    for data in [
        json!(null),
        json!(false),
        json!(-1),
        json!(1.5),
        json!("雪\n"),
        json!([null, true]),
        json!({"$fd":0,"nested":{"$fd":999}}),
    ] {
        for message in [
            Message::Event(Event::Call {
                serial: u64::MAX,
                result: Ok(data.clone()),
            }),
            Message::Event(Event::Refresh {
                approval: u64::MAX,
                result: Ok((data.clone(), "relative#?".into())),
            }),
            Message::Event(Event::DevShell {
                summary: crate::policy::DevShellSummary {
                    identity: "".into(),
                    generation: u32::MAX,
                    lock: data.clone(),
                },
            }),
        ] {
            let mut prepared = Encoder::default().prepare(message).unwrap();
            assert!(!prepared.needs_adoption());
            assert_eq!(prepared.frames.len(), 1);
            let mut frame = prepared.frames.pop_front().unwrap();
            assert!(frame.fds.is_empty());
            let field = if frame.value["method"] == "Event.DevShell" {
                "summary"
            } else {
                "result"
            };
            assert!(frame.value["params"][field].is_string());
            let expected = frame.value.clone();
            let (left, right) = std::os::unix::net::UnixStream::pair().unwrap();
            let mut sender = channel::Channel::new(left).unwrap();
            let mut receiver = channel::Channel::new(right).unwrap();
            sender.queue(&frame.value, &mut frame.fds).unwrap();
            sender.flush().unwrap();
            assert!(!sender.wants_write());
            let received = Decoder::default()
                .receive_channel(&mut receiver)
                .unwrap()
                .unwrap();
            assert!(received.adopted.is_none());
            match &received.message {
                Message::Event(Event::Call {
                    result: Ok(actual), ..
                }) => assert_eq!(actual, &data),
                Message::Event(Event::Refresh {
                    result: Ok((actual, path)),
                    ..
                }) => {
                    assert_eq!(actual, &data);
                    assert_eq!(path, "relative#?");
                }
                Message::Event(Event::DevShell { summary }) => assert_eq!(summary.lock, data),
                _ => panic!("wrong opaque event"),
            }
            assert_eq!(c16a_frame(received.message).value, expected);
        }
    }
    for message in [
        Message::Event(Event::Call {
            serial: 0,
            result: Err("failed".into()),
        }),
        Message::Event(Event::Refresh {
            approval: 0,
            result: Err("failed".into()),
        }),
    ] {
        roundtrip(message);
    }
    for (_, method, params, _) in c17c_cases().into_iter().filter(|(_, method, _, _)| {
        ["Event.Call", "Event.Refresh", "Event.DevShell"].contains(method)
    }) {
        let field = if method == "Event.DevShell" {
            "summary"
        } else {
            "result"
        };
        for text in [
            "{",
            "null",
            "{}",
            r#"{"Err":null}"#,
            r#"{"Ok":[],"Err":"e"}"#,
            r#"{"Ok":[null]}"#,
            r#"{"identity":"x","generation":4294967296,"lock":null}"#,
        ] {
            let error = match method {
                "Event.Call" => serde_json::from_str::<Result<Value, String>>(text)
                    .err()
                    .map(|e| e.to_string()),
                "Event.Refresh" => serde_json::from_str::<Result<(Value, String), String>>(text)
                    .err()
                    .map(|e| e.to_string()),
                _ => serde_json::from_str::<c17c_original::DevShellSummary>(text)
                    .err()
                    .map(|e| e.to_string()),
            };
            let mut value = json!({"jsonrpc":"2.0","method":method,"params":params});
            value["params"][field] = json!(text);
            if let Some(expected) = error {
                value["params"]["extra"] = json!(true);
                c16a_error(
                    Frame {
                        value: value.clone(),
                        fds: vec![],
                    },
                    &expected,
                );
                if method != "Event.DevShell" {
                    let first = if method == "Event.Call" {
                        "serial"
                    } else {
                        "approval"
                    };
                    value["params"].as_object_mut().unwrap().remove(first);
                    c16a_error(
                        Frame { value, fds: vec![] },
                        &format!("missing control field {first}"),
                    );
                }
            } else {
                // A valid Result<Value, String> can contain structurally odd data.
                let received = Decoder::default()
                    .receive(Frame { value, fds: vec![] })
                    .unwrap()
                    .unwrap();
                assert!(received.adopted.is_none());
            }
        }
    }
}
#[test]
fn c17c_path_serialization_errors_and_preview_skip_boundary_are_original() {
    use std::os::unix::ffi::OsStringExt;
    let bad = PathBuf::from(std::ffi::OsString::from_vec(vec![b'/', 0xff]));
    let expected = serde_json::to_value(&bad).unwrap_err().to_string();
    for message in [
        Message::Command(Command::Decide {
            request: c17c_request(),
            approved: false,
            output: Some(bad.clone()),
        }),
        Message::Event(Event::Granted {
            reply: goblins_protocol::Reply::new(None, "", None),
            detail: None,
            output: Some(bad.clone()),
        }),
    ] {
        let error = match Encoder::default().prepare(message) {
            Err(error) => error,
            Ok(_) => panic!("serialized non-UTF8 path"),
        };
        assert_eq!(error.to_string(), expected);
    }
    let preview = crate::catalog::Preview {
        output_path: Some(bad),
        in_store: false,
        download: None,
        build_required: false,
    };
    // Public Preview skips output_path; private encode_preview explicitly serializes it.
    assert_eq!(
        serde_json::to_value(&preview).unwrap(),
        json!({"in_store":false,"download":null,"build_required":false})
    );
    assert!(std::panic::catch_unwind(|| encode_preview(&Ok(preview))).is_err());
}
