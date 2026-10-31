use super::*;
use crate::supervised::{oneshot, tests::executable};
use goblins_runtime::{
    Event,
    channel::Channel,
    supervisor::{Encoder, Received, outbound::Writer},
};
use std::{
    fs::{self, File},
    io::{Read, Write},
    os::fd::{AsRawFd, OwnedFd},
    path::Path,
    thread,
    time::{Duration, Instant},
};

pub(crate) fn peer(root: &Path, mode: &str, received: Received, mut channel: Channel) {
    let Message::Maintenance(request) = received.message else {
        panic!("maintenance request")
    };
    fs::write(root.join("pid"), std::process::id().to_string()).unwrap();
    fs::write(
        root.join("groups"),
        format!("{} {}", unsafe { libc::getpgrp() }, unsafe {
            libc::getpgid(libc::getppid())
        }),
    )
    .unwrap();
    if mode == "maintenance-multipart" {
        crate::supervised::preflight::tests::wrong_direction_handoff(
            root,
            "preflight-handoff-start",
            channel,
            Encoder::default(),
        );
        return;
    }
    let mut output = File::from(OwnedFd::from(request.stdout));
    let mut error = File::from(OwnedFd::from(request.stderr));
    if mode == "maintenance-eof" {
        return;
    }
    if mode == "maintenance-partial" {
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
        return;
    }
    if mode == "maintenance-blocked" {
        fs::write(root.join("writing"), "blocked").unwrap();
        for _ in 0..1024 {
            output.write_all(&[b'x'; 4096]).unwrap();
        }
        panic!("blocked output unexpectedly drained");
    }
    if mode == "maintenance-stream" {
        let bytes: Vec<u8> = (0..4096).map(|n| n as u8).collect();
        for _ in 0..512 {
            output.write_all(&bytes).unwrap();
        }
    } else {
        output.write_all(b"progress\n").unwrap();
    }
    error.write_all(&[0xff, 0, 0x80, b'\n']).unwrap();
    drop(output);
    drop(error);
    let result = if matches!(mode, "maintenance-error" | "maintenance-error-nonzero") {
        Err("original cleanup diagnostic".into())
    } else {
        Ok(())
    };
    let mut encoder = Encoder::default();
    let mut writer = Writer::default();
    let message = if mode == "maintenance-direction" {
        Message::Event(Event::Mode { server: false })
    } else {
        Message::MaintenanceFinished { result }
    };
    writer
        .begin(encoder.prepare(message).unwrap())
        .ok()
        .unwrap();
    while !writer.idle(&channel) {
        writer.pump(&mut channel).unwrap();
    }
    if mode == "maintenance-duplicate" {
        writer
            .begin(
                encoder
                    .prepare(Message::MaintenanceFinished { result: Ok(()) })
                    .unwrap(),
            )
            .ok()
            .unwrap();
        while !writer.idle(&channel) {
            writer.pump(&mut channel).unwrap();
        }
    }
    if mode == "maintenance-error-nonzero" || mode == "maintenance-nonzero" {
        std::process::exit(7);
    }
    channel.close();
    if mode == "maintenance-hang" {
        thread::sleep(Duration::from_secs(30));
    } else {
        thread::sleep(Duration::from_millis(100));
    }
}

