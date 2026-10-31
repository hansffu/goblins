use super::*;
use std::{
    fs,
    os::{
        fd::{FromRawFd, OwnedFd},
        unix::{fs::PermissionsExt, net::UnixStream},
    },
    thread,
};

pub(crate) fn executable(root: &Path, mode: &str) -> Executable {
    fn quote(s: &str) -> String {
        format!("'{}'", s.replace('\'', "'\"'\"'"))
    }
    let path = root.join(format!("supervisor-{mode}"));
    let exe = std::env::current_exe().unwrap();
    fs::write(&path, format!("#!/bin/sh\nexport GOBLINS_TEST_SUPERVISOR_ROOT={}\nexport GOBLINS_TEST_SUPERVISOR_MODE={}\nexec {} --ignored --exact supervised::tests::peer --nocapture\n", quote(root.to_str().unwrap()), quote(mode), quote(exe.to_str().unwrap()))).unwrap();
    fs::set_permissions(&path, fs::Permissions::from_mode(0o700)).unwrap();
    Executable::from_path(Some(path))
}
pub(crate) fn pause(runtime: &Supervised) {
    runtime
        .process
        .as_ref()
        .unwrap()
        .child
        .signal_for_test(libc::SIGSTOP);
}
fn source(root: &Path) -> Source {
    Source::Host {
        configuration: root.join("absent.json"),
        name: "shell".into(),
        scope: None,
        dev_shell: None,
        workspace: None,
        cwd: None,
        state: root.into(),
        history: None,
        recreate: None,
        sequence: 1,
    }
}
pub(crate) fn launch(root: &Path, mode: &str) -> Supervised {
    goblins_unix::private_directory(&root.join("session")).unwrap();
    Supervised::start(
        executable(root, mode),
        source(root),
        root.join("session/resources"),
        (24, 80),
        false,
    )
    .unwrap()
}
fn round(runtime: &mut Supervised) -> Vec<Event> {
    std::iter::from_fn(|| runtime.poll()).collect()
}
pub(crate) fn ready(runtime: &mut Supervised, root: &Path) {
    let deadline = Instant::now() + Duration::from_secs(5);
    while !root.join("ready").exists() {
        assert!(
            Instant::now() < deadline,
            "test supervisor did not decode bootstrap"
        );
        round(runtime);
        thread::sleep(Duration::from_millis(1));
    }
}
fn transmit(channel: &mut Channel, encoder: &mut Encoder, message: Message) {
    let mut prepared = encoder.prepare(message).unwrap();
    while let Some(mut frame) = prepared.frames.pop_front() {
        channel.queue(&frame.value, &mut frame.fds).unwrap();
        while channel.wants_write() {
            channel.flush().unwrap();
            thread::sleep(Duration::from_millis(1));
        }
    }
}
/// Real inherited-control child fixture, always exercised by its parents.
#[test]
#[ignore = "subprocess helper; exercised by supervised lifecycle tests"]
fn peer() {
    let mode = std::env::var("GOBLINS_TEST_SUPERVISOR_MODE").unwrap();
    if mode == "engine" {
        std::process::exit(if goblins_supervisor::control_main().is_ok() {
            0
        } else {
            1
        });
    }
    let root = PathBuf::from(std::env::var_os("GOBLINS_TEST_SUPERVISOR_ROOT").unwrap());
    let mut channel =
        Channel::new(unsafe { UnixStream::from_raw_fd(supervisor::CONTROL_FD) }).unwrap();
    let mut decoder = Decoder::default();
    let mut encoder = Encoder::default();
    let deadline = Instant::now() + Duration::from_secs(30);
    let startup = loop {
        assert!(Instant::now() < deadline);
        if let Some(message) = decoder.receive_channel(&mut channel).unwrap() {
            break message;
        }
        thread::sleep(Duration::from_millis(1));
    };
    if mode.starts_with("preflight-") {
        super::preflight::tests::peer(&root, &mode, startup, channel);
        return;
    }
    assert!(matches!(startup.message, Message::SupervisorStart(_)));
    if mode != "unacked"
        && !mode.starts_with("close-")
        && let Some(transfer) = startup.adopted
    {
        transmit(&mut channel, &mut encoder, Message::Adopted { transfer });
    }
    transmit(
        &mut channel,
        &mut encoder,
        Message::Event(Event::Mode { server: false }),
    );
    fs::write(root.join("pid"), std::process::id().to_string()).unwrap();
    let clean = matches!(mode.as_str(), "clean" | "clean-barrier" | "clean-tail")
        || mode.starts_with("close-");
    if mode == "stopped" || clean {
        transmit(
            &mut channel,
            &mut encoder,
            Message::Event(Event::Stopped { exit_code: Some(0) }),
        );
        transmit(&mut channel, &mut encoder, Message::Released);
        if mode == "clean-tail" {
            for _ in 0..EVENTS {
                transmit(
                    &mut channel,
                    &mut encoder,
                    Message::Event(Event::Mode { server: false }),
                );
            }
        }
        transmit(
            &mut channel,
            &mut encoder,
            Message::Event(Event::Terminal {
                terminal: TerminalId::Main,
                status: TerminalStatus {
                    attached: mode == "stopped",
                    connected: mode == "stopped",
                    complete: clean,
                    eof: clean,
                    ..Default::default()
                },
                error: None,
            }),
        );
    }
    if mode == "close-step-clean" {
        // Four final metadata frames plus these twelve fill one production
        // receive budget, leaving EOF for the next tick, after its write phase.
        for _ in 0..EVENTS - 4 {
            transmit(
                &mut channel,
                &mut encoder,
                Message::Event(Event::Mode { server: false }),
            );
        }
    }
    fs::write(root.join("ready"), "ready").unwrap();
    if mode == "clean" {
        std::process::exit(0);
    }
    let _startup_owner = startup; // Keep typed received leases until real exit.
    if mode.starts_with("close-") {
        // Model production's close-before-Actor/Engine-drop ordering. Only this
        // test helper pauses between control teardown and the actual exit.
        decoder.close(&mut channel);
        fs::write(root.join("closed"), "control closed, child still alive").unwrap();
        while Instant::now() < deadline {
            if root.join("check-alive").exists() {
                fs::write(root.join("alive"), "still returning").unwrap();
            }
            if mode != "close-stalled" && root.join("exit").exists() {
                let code = if mode == "close-nonzero" { 7 } else { 0 };
                fs::write(root.join("exit-observed"), code.to_string()).unwrap();
                std::process::exit(code);
            }
            thread::sleep(Duration::from_millis(1));
        }
        panic!("close-before-exit helper barrier timed out");
    }
    let mut report = false;
    while Instant::now() < deadline && !channel.closed() {
        if mode == "clean-barrier" || mode == "clean-tail" {
            if root.join("exit").exists() {
                std::process::exit(0);
            }
        } else if mode == "no-read" {
            if !report && root.join("report").exists() {
                transmit(
                    &mut channel,
                    &mut encoder,
                    Message::Event(Event::Mode { server: true }),
                );
                report = true;
            }
        } else if let Some(received) = decoder.receive_channel(&mut channel).unwrap() {
            match received.message {
                Message::Command(Command::Stop) => {
                    transmit(&mut channel, &mut encoder, Message::Stopping);
                    if mode == "ack-stop" {
                        transmit(
                            &mut channel,
                            &mut encoder,
                            Message::Event(Event::Stopped { exit_code: Some(0) }),
                        );
                        transmit(&mut channel, &mut encoder, Message::Released);
                        std::process::exit(0);
                    }
                }
                Message::Command(Command::Export { .. }) => {
                    fs::write(root.join("export"), "received").unwrap();
                }
                Message::Command(Command::Spawn { .. }) => {
                    fs::write(root.join("spawn"), "unexpected").unwrap();
                }
                _ => (),
            }
        }
        thread::sleep(Duration::from_millis(1));
    }
    std::process::exit(0);
}

