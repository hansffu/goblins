use super::*;
use crate::supervised::tests::executable;
use goblins_runtime::supervisor::Received;
use std::{fs, thread, time::Duration};

pub(crate) fn peer(root: &Path, mode: &str, request: Received, mut channel: Channel) {
    let Message::Preflight { path, sequence, .. } = request.message else {
        panic!("preflight request")
    };
    fs::write(root.join("pid"), std::process::id().to_string()).unwrap();
    let mut writer = Writer::default();
    let mut encoder = Encoder::default();
    if mode.starts_with("preflight-handoff-") {
        wrong_direction_handoff(root, mode, channel, encoder);
        return;
    }
    if mode == "preflight-eof" {
        return;
    }
    if mode == "preflight-slow" {
        thread::sleep(Duration::from_millis(2100));
    }
    let message = if matches!(mode, "preflight-error" | "preflight-error-nonzero") {
        Message::PreflightFailed {
            error: "original Nix diagnostic".into(),
        }
    } else {
        let root_link = root.join("lease-root");
        let mut lease = ManifestLease::begin_root(path.clone(), root_link.clone()).unwrap();
        std::os::unix::fs::symlink("/immutable/fixture-output", &root_link).unwrap();
        lease.finish_root().unwrap();
        Message::Event(Event::Manifest {
            sequence: if mode == "preflight-sequence" {
                sequence + 1
            } else {
                sequence
            },
            lease,
        })
    };
    writer
        .begin(encoder.prepare(message).unwrap())
        .ok()
        .unwrap();
    while !writer.idle(&channel) {
        writer.pump(&mut channel).unwrap();
    }
    if mode == "preflight-duplicate" {
        writer
            .begin(
                encoder
                    .prepare(Message::PreflightFailed {
                        error: "extra".into(),
                    })
                    .unwrap(),
            )
            .ok()
            .unwrap();
        while !writer.idle(&channel) {
            writer.pump(&mut channel).unwrap();
        }
    }
    if mode == "preflight-partial" {
        let bytes = b"Content-Length: 100\r\n";
        assert_eq!(
            unsafe {
                libc::send(
                    channel.fd().unwrap(),
                    bytes.as_ptr().cast(),
                    bytes.len(),
                    libc::MSG_NOSIGNAL,
                )
            },
            bytes.len() as isize
        );
    }
    if mode == "preflight-error-nonzero" {
        std::process::exit(7);
    }
    if mode == "preflight-error" {
        channel.close();
        thread::sleep(Duration::from_millis(100));
        return;
    }
    let mut decoder = Decoder::default();
    let mut acked = false;
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        if Instant::now() >= deadline {
            panic!("parent neither acknowledged nor closed");
        }
        if let Some(received) = decoder.receive_channel(&mut channel).unwrap() {
            let Message::Adopted { transfer } = received.message else {
                panic!("expected Adopted")
            };
            writer.adopted(transfer).unwrap();
            acked = true;
            break;
        }
        if channel.closed() {
            break;
        }
        thread::sleep(Duration::from_millis(1));
    }
    if mode == "preflight-sequence" {
        // Release the sender's lock; the invalid received typed lease must
        // survive while this actual child is still alive, through parent reap.
        writer.close(&mut channel);
        thread::sleep(Duration::from_millis(100));
        fs::write(
            root.join("owned-before-exit"),
            fs::symlink_metadata(root.join("lease-root"))
                .is_ok()
                .to_string(),
        )
        .unwrap();
        return;
    }
    if mode == "preflight-hang" {
        assert!(acked);
        fs::write(root.join("ack"), "flushed").unwrap();
        channel.close();
        thread::sleep(Duration::from_secs(30));
    }
    if mode == "preflight-nonzero" {
        assert!(acked);
        std::process::exit(7);
    }
    if matches!(mode, "preflight-success" | "preflight-slow") {
        assert!(acked);
        // Make EOF observable while the actual child is still alive. An
        // empty writer must not manufacture BrokenPipe during this interval.
        channel.close();
        thread::sleep(Duration::from_millis(100));
    }
}

