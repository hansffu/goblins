//! Fast engine-bootstrap probe: fake helper, real descriptors, no namespaces.
use super::*;
use goblins_runtime::{ResourceLease, lease::resource_exclusive};
use std::os::unix::{
    fs::{MetadataExt, PermissionsExt},
    net::UnixListener,
};

fn fake_helper(root: &Path) {
    let shell = Command::new("sh")
        .args(["-c", "command -v sh"])
        .output()
        .unwrap();
    assert!(shell.status.success());
    let shell = String::from_utf8(shell.stdout).unwrap();
    let quote = |path: &Path| format!("'{}'", path.display().to_string().replace('\'', "'\\''"));
    let helper = root.join("helper");
    fs::write(&helper, format!("#!{}\nprintf '%s\\n' \"$@\" > {}\nGOBLINS_FAKE_ENGINE_ROOT={} exec {} --ignored --exact docker::tests::fake_engine_bootstrap --nocapture\n", shell.trim(), quote(&root.join("args")), quote(root), quote(&std::env::current_exe().unwrap()))).unwrap();
    fs::set_permissions(helper, fs::Permissions::from_mode(0o700)).unwrap();
}
fn bootstrap_test_engine(root: &Path, leases: &[ResourceLease]) -> Result<Engine> {
    bootstrap_test_engine_with_cancel(root, leases, &process::Cancellation::default())
}
fn bootstrap_test_engine_with_cancel(
    root: &Path,
    leases: &[ResourceLease],
    cancel: &process::Cancellation,
) -> Result<Engine> {
    let path = root.join("scope-data");
    fs::create_dir_all(&path)?;
    let storage = Storage {
        path: path.clone(),
        parent: File::open(root)?,
        directory: File::open(path)?,
        lock: None,
        persistent: !root.join("anonymous-mode").exists(),
    };
    let namespaces = crate::scope_keeper::NamespaceFds {
        outer: File::open("/proc/self/ns/user")?,
        user: File::open("/proc/self/ns/user")?,
        net: File::open("/proc/self/ns/net")?,
        pid: None,
        net_path: PathBuf::new(),
    };
    Engine::bootstrap(
        &DockerConfig {
            bwrap: "/unused".into(),
            helper: root.join("helper"),
            daemon: "/unused".into(),
        },
        Filesystem {
            args: &[],
            fds: &[],
        },
        storage,
        &root.join("runtime"),
        leases,
        cancel,
        &namespaces,
    )
}
fn prepare_runtime(root: &Path) {
    unix::private_directory(&root.join("runtime")).unwrap();
    fs::create_dir(root.join("runtime/docker-socket")).unwrap();
}