#[test]
fn record_drops_are_counted_across_stop_failure_and_confirmed_reap() {
    let _forking = goblins_unix::forking();
    let root = goblins_unix::temp_directory().unwrap();
    let mut runtime = launch(&root, "hold");
    ready(&mut runtime, &root);
    let record = || Command::RecordEvent {
        event: "diagnostic".into(),
        fields: serde_json::json!({"exec":1}),
    };
    for _ in 0..17 {
        runtime.send(record()).unwrap();
    }
    assert_eq!(runtime.commands.unsent_requests(), 0);
    assert_eq!(runtime.commands.dropped_record_events(), 1);
    runtime.send(Command::Stop).unwrap();
    assert_eq!(runtime.commands.dropped_record_events(), 17);
    runtime.abort();
    runtime.send(record()).unwrap();
    assert_eq!(runtime.commands.dropped_record_events(), 18);
    let deadline = Instant::now() + Duration::from_secs(5);
    while !runtime.ended() {
        round(&mut runtime);
        assert!(Instant::now() < deadline);
        thread::sleep(Duration::from_millis(1));
    }
    assert_eq!(runtime.commands.dropped_record_events(), 18);
    runtime.send(record()).unwrap();
    assert_eq!(runtime.commands.dropped_record_events(), 19);
    assert!(runtime.ended());
    assert!(!root.join("session/resources").exists());
    drop(runtime);
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn c4_acceptance_has_legacy_listener_before_bootstrap() {
    let _forking = goblins_unix::forking();
    let root = goblins_unix::temp_directory().unwrap();
    let runtime = launch(&root, "hold");
    // No poll/bootstrap transmission or child readiness wait is allowed here:
    // the launch receipt already exposes this path to an immediate frontend.
    let stream = UnixStream::connect(root.join("session/terminal.sock")).unwrap();
    use std::os::unix::fs::PermissionsExt;
    assert_eq!(
        fs::metadata(root.join("session/terminal.sock"))
            .unwrap()
            .permissions()
            .mode()
            & 0o777,
        0o600
    );
    drop(stream);
    drop(runtime);
    assert!(!root.join("session/terminal.sock").exists());
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn c4_successful_final_exit_does_not_turn_late_write_failure_into_interruption() {
    let _forking = goblins_unix::forking();
    for failure in ["transport", "encoding", "protocol"] {
        let root = goblins_unix::temp_directory().unwrap();
        let mut runtime = launch(&root, "clean-barrier");
        let deadline = Instant::now() + Duration::from_secs(5);
        while !runtime.released() || !runtime.terminals[&TerminalId::Main].complete {
            round(&mut runtime);
            assert!(Instant::now() < deadline);
            thread::sleep(Duration::from_millis(1));
        }
        assert!(runtime.stopped && runtime.release_seen);
        assert!(runtime.terminals[&TerminalId::Main].complete);
        // Model the legitimate race between the receive and write phases of one
        // tick: final lifecycle was read, then the child exits before a late resize.
        runtime
            .send(Command::Resize {
                terminal: TerminalId::Main,
                rows: 24,
                cols: 80,
            })
            .unwrap();
        fs::write(root.join("exit"), "exit successfully").unwrap();
        while !runtime.process.as_ref().unwrap().child.exited().unwrap() {
            assert!(Instant::now() < deadline);
            thread::sleep(Duration::from_millis(1));
        }
        let error = runtime.write_control().unwrap_err();
        assert!(error.to_string().contains("Broken pipe"), "{error}");
        match failure {
            "transport" => runtime.fail_write(error, Instant::now()),
            "encoding" => runtime.fail_write(
                std::io::Error::new(std::io::ErrorKind::InvalidData, "invalid encoded control")
                    .into(),
                Instant::now(),
            ),
            "protocol" => runtime.fail("invalid incoming control".into()),
            _ => unreachable!(),
        }
        let mut interrupted = false;
        while !runtime.ended() {
            for event in round(&mut runtime) {
                if let Event::Terminal { status, .. } = event {
                    assert_eq!(
                        status.interrupted,
                        failure != "transport",
                        "only a transport write racing a successful final exit preserves completion"
                    );
                    interrupted |= status.interrupted;
                } else {
                    assert!(!matches!(event, Event::Failed(_)));
                }
            }
            assert!(Instant::now() < deadline);
        }
        assert_eq!(interrupted, failure != "transport");
        drop(runtime);
        fs::remove_dir_all(root).unwrap();
    }
}

fn waiting_closed_member(mode: &str) -> (PathBuf, Supervised) {
    let root = goblins_unix::temp_directory().unwrap();
    goblins_unix::private_directory(&root.join("session")).unwrap();
    let held = root.join("held");
    goblins_unix::private_directory(&held).unwrap();
    let handoff = goblins_runtime::Handoff {
        workspace: OwnedFd::from(fs::File::open(&held).unwrap()).into(),
        leases: vec![goblins_runtime::ResourceLease::new(held).unwrap()],
        working_directory: None,
        dev_shell: None,
        docker: false,
    };
    let source = Source::Member {
        scope: None,
        owner_scope: None,
        manifest: root.join("manifest"),
        configuration: "shell".into(),
        auto: Default::default(),
        state: root.clone(),
        history: None,
    };
    let mut runtime = Supervised::start(
        executable(&root, mode),
        source,
        root.join("session/resources"),
        (24, 80),
        false,
    )
    .unwrap();
    runtime.send(Command::Start(Box::new(handoff))).unwrap();
    runtime.write_control().unwrap();
    let deadline = Instant::now() + Duration::from_secs(5);
    while !root.join("closed").exists() {
        assert!(
            Instant::now() < deadline,
            "helper did not close control before exit"
        );
        thread::sleep(Duration::from_millis(1));
    }
    (root, runtime)
}

fn close_before_exit(
    mode: &str,
) -> (
    PathBuf,
    Supervised,
    Box<dyn std::error::Error + Send + Sync>,
) {
    let (root, mut runtime) = waiting_closed_member(mode);
    // Consume exactly the final metadata, leaving local Channel open until the
    // real write detects the peer closure. No pidfd-exit prewait is permitted.
    while !runtime.stopped
        || !runtime.release_seen
        || !runtime.terminals[&TerminalId::Main].complete
    {
        let received = runtime
            .decoder
            .receive_channel(&mut runtime.process.as_mut().unwrap().channel)
            .unwrap()
            .expect("complete final metadata was transmitted before closed marker");
        runtime
            .receive(received.message, received.adopted, Instant::now())
            .unwrap();
    }
    assert!(runtime.failure.is_none());
    assert!(!runtime.process.as_ref().unwrap().child.exited().unwrap());
    assert!(root.join("held").exists());
    assert!(root.join("session/terminal.sock").exists());
    assert!(
        !runtime
            .writer
            .drained(&runtime.process.as_ref().unwrap().channel)
    );
    assert!(!runtime.released());
    // These are the already delivered Mode, Stopped and complete terminal
    // snapshot. Subsequent assertions inspect only effects of the write race.
    assert_eq!(runtime.events.len(), 3);
    assert_eq!(
        runtime
            .events
            .iter()
            .filter(|e| matches!(e, Event::Stopped { exit_code: Some(0) }))
            .count(),
        1
    );
    assert_eq!(runtime.events.iter().filter(|e| matches!(e, Event::Terminal { status, .. } if status.complete && status.eof && !status.interrupted)).count(), 1);
    runtime.events.clear();
    runtime
        .send(Command::Resize {
            terminal: TerminalId::Main,
            rows: 24,
            cols: 80,
        })
        .unwrap();
    let error = runtime.write_control().unwrap_err();
    assert!(error.to_string().contains("Broken pipe"), "{error}");
    assert!(
        !runtime.process.as_ref().unwrap().child.exited().unwrap(),
        "BrokenPipe must precede the actual child exit"
    );
    (root, runtime, error)
}

fn finish_close_race(root: &Path, runtime: &mut Supervised, expect_interrupted: bool) {
    let deadline = Instant::now() + Duration::from_secs(5);
    let mut interrupted = false;
    while !runtime.ended() {
        for event in round(runtime) {
            match event {
                Event::Terminal { status, .. } => {
                    assert_eq!(status.interrupted, expect_interrupted);
                    if status.interrupted {
                        assert!(status.eof);
                    }
                    interrupted |= status.interrupted;
                }
                Event::Failed(_) => panic!("final stopped record must not be revived"),
                _ => (),
            }
        }
        assert!(Instant::now() < deadline, "closed child was not reaped");
        thread::sleep(Duration::from_millis(1));
    }
    assert_eq!(interrupted, expect_interrupted);
    assert!(!root.join("held").exists());
    assert!(!root.join("session/terminal.sock").exists());
}

#[test]
fn c4_close_before_exit_allows_successful_natural_return() {
    let _forking = goblins_unix::forking();
    let (root, mut runtime, error) = close_before_exit("close-clean");
    runtime.fail_write(error, Instant::now());
    assert!(
        root.join("held").exists(),
        "sender lease must survive until reap"
    );
    fs::write(root.join("exit"), "return successfully").unwrap();
    finish_close_race(&root, &mut runtime, false);
    assert_eq!(fs::read_to_string(root.join("exit-observed")).unwrap(), "0");
    drop(runtime);
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn c4_close_before_exit_unsuccessful_return_remains_interrupted() {
    let _forking = goblins_unix::forking();
    let (root, mut runtime, error) = close_before_exit("close-nonzero");
    runtime.fail_write(error, Instant::now());
    assert!(root.join("held").exists());
    assert_eq!(
        runtime
            .process
            .as_ref()
            .unwrap()
            .child
            .exit_status_for_test()
            .unwrap(),
        None,
        "a still-running child has no exit result"
    );
    fs::write(root.join("exit"), "return unsuccessfully").unwrap();
    let deadline = Instant::now() + Duration::from_secs(5);
    while !runtime.process.as_ref().unwrap().child.exited().unwrap() {
        assert!(
            Instant::now() < deadline,
            "child did not return unsuccessfully"
        );
        thread::sleep(Duration::from_millis(1));
    }
    assert_eq!(
        runtime
            .process
            .as_ref()
            .unwrap()
            .child
            .exit_status_for_test()
            .unwrap(),
        Some((libc::CLD_EXITED, 7)),
        "the kernel must report natural exit 7, not an intervening kill"
    );
    assert!(
        root.join("held").exists(),
        "WNOWAIT must leave sender ownership for ordinary reap"
    );
    assert!(root.join("session/terminal.sock").exists());
    finish_close_race(&root, &mut runtime, true);
    assert_eq!(fs::read_to_string(root.join("exit-observed")).unwrap(), "7");
    drop(runtime);
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn c4_close_before_exit_stall_is_bounded_without_early_kill() {
    let _forking = goblins_unix::forking();
    let (root, mut runtime, error) = close_before_exit("close-stalled");
    let started = Instant::now();
    runtime.fail_write(error, started);
    let expires = started + EXIT_GRACE;
    assert_eq!(runtime.exit_grace, Some(expires));
    // Stopped disabled both historical per-handle Stop deadlines. They must
    // not be resurrected to shorten this new, distinct natural-return grace.
    runtime.stop = Some(started - Duration::from_secs(90));
    runtime.stopping = Some(started - Duration::from_secs(89));
    fs::write(root.join("check-alive"), "prove no premature kill").unwrap();
    let deadline = started + Duration::from_secs(5);
    while !root.join("alive").exists() {
        assert!(
            Instant::now() < deadline,
            "eligible final child was killed before natural-return grace"
        );
        thread::sleep(Duration::from_millis(1));
    }
    runtime.step(expires - Duration::from_nanos(1));
    assert!(!runtime.process.as_ref().unwrap().child.exited().unwrap());
    assert!(root.join("held").exists());
    assert!(root.join("session/terminal.sock").exists());
    assert!(!runtime.released());
    runtime.fail_write(
        std::io::Error::from(std::io::ErrorKind::BrokenPipe).into(),
        expires - Duration::from_nanos(1),
    );
    assert_eq!(
        runtime.exit_grace,
        Some(expires),
        "another transport error cannot reset the grace"
    );
    // Drive the existing monotonic poll clock to the exact original expiry.
    // The failure already stored by fail_write must not disable this check.
    runtime.step(expires);
    assert!(!runtime.write_failure && runtime.exit_grace.is_none());
    finish_close_race(&root, &mut runtime, true);
    drop(runtime);
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn c4_close_before_exit_real_step_exhausts_read_budget_before_write() {
    let _forking = goblins_unix::forking();
    let (root, mut runtime) = waiting_closed_member("close-step-clean");
    assert!(!runtime.process.as_ref().unwrap().child.exited().unwrap());
    assert!(root.join("held").exists());
    assert!(
        !runtime
            .writer
            .drained(&runtime.process.as_ref().unwrap().channel)
    );
    runtime
        .send(Command::Resize {
            terminal: TerminalId::Main,
            rows: 24,
            cols: 80,
        })
        .unwrap();
    let now = Instant::now();
    // Exercise the complete production receive/write/reap tick. Sixteen
    // messages include Released, so fifteen Events are owned at its boundary.
    runtime.step(now);
    assert_eq!(runtime.events.len(), EVENTS - 1);
    assert!(runtime.stopped && runtime.release_seen);
    assert!(runtime.terminals[&TerminalId::Main].complete);
    assert!(runtime.failure.as_ref().unwrap().contains("Broken pipe"));
    assert_eq!(runtime.exit_grace, Some(now + EXIT_GRACE));
    assert!(!runtime.process.as_ref().unwrap().child.exited().unwrap());
    assert!(root.join("held").exists());
    assert!(root.join("session/terminal.sock").exists());
    assert!(!runtime.released());
    fs::write(root.join("exit"), "return successfully after the real tick").unwrap();
    finish_close_race(&root, &mut runtime, false);
    assert_eq!(fs::read_to_string(root.join("exit-observed")).unwrap(), "0");
    drop(runtime);
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn c4_close_before_exit_shared_abort_cuts_grace_short() {
    let _forking = goblins_unix::forking();
    let (root, mut runtime, error) = close_before_exit("close-stalled");
    runtime.fail_write(error, Instant::now());
    assert!(runtime.exit_grace.is_some());
    assert!(root.join("held").exists());
    // Controller's shared shutdown deadline calls this existing Runtime hook.
    runtime.abort();
    assert!(runtime.exit_grace.is_none());
    assert!(!runtime.write_failure);
    let deadline = Instant::now() + Duration::from_secs(5);
    while !runtime.process.as_ref().unwrap().child.exited().unwrap() {
        assert!(Instant::now() < deadline);
        thread::sleep(Duration::from_millis(1));
    }
    assert_eq!(
        runtime
            .process
            .as_ref()
            .unwrap()
            .child
            .exit_status_for_test()
            .unwrap(),
        Some((libc::CLD_KILLED, libc::SIGKILL)),
        "shared abort must be distinguished from natural exit"
    );
    assert!(root.join("held").exists());
    finish_close_race(&root, &mut runtime, true);
    drop(runtime);
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn c4_close_before_exit_observed_natural_exit_wins_over_late_poll() {
    let _forking = goblins_unix::forking();
    let (root, mut runtime, error) = close_before_exit("close-clean");
    let now = Instant::now();
    runtime.fail_write(error, now);
    fs::write(root.join("exit"), "return before next poll").unwrap();
    let deadline = now + Duration::from_secs(5);
    while !runtime.process.as_ref().unwrap().child.exited().unwrap() {
        assert!(Instant::now() < deadline);
        thread::sleep(Duration::from_millis(1));
    }
    // The child is already gone; a late poll cannot manufacture a timeout.
    runtime.step(now + EXIT_GRACE);
    finish_close_race(&root, &mut runtime, false);
    assert_eq!(fs::read_to_string(root.join("exit-observed")).unwrap(), "0");
    drop(runtime);
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn c4_close_before_exit_grace_never_forgives_ineligible_failures() {
    let _forking = goblins_unix::forking();
    for kind in [
        "encoding",
        "protocol",
        "unresolved",
        "missing-release",
        "earlier-failure",
    ] {
        let (root, mut runtime, error) = close_before_exit("close-stalled");
        match kind {
            "encoding" => runtime.fail_write(
                std::io::Error::new(std::io::ErrorKind::InvalidData, "invalid encoding").into(),
                Instant::now(),
            ),
            "protocol" => runtime.fail("invalid incoming protocol".into()),
            other => {
                match other {
                    "unresolved" => {
                        runtime
                            .terminals
                            .get_mut(&TerminalId::Main)
                            .unwrap()
                            .complete = false
                    }
                    "missing-release" => runtime.release_seen = false,
                    "earlier-failure" => runtime.failure = Some("earlier failure".into()),
                    _ => unreachable!(),
                }
                runtime.fail_write(error, Instant::now());
            }
        }
        assert!(runtime.exit_grace.is_none(), "{kind}");
        assert!(!runtime.write_failure, "{kind}");
        if kind == "earlier-failure" {
            assert_eq!(runtime.failure.as_deref(), Some("earlier failure"));
        }
        assert!(root.join("held").exists());
        finish_close_race(&root, &mut runtime, true);
        drop(runtime);
        fs::remove_dir_all(root).unwrap();
    }
}

#[test]
fn failed_spawn_and_abandoned_member_unlink_the_prebound_listener() {
    let _forking = goblins_unix::forking();
    let root = goblins_unix::temp_directory().unwrap();
    let path = root.join("terminal.sock");
    let missing = Executable::from_path(Some(root.join("does-not-exist")));
    assert!(
        Supervised::start(
            missing.clone(),
            source(&root),
            root.join("resources"),
            (24, 80),
            false
        )
        .is_err()
    );
    assert!(!path.exists());
    let existing = std::os::unix::net::UnixListener::bind(&path).unwrap();
    assert!(
        Supervised::start(
            missing.clone(),
            source(&root),
            root.join("resources"),
            (24, 80),
            false
        )
        .is_err()
    );
    assert!(path.exists(), "bind failure must preserve another listener");
    drop(UnixStream::connect(&path).unwrap());
    for (directory, dimensions) in [
        (PathBuf::from("relative/resources"), (24, 80)),
        (root.join("resources"), (0, 80)),
    ] {
        let error = Supervised::start(missing.clone(), source(&root), directory, dimensions, false)
            .err()
            .unwrap();
        assert!(
            error
                .to_string()
                .contains("invalid supervisor startup parameters")
        );
    }
    drop(existing);
    fs::remove_file(&path).unwrap();
    let member = || Source::Member {
        scope: None,
        owner_scope: None,
        manifest: root.join("manifest"),
        configuration: "shell".into(),
        auto: Default::default(),
        state: root.clone(),
        history: None,
    };
    let mut runtime = Supervised::start(
        missing.clone(),
        member(),
        root.join("resources"),
        (24, 80),
        false,
    )
    .unwrap();
    assert!(path.exists());
    runtime
        .send(Command::Start(Box::new(goblins_runtime::Handoff {
            workspace: OwnedFd::from(fs::File::open(&root).unwrap()).into(),
            leases: vec![],
            working_directory: None,
            dev_shell: None,
            docker: false,
        })))
        .unwrap();
    assert!(runtime.process.is_none());
    assert!(!path.exists());
    assert!(matches!(
        round(&mut runtime).as_slice(),
        [Event::Failed(_), Event::Stopped { exit_code: None }]
    ));
    assert!(runtime.ended());
    drop(runtime);
    let runtime =
        Supervised::start(missing, member(), root.join("resources"), (24, 80), false).unwrap();
    assert!(path.exists());
    drop(runtime);
    assert!(!path.exists());
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn transport_write_failure_cannot_hide_an_undelivered_terminal_outcome() {
    let _forking = goblins_unix::forking();
    let root = goblins_unix::temp_directory().unwrap();
    let mut runtime = launch(&root, "clean-tail");
    runtime.step(Instant::now()); // Transmit bootstrap, without draining final events.
    let deadline = Instant::now() + Duration::from_secs(5);
    while !root.join("ready").exists() {
        assert!(Instant::now() < deadline);
        thread::sleep(Duration::from_millis(1));
    }
    runtime.step(Instant::now());
    assert!(runtime.stopped && runtime.release_seen);
    // Released uses a receive slot without adding an event. The next bounded
    // round fills the event queue while leaving terminal completion unread.
    runtime.step(Instant::now());
    assert_eq!(runtime.events.len(), EVENTS);
    assert!(!runtime.terminals[&TerminalId::Main].complete);
    runtime
        .send(Command::Resize {
            terminal: TerminalId::Main,
            rows: 24,
            cols: 80,
        })
        .unwrap();
    fs::write(root.join("exit"), "exit successfully").unwrap();
    while !runtime.process.as_ref().unwrap().child.exited().unwrap() {
        assert!(Instant::now() < deadline);
        thread::sleep(Duration::from_millis(1));
    }
    let error = runtime.write_control().unwrap_err();
    assert!(error.to_string().contains("Broken pipe"), "{error}");
    runtime.fail_write(error, Instant::now());
    let mut interrupted = false;
    while !runtime.ended() {
        for event in round(&mut runtime) {
            if let Event::Terminal {
                terminal: TerminalId::Main,
                status,
                ..
            } = event
            {
                assert!(status.interrupted && status.eof);
                interrupted = true;
            }
        }
        assert!(Instant::now() < deadline);
    }
    assert!(
        interrupted,
        "a lost completion must not leave the frontend waiting forever"
    );
    drop(runtime);
    assert!(!root.join("session/terminal.sock").exists());
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn member_placeholder_holds_scope_and_stop_never_spawns() {
    let _no_forks = goblins_unix::no_forks();
    for (command, expected) in [(Command::Stop, None), (Command::ScopeFailed, Some(1))] {
        let root = goblins_unix::temp_directory().unwrap();
        let (scope, mut observer) = UnixStream::pair().unwrap();
        observer.set_nonblocking(true).unwrap();
        let source = Source::Member {
            scope: Some(OwnedFd::from(scope).into()),
            owner_scope: None,
            manifest: root.join("manifest"),
            configuration: "shell".into(),
            auto: Default::default(),
            state: root.clone(),
            history: None,
        };
        let mut runtime = Supervised::start(
            executable(&root, "hold"),
            source,
            root.join("resources"),
            (24, 80),
            false,
        )
        .unwrap();
        assert!(runtime.process.is_none());
        assert!(!runtime.released());
        let listener = root.join("terminal.sock");
        let stream = UnixStream::connect(&listener).unwrap();
        assert!(!root.join("pid").exists());
        use std::io::Read;
        assert_eq!(
            observer.read(&mut [0]).unwrap_err().kind(),
            std::io::ErrorKind::WouldBlock
        );
        runtime.send(command).unwrap();
        assert!(!listener.exists());
        assert!(UnixStream::connect(&listener).is_err());
        drop(stream);
        assert!(
            matches!(round(&mut runtime).as_slice(), [Event::Stopped { exit_code }] if *exit_code == expected)
        );
        assert!(runtime.ended());
        runtime.send(Command::Stop).unwrap();
        assert!(round(&mut runtime).is_empty());
        assert!(runtime.released() && runtime.ended());
        assert!(!root.join("pid").exists());
        assert_eq!(observer.read(&mut [0]).unwrap(), 0);
        drop(runtime);
        fs::remove_dir_all(root).unwrap();
    }
}

pub(crate) fn unacked_member(root: &Path, scope: Option<goblins_runtime::Fd>) -> Supervised {
    let resources = root.join("resources");
    goblins_unix::private_directory(&resources).unwrap();
    let handoff = goblins_runtime::Handoff {
        workspace: OwnedFd::from(fs::File::open(&resources).unwrap()).into(),
        leases: vec![goblins_runtime::ResourceLease::new(resources).unwrap()],
        working_directory: None,
        dev_shell: None,
        docker: false,
    };
    let source = Source::Member {
        scope,
        owner_scope: None,
        manifest: root.join("manifest"),
        configuration: "shell".into(),
        auto: Default::default(),
        state: root.into(),
        history: None,
    };
    goblins_unix::private_directory(&root.join("runtime")).unwrap();
    let mut runtime = Supervised::start(
        executable(root, "unacked"),
        source,
        root.join("runtime/resources"),
        (24, 80),
        false,
    )
    .unwrap();
    runtime.send(Command::Start(Box::new(handoff))).unwrap();
    ready(&mut runtime, root);
    runtime
}

#[test]
fn startup_sender_ownership_survives_until_confirmed_child_death() {
    let _forking = goblins_unix::forking();
    let root = goblins_unix::temp_directory().unwrap();
    let (scope, mut observer) = UnixStream::pair().unwrap();
    observer.set_nonblocking(true).unwrap();
    let mut runtime = unacked_member(&root, Some(OwnedFd::from(scope).into()));
    assert!(
        !runtime
            .writer
            .drained(&runtime.process.as_ref().unwrap().channel)
    );
    runtime
        .process
        .as_ref()
        .unwrap()
        .child
        .signal_for_test(libc::SIGSTOP);
    runtime.abort();
    assert!(
        root.join("resources").exists(),
        "sender guard must remain before reap"
    );
    assert!(!runtime.released());
    assert!(root.join("runtime/terminal.sock").exists());
    let deadline = Instant::now() + Duration::from_secs(5);
    let mut failed = 0;
    while !runtime.ended() {
        failed += round(&mut runtime)
            .iter()
            .filter(|e| matches!(e, Event::Failed(_)))
            .count();
        assert!(Instant::now() < deadline);
        thread::sleep(Duration::from_millis(1));
    }
    assert_eq!(failed, 1);
    assert!(!root.join("resources").exists());
    assert!(!root.join("runtime/terminal.sock").exists());
    use std::io::Read;
    assert_eq!(
        observer.read(&mut [0]).unwrap(),
        0,
        "scope held by sender and receiver must close after exit"
    );
    drop(runtime);
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn blocked_command_output_does_not_block_supervisor_event_reads() {
    let _forking = goblins_unix::forking();
    let root = goblins_unix::temp_directory().unwrap();
    let mut runtime = launch(&root, "no-read");
    ready(&mut runtime, &root);
    let size: libc::c_int = 4096;
    assert_eq!(
        unsafe {
            libc::setsockopt(
                runtime.process.as_ref().unwrap().channel.fd().unwrap(),
                libc::SOL_SOCKET,
                libc::SO_SNDBUF,
                (&size as *const libc::c_int).cast(),
                std::mem::size_of_val(&size) as _,
            )
        },
        0
    );
    runtime
        .send(Command::SandboxSend {
            connection: 1,
            message: goblins_jsonrpc::result(
                serde_json::json!(1),
                serde_json::json!("x".repeat(512 * 1024)),
            ),
        })
        .unwrap();
    round(&mut runtime);
    assert!(runtime.process.as_ref().unwrap().channel.wants_write());
    fs::write(root.join("report"), "send reverse event").unwrap();
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let messages = round(&mut runtime);
        assert!(runtime.process.as_ref().unwrap().channel.wants_write());
        if messages
            .iter()
            .any(|e| matches!(e, Event::Mode { server: true }))
        {
            break;
        }
        assert!(Instant::now() < deadline);
        thread::sleep(Duration::from_millis(1));
    }
    drop(runtime);
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn sigcont_before_deadline_drains_earlier_answers_then_acknowledges_stop() {
    let _forking = goblins_unix::forking();
    let root = goblins_unix::temp_directory().unwrap();
    let mut runtime = launch(&root, "ack-stop");
    ready(&mut runtime, &root);
    runtime
        .process
        .as_ref()
        .unwrap()
        .child
        .signal_for_test(libc::SIGSTOP);
    let size: libc::c_int = 4096;
    assert_eq!(
        unsafe {
            libc::setsockopt(
                runtime.process.as_ref().unwrap().channel.fd().unwrap(),
                libc::SOL_SOCKET,
                libc::SO_SNDBUF,
                (&size as *const libc::c_int).cast(),
                std::mem::size_of_val(&size) as _,
            )
        },
        0
    );
    runtime
        .send(Command::SandboxSend {
            connection: 1,
            message: goblins_jsonrpc::result(
                serde_json::json!(1),
                serde_json::json!("x".repeat(512 * 1024)),
            ),
        })
        .unwrap();
    for approval in 0..4 {
        runtime
            .send(Command::Preview {
                approval,
                package: "hello".into(),
            })
            .unwrap();
    }
    assert!(
        runtime
            .send(Command::Preview {
                approval: 5,
                package: "hello".into()
            })
            .is_err()
    );
    runtime.send(Command::Cancel(0)).unwrap();
    runtime.send(Command::Stop).unwrap();
    round(&mut runtime);
    assert!(runtime.process.as_ref().unwrap().channel.wants_write());
    runtime
        .process
        .as_ref()
        .unwrap()
        .child
        .signal_for_test(libc::SIGCONT);
    let deadline = Instant::now() + Duration::from_secs(5);
    let mut stopped = 0;
    while !runtime.ended() {
        for event in round(&mut runtime) {
            match event {
                Event::Stopped { .. } => stopped += 1,
                Event::Failed(e) => panic!("resumed supervisor failed: {e}"),
                _ => (),
            }
        }
        assert!(Instant::now() < deadline);
        thread::sleep(Duration::from_millis(1));
    }
    assert!(runtime.stopping.is_some());
    assert_eq!(stopped, 1);
    drop(runtime);
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn stop_deadlines_start_at_send_and_keep_a_separate_cleanup_budget() {
    let _forking = goblins_unix::forking();
    for acknowledged in [false, true] {
        let root = goblins_unix::temp_directory().unwrap();
        let mut runtime = launch(&root, "hold");
        ready(&mut runtime, &root);
        runtime.send(Command::Stop).unwrap();
        let start = runtime.stop.unwrap();
        if acknowledged {
            let deadline = Instant::now() + Duration::from_secs(5);
            while runtime.stopping.is_none() {
                round(&mut runtime);
                assert!(Instant::now() < deadline);
                thread::sleep(Duration::from_millis(1));
            }
        } else {
            runtime
                .process
                .as_ref()
                .unwrap()
                .child
                .signal_for_test(libc::SIGSTOP);
        }
        runtime.step(start + ACK_TIMEOUT + Duration::from_millis(1));
        assert_eq!(runtime.failure.is_some() || runtime.failed, !acknowledged);
        if acknowledged {
            let acknowledged_at = runtime.stopping.unwrap();
            // A duplicate notification must not buy another cleanup budget.
            runtime
                .receive(
                    Message::Stopping,
                    None,
                    acknowledged_at + Duration::from_secs(10),
                )
                .unwrap();
            assert_eq!(runtime.stopping, Some(acknowledged_at));
            let cleanup_deadline = acknowledged_at + CLEANUP_TIMEOUT;
            assert!(
                cleanup_deadline < start + SHUTDOWN_TIMEOUT,
                "early ACK must not get extra cleanup time"
            );
            runtime.step(cleanup_deadline - Duration::from_millis(1));
            assert!(
                runtime.failure.is_none() && !runtime.failed,
                "cleanup budget must last the full 15 seconds after ACK"
            );
            runtime.step(cleanup_deadline);
            assert!(
                runtime
                    .failure
                    .as_ref()
                    .is_some_and(|e| e.contains("cleanup"))
                    || runtime.events.iter().any(
                        |e| matches!(e, Event::Failed(message) if message.contains("cleanup"))
                    )
            );
        }
        let deadline = Instant::now() + Duration::from_secs(5);
        let mut failed = 0;
        while !runtime.ended() {
            failed += round(&mut runtime)
                .iter()
                .filter(|e| matches!(e, Event::Failed(_)))
                .count();
            assert!(Instant::now() < deadline);
            thread::sleep(Duration::from_millis(1));
        }
        assert_eq!(failed, 1);
        let pid = fs::read_to_string(root.join("pid")).unwrap();
        assert!(
            !Path::new(&format!("/proc/{pid}")).exists(),
            "child was not reaped"
        );
        drop(runtime);
        fs::remove_dir_all(root).unwrap();
    }
}

#[test]
fn clean_exit_after_stopped_is_reaped_without_interrupting_terminal_outcome() {
    let _forking = goblins_unix::forking();
    let root = goblins_unix::temp_directory().unwrap();
    let mut runtime = launch(&root, "clean");
    let deadline = Instant::now() + Duration::from_secs(5);
    let mut stopped = 0;
    while !runtime.ended() {
        for event in round(&mut runtime) {
            match event {
                Event::Stopped { exit_code: Some(0) } => stopped += 1,
                Event::Failed(e) => panic!("clean exit failed: {e}"),
                Event::Terminal { status, .. } => assert!(!status.interrupted),
                _ => (),
            }
        }
        assert!(Instant::now() < deadline);
        thread::sleep(Duration::from_millis(1));
    }
    assert_eq!(stopped, 1);
    let pid = fs::read_to_string(root.join("pid")).unwrap();
    assert!(!Path::new(&format!("/proc/{pid}")).exists());
    drop(runtime);
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn killed_stopped_supervisor_only_interrupts_terminal_and_release_stays_monotonic() {
    let _forking = goblins_unix::forking();
    let root = goblins_unix::temp_directory().unwrap();
    let mut runtime = launch(&root, "stopped");
    let deadline = Instant::now() + Duration::from_secs(5);
    let mut healthy_terminals = 0;
    while !runtime.released() || healthy_terminals == 0 {
        for event in round(&mut runtime) {
            if let Event::Terminal { status, .. } = event {
                assert!(
                    status.connected
                        && status.attached
                        && !status.interrupted
                        && !status.complete
                        && !status.detached,
                    "unexpected healthy retained status: {status:?}"
                );
                healthy_terminals += 1;
            }
        }
        assert!(Instant::now() < deadline);
        thread::sleep(Duration::from_millis(1));
    }
    assert_eq!(healthy_terminals, 1);
    assert!(!runtime.ended());
    let now = Instant::now();
    runtime.stop = Some(now - Duration::from_secs(90));
    runtime.stopping = Some(now - Duration::from_secs(89));
    runtime.step(now);
    assert!(runtime.failure.is_none() && runtime.exit_grace.is_none());
    assert!(!runtime.process.as_ref().unwrap().child.exited().unwrap());
    assert!(!runtime.terminals[&TerminalId::Main].complete);
    assert!(
        runtime.released(),
        "healthy stopped output retention stays released"
    );
    assert!(
        round(&mut runtime).is_empty(),
        "healthy status must already be delivered before abort"
    );
    runtime.abort();
    let mut interrupted = 0;
    while !runtime.ended() {
        for event in round(&mut runtime) {
            match event {
                Event::Failed(e) => panic!("stopped record revived as failed: {e}"),
                Event::Stopped { .. } => panic!("duplicate lifecycle"),
                Event::Terminal { status, .. } => {
                    assert!(
                        status.interrupted
                            && status.eof
                            && !status.connected
                            && !status.attached
                            && !status.detached,
                        "unexpected post-abort terminal status: {status:?}"
                    );
                    interrupted += 1;
                }
                _ => (),
            }
        }
        assert!(runtime.released());
        assert!(Instant::now() < deadline);
        thread::sleep(Duration::from_millis(1));
    }
    assert_eq!(interrupted, 1);
    drop(runtime);
    fs::remove_dir_all(root).unwrap();
}