fn assert_reaped(root: &Path) {
    let pid: i32 = fs::read_to_string(root.join("pid"))
        .unwrap()
        .parse()
        .unwrap();
    let mut status = 0;
    assert_eq!(
        unsafe { libc::waitpid(pid, &mut status, libc::WNOHANG) },
        -1
    );
    assert_eq!(
        std::io::Error::last_os_error().raw_os_error(),
        Some(libc::ECHILD)
    );
    assert_eq!(unsafe { libc::kill(pid, 0) }, -1);
    assert_eq!(
        std::io::Error::last_os_error().raw_os_error(),
        Some(libc::ESRCH)
    );
}
#[test]
fn a_preflight_lease_is_published_only_after_ack_eof_and_successful_reap() {
    let _fork = goblins_unix::forking();
    let root = goblins_unix::temp_directory().unwrap();
    let path = root.join("manifest");
    let lease = lease(&executable(&root, "preflight-success"), &path, &root, 42).unwrap();
    assert_reaped(&root);
    assert_eq!(lease.path, path);
    assert!(root.join("lease-root").is_symlink());
    drop(lease);
    assert!(!root.join("lease-root").is_symlink());
    fs::remove_dir_all(root).unwrap();
}
#[test]
fn a_preflight_operation_error_preserves_its_original_diagnostic_after_reap() {
    let _fork = goblins_unix::forking();
    let root = goblins_unix::temp_directory().unwrap();
    let error = lease(
        &executable(&root, "preflight-error"),
        &root.join("manifest"),
        &root,
        0,
    )
    .unwrap_err();
    assert_eq!(error.to_string(), "original Nix diagnostic");
    assert_reaped(&root);
    fs::remove_dir_all(root).unwrap();
}
#[test]
fn invalid_preflight_transcripts_reject_and_reap_before_releasing_typed_owners() {
    let _fork = goblins_unix::forking();
    for (mode, error) in [
        ("preflight-sequence", "invalid preflight result"),
        ("preflight-duplicate", "extra preflight result"),
        ("preflight-partial", "unexpected end of file"),
        ("preflight-nonzero", "unsuccessfully"),
        ("preflight-error-nonzero", "unsuccessfully"),
        ("preflight-eof", "without a result"),
    ] {
        let root = goblins_unix::temp_directory().unwrap();
        let result =
            lease(&executable(&root, mode), &root.join("manifest"), &root, 42).unwrap_err();
        assert!(result.to_string().contains(error), "{mode}: {result}");
        assert_reaped(&root);
        if mode == "preflight-sequence" {
            assert_eq!(
                fs::read_to_string(root.join("owned-before-exit")).unwrap(),
                "true"
            );
        }
        assert!(
            !root.join("lease-root").is_symlink(),
            "{mode} leaked a root"
        );
        fs::remove_dir_all(root).unwrap();
    }
}
#[test]
fn preflight_result_deadline_covers_ack_eof_and_reap_without_a_second_grace() {
    let _fork = goblins_unix::forking();
    let root = goblins_unix::temp_directory().unwrap();
    let started = Instant::now();
    let error = lease(
        &executable(&root, "preflight-hang"),
        &root.join("manifest"),
        &root,
        0,
    )
    .unwrap_err();
    assert!(error.to_string().contains("within 2 seconds"), "{error}");
    let elapsed = started.elapsed();
    assert!(
        elapsed >= Duration::from_secs(2) && elapsed < Duration::from_secs(3),
        "{elapsed:?}"
    );
    assert_eq!(fs::read_to_string(root.join("ack")).unwrap(), "flushed");
    assert_reaped(&root);
    assert!(!root.join("lease-root").is_symlink());
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn preflight_work_before_a_result_has_no_two_second_operation_limit() {
    let _fork = goblins_unix::forking();
    let root = goblins_unix::temp_directory().unwrap();
    let started = Instant::now();
    let lease = lease(
        &executable(&root, "preflight-slow"),
        &root.join("manifest"),
        &root,
        0,
    )
    .unwrap();
    assert!(started.elapsed() >= Duration::from_millis(2100));
    assert_reaped(&root);
    drop(lease);
    assert!(!root.join("lease-root").is_symlink());
    fs::remove_dir_all(root).unwrap();
}

pub(crate) fn wrong_direction_handoff(
    root: &Path,
    mode: &str,
    mut channel: Channel,
    mut encoder: Encoder,
) {
    use goblins_runtime::{Command, Handoff, ResourceLease, Source, supervisor::SupervisorStart};
    let resources = root.join("unexpected-resources");
    goblins_unix::private_directory(&resources).unwrap();
    let handoff = Box::new(Handoff {
        leases: vec![ResourceLease::new(resources.clone()).unwrap()],
        workspace: std::os::fd::OwnedFd::from(fs::File::open(&resources).unwrap()).into(),
        working_directory: None,
        dev_shell: None,
        docker: false,
    });
    let message = match mode {
        "preflight-handoff-start" => Message::Command(Command::Start(handoff)),
        "preflight-handoff-export" => Message::Event(Event::Exported {
            member: "unexpected".into(),
            result: Ok(handoff),
        }),
        "preflight-handoff-bootstrap" => Message::SupervisorStart(SupervisorStart {
            supervisor_api: supervisor::API,
            source: Source::Member {
                scope: None,
                owner_scope: None,
                manifest: root.join("manifest"),
                configuration: "unexpected".into(),
                auto: Default::default(),
                state: root.into(),
                history: None,
            },
            directory: root.into(),
            dimensions: (24, 80),
            detached: false,
            payload_nofile: None,
            legacy_listener: None,
            handoff: Some(handoff),
        }),
        _ => unreachable!(),
    };
    let mut prepared = encoder.prepare(message).unwrap();
    assert_eq!(prepared.frames.len(), 1);
    let mut frame = prepared.frames.pop_front().unwrap();
    let h = if mode == "preflight-handoff-export" {
        &mut frame.value["params"]["result"]["Ok"]
    } else {
        &mut frame.value["params"]["handoff"]
    };
    assert_eq!(h["leases"].as_array().unwrap().len(), 1);
    // A valid first multipart frame whose declared second lease never arrives.
    // The general decoder would retain typed pending owners and return None.
    h["count"] = serde_json::json!(2);
    h["more"] = serde_json::json!(true);
    assert_eq!(frame.fds.len(), 2);
    channel.queue(&frame.value, &mut frame.fds).unwrap();
    while channel.wants_write() {
        channel.flush().unwrap();
    }
    let mut decoder = Decoder::default();
    let deadline = Instant::now() + Duration::from_secs(5);
    while !channel.closed() {
        assert!(
            Instant::now() < deadline,
            "wrong direction was not rejected promptly"
        );
        assert!(
            decoder.receive_channel(&mut channel).unwrap().is_none(),
            "invalid handoff was acknowledged"
        );
        thread::sleep(Duration::from_millis(1));
    }
    assert!(
        resources.exists(),
        "sender must hold its typed original until cancellation"
    );
    drop(prepared);
    assert!(
        !resources.exists(),
        "receiver raw descriptors must close before cancellation EOF"
    );
    fs::write(root.join("sender-clean"), "no ACK; EOF; removed").unwrap();
}
#[test]
fn wrong_direction_multipart_results_are_rejected_before_waiting_for_more_leases() {
    let _fork = goblins_unix::forking();
    for mode in [
        "preflight-handoff-start",
        "preflight-handoff-export",
        "preflight-handoff-bootstrap",
    ] {
        let root = goblins_unix::temp_directory().unwrap();
        let started = Instant::now();
        let error = lease(&executable(&root, mode), &root.join("manifest"), &root, 0).unwrap_err();
        assert_eq!(error.to_string(), "invalid preflight result method");
        assert!(started.elapsed() < Duration::from_secs(3));
        assert_reaped(&root);
        assert_eq!(
            fs::read_to_string(root.join("sender-clean")).unwrap(),
            "no ACK; EOF; removed"
        );
        assert!(!root.join("unexpected-resources").exists());
        fs::remove_dir_all(root).unwrap();
    }
}