#[test]
#[ignore = "fake child; invoked by guardian_retains_donor_resources_after_its_owner_releases_them"]
fn fake_engine_bootstrap() {
    let root = PathBuf::from(std::env::var_os("GOBLINS_FAKE_ENGINE_ROOT").unwrap());
    let args: Vec<_> = fs::read_to_string(root.join("args"))
        .unwrap()
        .lines()
        .map(str::to_owned)
        .collect();
    let info: i32 = args[args.iter().position(|a| a == "--info-fd").unwrap() + 1]
        .parse()
        .unwrap();
    let init = args.iter().position(|a| a == "--docker-init").unwrap();
    let ready_fd: i32 = args[init + 4].parse().unwrap();
    use std::os::fd::FromRawFd;
    let guardian_fd: i32 = args[init + 5].parse().unwrap();
    let locks: Vec<_> = args[init + 6]
        .split(',')
        .map(|s| unsafe { std::os::fd::OwnedFd::from_raw_fd(s.parse().unwrap()) })
        .collect();
    let mut guardian =
        guardian::Bootstrap::with_locks(unsafe { UnixStream::from_raw_fd(guardian_fd) }, locks)
            .unwrap()
            .enter()
            .unwrap();
    if root.join("early-mode").exists() {
        if root.join("anonymous-mode").exists() {
            fs::write(
                root.join("scope-data/engine-data"),
                "anonymous engine storage",
            )
            .unwrap();
        }
        fs::write(root.join("early-ready.tmp"), std::process::id().to_string()).unwrap();
        fs::rename(root.join("early-ready.tmp"), root.join("early-ready")).unwrap();
        let deadline = Instant::now() + Duration::from_secs(20);
        loop {
            guardian.receive().unwrap();
            if guardian.channel.closed() {
                break;
            }
            assert!(
                Instant::now() < deadline,
                "fake scope did not die before readiness"
            );
            std::thread::sleep(Duration::from_millis(5));
        }
        fs::write(root.join("early-eof"), "scope died before readiness").unwrap();
        while !root.join("finish-early-cleanup").exists() {
            assert!(
                Instant::now() < deadline,
                "early guardian cleanup was not released"
            );
            std::thread::sleep(Duration::from_millis(5));
        }
        if root.join("anonymous-mode").exists() {
            fs::remove_dir_all(root.join("scope-data")).unwrap();
        }
        drop(guardian);
        return;
    }
    let listener = UnixListener::bind(root.join("runtime/docker-socket/docker.sock")).unwrap();
    writeln!(
        unsafe { File::from_raw_fd(info) },
        "{{\n\"child-pid\": {}\n}}",
        std::process::id()
    )
    .unwrap();
    writeln!(
        unsafe { File::from_raw_fd(ready_fd) },
        "{}",
        std::process::id()
    )
    .unwrap();
    let (mut client, _) = listener.accept().unwrap();
    let mut request = [0u8; 128];
    client.read(&mut request).unwrap();
    client
        .write_all(b"HTTP/1.0 200 OK\r\nContent-Length: 2\r\n\r\nOK")
        .unwrap();
    drop(client);
    loop {
        guardian.receive().unwrap();
        // Inspect the real retained capability, not the metadata path. The
        // exec allowlist closed every donor descriptor before startup.
        for entry in fs::read_dir("/proc/self/fd").unwrap().flatten() {
            if fs::read_link(entry.path()).is_ok_and(|p| p == root.join("donor/.gc-lease")) {
                let metadata = fs::metadata(entry.path()).unwrap();
                fs::write(
                    root.join("lease-identity.tmp"),
                    format!("{} {}", metadata.dev(), metadata.ino()),
                )
                .unwrap();
                fs::rename(root.join("lease-identity.tmp"), root.join("lease-identity")).unwrap();
            }
        }
        if guardian.channel.closed() {
            break;
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    drop(guardian);
}

#[test]
fn guardian_retains_donor_resources_after_its_owner_releases_them() {
    let _forking = unix::forking();
    let root = unix::temp_directory().unwrap();
    struct Cleanup(PathBuf);
    impl Drop for Cleanup {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }
    let _cleanup = Cleanup(root.clone());
    let runtime = root.join("runtime");
    unix::private_directory(&runtime).unwrap();
    fs::create_dir(runtime.join("docker-socket")).unwrap();
    let donor = root.join("donor");
    unix::private_directory(&donor).unwrap();
    let donor_lease = ResourceLease::new(donor.clone()).unwrap();
    fs::write(donor.join("rooted-grant"), "retained").unwrap();
    let expected = fs::metadata(donor.join(".gc-lease")).unwrap();
    // A distinct runtime lock makes accidentally guarding the runtime instead
    // of the donor observable, even while the runtime itself stays alive.
    let runtime_lease = goblins_runtime::lease::flock(&runtime.join("lock"), false)
        .unwrap()
        .unwrap();
    fake_helper(&root);
    let engine = bootstrap_test_engine(&root, std::slice::from_ref(&donor_lease)).unwrap();
    let deadline = Instant::now() + Duration::from_secs(10);
    while !root.join("lease-identity").exists() {
        assert!(
            Instant::now() < deadline,
            "fake guardian did not report its retained capability"
        );
        std::thread::sleep(Duration::from_millis(5));
    }
    assert_eq!(
        fs::read_to_string(root.join("lease-identity")).unwrap(),
        format!("{} {}", expected.dev(), expected.ino())
    );
    assert_ne!(
        unsafe { libc::flock(donor_lease.fd(), libc::LOCK_EX | libc::LOCK_NB) },
        0
    );
    drop(donor_lease);
    assert_ne!(
        unsafe { libc::flock(runtime_lease.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) },
        0
    );
    drop(runtime_lease);
    assert!(!goblins_runtime::scope::remove_stale(&runtime, false).unwrap());
    assert!(
        donor.join("rooted-grant").exists(),
        "guardian guarded runtime ancestry instead of donor resources"
    );
    assert!(resource_exclusive(&donor).unwrap().is_none());
    drop(engine);
    assert!(resource_exclusive(&donor).unwrap().is_some());
    assert!(goblins_runtime::scope::remove_stale(&runtime, false).unwrap());
}

#[test]
#[ignore = "fake scope child; exercised by scope_death_before_engine_readiness_retains_the_entire_chain_and_runtime"]
fn fake_scope_starting_engine() {
    let _forking = unix::forking();
    let root = PathBuf::from(std::env::var_os("GOBLINS_FAKE_SCOPE_ROOT").unwrap());
    let leases = ["donor", "ancestor"]
        .into_iter()
        .map(|name| ResourceLease::new(root.join(name)).unwrap())
        .collect::<Vec<_>>();
    let _engine = bootstrap_test_engine(&root, &leases).unwrap();
    panic!("fake scope unexpectedly passed early engine readiness");
}

#[test]
fn scope_death_before_engine_readiness_retains_the_entire_chain_and_runtime() {
    let _forking = unix::forking();
    let root = unix::temp_directory().unwrap();
    prepare_runtime(&root);
    for name in ["ancestor", "donor"] {
        fs::create_dir(root.join(name)).unwrap();
        fs::write(root.join(name).join("root"), "retained").unwrap();
    }
    fs::write(root.join("early-mode"), "die before any info/readiness/ACK").unwrap();
    fake_helper(&root);
    let scope = Command::new(std::env::current_exe().unwrap())
        .args([
            "--ignored",
            "--exact",
            "docker::tests::fake_scope_starting_engine",
            "--nocapture",
        ])
        .env("GOBLINS_FAKE_SCOPE_ROOT", &root)
        .stdout(File::create(root.join("scope.log")).unwrap())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    struct Cleanup {
        scope: Child,
        guardian: Option<std::os::fd::OwnedFd>,
        root: PathBuf,
    }
    impl Drop for Cleanup {
        fn drop(&mut self) {
            let _ = self.scope.kill();
            let _ = self.scope.wait();
            if let Some(fd) = &self.guardian {
                unsafe {
                    libc::syscall(
                        libc::SYS_pidfd_send_signal,
                        fd.as_raw_fd(),
                        libc::SIGKILL,
                        std::ptr::null::<libc::siginfo_t>(),
                        0,
                    );
                }
                let mut poll = libc::pollfd {
                    fd: fd.as_raw_fd(),
                    events: libc::POLLIN,
                    revents: 0,
                };
                unsafe {
                    libc::poll(&mut poll, 1, 1000);
                }
            }
            let _ = fs::remove_dir_all(&self.root);
        }
    }
    let mut cleanup = Cleanup {
        scope,
        guardian: None,
        root: root.clone(),
    };
    let deadline = Instant::now() + Duration::from_secs(10);
    while !root.join("early-ready").exists() {
        assert!(
            Instant::now() < deadline,
            "early guardian did not start: {}",
            fs::read_to_string(root.join("scope.log")).unwrap()
        );
        std::thread::sleep(Duration::from_millis(5));
    }
    let pid: i32 = fs::read_to_string(root.join("early-ready"))
        .unwrap()
        .parse()
        .unwrap();
    cleanup.guardian =
        Some(unix::owned(unsafe { libc::syscall(libc::SYS_pidfd_open, pid, 0) as i32 }).unwrap());
    cleanup.scope.kill().unwrap();
    assert!(!cleanup.scope.wait().unwrap().success());
    while !root.join("early-eof").exists() {
        assert!(
            Instant::now() < deadline,
            "guardian did not observe pre-readiness scope death"
        );
        std::thread::sleep(Duration::from_millis(5));
    }
    for name in ["ancestor", "donor"] {
        let path = root.join(name);
        assert!(path.join("root").exists());
        assert!(
            resource_exclusive(&path).unwrap().is_none(),
            "bootstrap lost {name}'s fresh lease before first ACK"
        );
    }
    assert!(!goblins_runtime::scope::remove_stale(&root.join("runtime"), false).unwrap());
    fs::write(root.join("finish-early-cleanup"), "cleanup completed").unwrap();
    let mut poll = libc::pollfd {
        fd: cleanup.guardian.as_ref().unwrap().as_raw_fd(),
        events: libc::POLLIN,
        revents: 0,
    };
    assert_eq!(unsafe { libc::poll(&mut poll, 1, 10000) }, 1);
    for name in ["ancestor", "donor"] {
        assert!(resource_exclusive(&root.join(name)).unwrap().is_some());
    }
    assert!(goblins_runtime::scope::remove_stale(&root.join("runtime"), false).unwrap());
}

#[test]
fn startup_failure_reports_process_status_and_stderr_tail() {
    let _forking = unix::forking();
    let root = unix::temp_directory().unwrap();
    prepare_runtime(&root);
    fs::write(
        root.join("anonymous-mode"),
        "pre-guardian failure must remove empty storage",
    )
    .unwrap();
    let shell = Command::new("sh")
        .args(["-c", "command -v sh"])
        .output()
        .unwrap();
    assert!(shell.status.success());
    fs::write(
        root.join("helper"),
        format!(
            "#!{}\necho bootstrap-stderr-sentinel >&2\nexit 37\n",
            String::from_utf8(shell.stdout).unwrap().trim()
        ),
    )
    .unwrap();
    fs::set_permissions(root.join("helper"), fs::Permissions::from_mode(0o700)).unwrap();
    fs::create_dir(root.join("donor")).unwrap();
    let owner = ResourceLease::new(root.join("donor")).unwrap();
    let error = bootstrap_test_engine(&root, std::slice::from_ref(&owner))
        .err()
        .expect("expected fake helper failure")
        .to_string();
    assert!(error.contains("engine process exit status: 37"), "{error}");
    assert!(error.contains("bootstrap-stderr-sentinel"), "{error}");
    assert!(!root.join("scope-data").exists());
    drop(owner);
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn startup_leave_cancels_before_readiness_and_waits_for_guardian_cleanup() {
    let _forking = unix::forking();
    let root = unix::temp_directory().unwrap();
    prepare_runtime(&root);
    fs::create_dir(root.join("donor")).unwrap();
    let owner = ResourceLease::new(root.join("donor")).unwrap();
    fs::write(root.join("early-mode"), "hold readiness and cleanup").unwrap();
    fs::write(
        root.join("anonymous-mode"),
        "remove populated storage at cleanup",
    )
    .unwrap();
    fake_helper(&root);
    let (member, runtime) = UnixStream::pair().unwrap();
    let mut member = Channel::new(member).unwrap();
    let cancel = process::Cancellation::until_readable(member.fd().unwrap()).unwrap();
    let thread_root = root.clone();
    let (sent, result) = std::sync::mpsc::sync_channel(1);
    let worker = std::thread::spawn(move || {
        let value =
            bootstrap_test_engine_with_cancel(&thread_root, std::slice::from_ref(&owner), &cancel)
                .map(drop)
                .map_err(|e| e.to_string());
        sent.send(value).unwrap();
    });
    struct Cleanup {
        root: PathBuf,
        helper: Option<std::os::fd::OwnedFd>,
        worker: Option<std::thread::JoinHandle<()>>,
    }
    impl Drop for Cleanup {
        fn drop(&mut self) {
            let _ = fs::write(self.root.join("finish-early-cleanup"), "test cleanup");
            if let Some(fd) = &self.helper {
                unsafe {
                    libc::syscall(
                        libc::SYS_pidfd_send_signal,
                        fd.as_raw_fd(),
                        libc::SIGKILL,
                        std::ptr::null::<libc::siginfo_t>(),
                        0,
                    );
                }
            }
            if let Some(worker) = self.worker.take() {
                let _ = worker.join();
            }
            let _ = fs::remove_dir_all(&self.root);
        }
    }
    let mut cleanup = Cleanup {
        root: root.clone(),
        helper: None,
        worker: Some(worker),
    };
    let deadline = Instant::now() + Duration::from_secs(10);
    while !root.join("early-ready").exists() {
        assert!(
            Instant::now() < deadline,
            "guardian did not reach pre-readiness bootstrap"
        );
        std::thread::sleep(Duration::from_millis(5));
    }
    let pid: i32 = fs::read_to_string(root.join("early-ready"))
        .unwrap()
        .parse()
        .unwrap();
    cleanup.helper =
        Some(unix::owned(unsafe { libc::syscall(libc::SYS_pidfd_open, pid, 0) as i32 }).unwrap());
    let mut runtime = Channel::new(runtime).unwrap();
    runtime
        .queue(
            &goblins_runtime::scope::encode(&goblins_runtime::scope::Message::Leave {}),
            &mut vec![],
        )
        .unwrap();
    runtime.flush().unwrap();
    while !root.join("early-eof").exists() {
        assert!(
            Instant::now() < deadline,
            "queued Leave did not cancel real Engine bootstrap"
        );
        std::thread::sleep(Duration::from_millis(5));
    }
    assert!(matches!(
        result.try_recv(),
        Err(std::sync::mpsc::TryRecvError::Empty)
    ));
    assert!(root.join("scope-data/engine-data").exists());
    assert!(!goblins_runtime::scope::remove_stale(&root.join("runtime"), false).unwrap());
    fs::write(root.join("finish-early-cleanup"), "cleanup completed").unwrap();
    let error = result
        .recv_timeout(Duration::from_secs(10))
        .unwrap()
        .unwrap_err();
    assert!(error.contains("session cancelled"), "{error}");
    assert!(
        !root.join("scope-data").exists(),
        "cancelled bootstrap returned before anonymous storage cleanup"
    );
    cleanup.worker.take().unwrap().join().unwrap();
    assert!(goblins_runtime::scope::remove_stale(&root.join("runtime"), false).unwrap());
    assert!(matches!(
        goblins_runtime::scope::decode(member.receive().unwrap().unwrap().value).unwrap(),
        goblins_runtime::scope::Message::Leave {}
    ));
}