fn request(output: OwnedFd, error: OwnedFd, root: &Path) -> Message {
    Message::Maintenance(Maintenance {
        supervisor_api: supervisor::API,
        operation: MaintenanceOperation::Gc {
            runtime: None,
            state: root.join("state"),
            dry_run: true,
        },
        stdout: output.into(),
        stderr: error.into(),
    })
}
fn reaped(root: &Path) {
    let pid: i32 = fs::read_to_string(root.join("pid"))
        .unwrap()
        .parse()
        .unwrap();
    assert_eq!(
        unsafe { libc::waitpid(pid, std::ptr::null_mut(), libc::WNOHANG) },
        -1
    );
    assert_eq!(
        std::io::Error::last_os_error().raw_os_error(),
        Some(libc::ECHILD)
    );
}
fn files(root: &Path) -> (OwnedFd, OwnedFd) {
    (
        File::create(root.join("stdout")).unwrap().into(),
        File::create(root.join("stderr")).unwrap().into(),
    )
}
#[test]
fn maintenance_preserves_progress_binary_stderr_and_error_only_after_eof_and_reap() {
    let _fork = goblins_unix::forking();
    for mode in ["maintenance-success", "maintenance-error"] {
        let root = goblins_unix::temp_directory().unwrap();
        let (out, err) = files(&root);
        let result = oneshot::request(
            &executable(&root, mode),
            request(out, err, &root),
            Expected::Maintenance,
            || false,
        )
        .unwrap();
        match result {
            Message::MaintenanceFinished { result } => assert_eq!(
                result,
                if mode == "maintenance-error" {
                    Err("original cleanup diagnostic".into())
                } else {
                    Ok(())
                }
            ),
            _ => panic!("wrong result"),
        }
        reaped(&root);
        assert_eq!(fs::read(root.join("stdout")).unwrap(), b"progress\n");
        assert_eq!(
            fs::read(root.join("stderr")).unwrap(),
            [0xff, 0, 0x80, b'\n']
        );
        let groups: Vec<i32> = fs::read_to_string(root.join("groups"))
            .unwrap()
            .split_whitespace()
            .map(|s| s.parse().unwrap())
            .collect();
        assert_ne!(
            groups[0], groups[1],
            "maintenance must leave its CLI's signal group"
        );
        fs::remove_dir_all(root).unwrap();
    }
}
#[test]
fn maintenance_rejects_wrong_direction_multipart_partial_duplicate_missing_and_nonzero_results() {
    let _fork = goblins_unix::forking();
    for (mode, expected) in [
        ("maintenance-eof", "maintenance closed without a result"),
        ("maintenance-partial", "unexpected end of file"),
        ("maintenance-duplicate", "extra maintenance result"),
        ("maintenance-direction", "invalid maintenance result method"),
        ("maintenance-multipart", "invalid maintenance result method"),
        (
            "maintenance-nonzero",
            "maintenance supervisor exited unsuccessfully",
        ),
        (
            "maintenance-error-nonzero",
            "maintenance supervisor exited unsuccessfully",
        ),
    ] {
        let root = goblins_unix::temp_directory().unwrap();
        let (out, err) = files(&root);
        let error = oneshot::request(
            &executable(&root, mode),
            request(out, err, &root),
            Expected::Maintenance,
            || false,
        )
        .err()
        .expect("reject result");
        assert!(error.to_string().contains(expected), "{mode}: {error}");
        reaped(&root);
        if mode == "maintenance-multipart" {
            assert_eq!(
                fs::read_to_string(root.join("sender-clean")).unwrap(),
                "no ACK; EOF; removed"
            );
            assert!(!root.join("unexpected-resources").exists());
        }
        fs::remove_dir_all(root).unwrap();
    }
}
#[test]
fn maintenance_output_is_streamed_over_pipe_capacity_without_changing_shared_flags_or_timing_out_work()
 {
    let _fork = goblins_unix::forking();
    let root = goblins_unix::temp_directory().unwrap();
    let (read, write) = goblins_unix::pipe().unwrap();
    let held = write.try_clone().unwrap();
    let flags = unsafe { libc::fcntl(held.as_raw_fd(), libc::F_GETFL) };
    let reader = thread::spawn(move || {
        thread::sleep(Duration::from_millis(2100));
        let mut output = Vec::new();
        let mut file = File::from(read);
        let mut chunk = [0; 8192];
        loop {
            let n = file.read(&mut chunk).unwrap();
            if n == 0 {
                break;
            }
            output.extend_from_slice(&chunk[..n]);
            thread::sleep(Duration::from_millis(1));
        }
        output
    });
    let started = Instant::now();
    let message = request(
        write,
        File::create(root.join("stderr")).unwrap().into(),
        &root,
    );
    let result = oneshot::request(
        &executable(&root, "maintenance-stream"),
        message,
        Expected::Maintenance,
        || false,
    )
    .unwrap();
    assert!(matches!(
        result,
        Message::MaintenanceFinished { result: Ok(()) }
    ));
    assert!(started.elapsed() >= Duration::from_millis(2100));
    assert_eq!(
        unsafe { libc::fcntl(held.as_raw_fd(), libc::F_GETFL) },
        flags
    );
    drop(held);
    let output = reader.join().unwrap();
    assert_eq!(output.len(), 512 * 4096);
    assert!(output.iter().enumerate().all(|(n, b)| *b == n as u8));
    reaped(&root);
    fs::remove_dir_all(root).unwrap();
}
#[test]
fn cancellation_stays_responsive_while_child_output_is_blocked_and_has_one_cleanup_grace() {
    let _fork = goblins_unix::forking();
    let root = goblins_unix::temp_directory().unwrap();
    let (_read, write) = goblins_unix::pipe().unwrap();
    let held = write.try_clone().unwrap();
    let flags = unsafe { libc::fcntl(held.as_raw_fd(), libc::F_GETFL) };
    let message = request(
        write,
        File::create(root.join("stderr")).unwrap().into(),
        &root,
    );
    let started = Instant::now();
    let error = oneshot::request(
        &executable(&root, "maintenance-blocked"),
        message,
        Expected::Maintenance,
        || started.elapsed() >= Duration::from_millis(100),
    )
    .err()
    .unwrap();
    assert_eq!(error.to_string(), "session cancelled");
    assert!(root.join("writing").exists());
    assert!(
        started.elapsed() >= Duration::from_secs(2)
            && started.elapsed() < Duration::from_millis(3200)
    );
    assert_eq!(
        unsafe { libc::fcntl(held.as_raw_fd(), libc::F_GETFL) },
        flags
    );
    reaped(&root);
    fs::remove_dir_all(root).unwrap();
}
#[test]
fn maintenance_postresult_completion_deadline_is_fixed_without_second_grace() {
    let _fork = goblins_unix::forking();
    let root = goblins_unix::temp_directory().unwrap();
    let (out, err) = files(&root);
    let started = Instant::now();
    let error = oneshot::request(
        &executable(&root, "maintenance-hang"),
        request(out, err, &root),
        Expected::Maintenance,
        || false,
    )
    .err()
    .unwrap();
    assert!(error.to_string().contains("within 2 seconds"), "{error}");
    assert!(
        started.elapsed() >= Duration::from_secs(2) && started.elapsed() < Duration::from_secs(3)
    );
    reaped(&root);
    fs::remove_dir_all(root).unwrap();
}
#[test]
fn already_cancelled_maintenance_never_spawns_or_creates_operation_state() {
    let _fork = goblins_unix::forking();
    let root = goblins_unix::temp_directory().unwrap();
    let (out, err) = files(&root);
    let error = oneshot::request(
        &executable(&root, "maintenance-success"),
        request(out, err, &root),
        Expected::Maintenance,
        || true,
    )
    .err()
    .unwrap();
    assert_eq!(error.to_string(), "session cancelled");
    assert!(!root.join("pid").exists());
    assert!(!root.join("state").exists());
    fs::remove_dir_all(root).unwrap();
}
