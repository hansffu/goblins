//! Fast engine-bootstrap probe: fake helper, real descriptors, no namespaces.
use super::*;
use goblins_runtime::{ResourceLease, lease::resource_exclusive};
use std::os::unix::{
    fs::{MetadataExt, PermissionsExt},
    net::UnixListener,
};

#[test]
#[ignore = "fake child; invoked by guardian_retains_donor_resources_after_its_owner_releases_them"]
fn fake_engine_bootstrap() {
    let root = PathBuf::from(std::env::var_os("GOBLINS_FAKE_ENGINE_ROOT").unwrap());
    let args: Vec<_> = fs::read_to_string(root.join("args"))
        .unwrap()
        .lines()
        .map(str::to_owned)
        .collect();
    let destination = args
        .iter()
        .position(|a| a == "/run/goblins/storage-lease")
        .unwrap();
    assert_eq!(args[destination - 2], "--ro-bind-fd");
    let original: i32 = args[destination - 1].parse().unwrap();
    let lease = File::open(format!("/proc/self/fd/{original}")).unwrap();
    unix::cvt(unsafe { libc::flock(lease.as_raw_fd(), libc::LOCK_SH | libc::LOCK_NB) }).unwrap();
    // Bubblewrap consumes the inherited bind-fd; retain only a fresh shared
    // lease, so this fixture cannot accidentally rely on the donor's OFD.
    unsafe {
        libc::close(original);
    }
    let metadata = lease.metadata().unwrap();
    fs::write(
        root.join("lease-identity"),
        format!("{} {}", metadata.dev(), metadata.ino()),
    )
    .unwrap();
    let info: i32 = args[args.iter().position(|a| a == "--info-fd").unwrap() + 1]
        .parse()
        .unwrap();
    let init = args.iter().position(|a| a == "--docker-init").unwrap();
    let ready_fd: i32 = args[init + 4].parse().unwrap();
    let listener = UnixListener::bind(root.join("runtime/docker-socket/docker.sock")).unwrap();
    use std::os::fd::FromRawFd;
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
    std::io::stdin().read(&mut [0]).unwrap();
    drop(lease);
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
    let _runtime_lease = goblins_runtime::lease::flock(&runtime.join("lock"), false)
        .unwrap()
        .unwrap();
    let shell = Command::new("sh")
        .args(["-c", "command -v sh"])
        .output()
        .unwrap();
    assert!(shell.status.success());
    let shell = String::from_utf8(shell.stdout).unwrap();
    let quote = |path: &Path| format!("'{}'", path.display().to_string().replace('\'', "'\\''"));
    let helper = root.join("helper");
    fs::write(&helper, format!("#!{}\nprintf '%s\\n' \"$@\" > {}\nGOBLINS_FAKE_ENGINE_ROOT={} exec {} --ignored --exact docker::tests::fake_engine_bootstrap --nocapture\n", shell.trim(), quote(&root.join("args")), quote(&root), quote(&std::env::current_exe().unwrap()))).unwrap();
    fs::set_permissions(&helper, fs::Permissions::from_mode(0o700)).unwrap();
    let launch: Launch = serde_json::from_value(serde_json::json!({
        "shell":"/unused", "posix_shell":"/unused", "helper":helper,
        "bwrap":"/unused", "flake":"unused", "client_package":"/unused",
        "docker":{"daemon":"/unused","client":null,"enabled":false}
    }))
    .unwrap();
    let path = root.join("scope-data");
    fs::create_dir(&path).unwrap();
    let storage = Storage {
        path: path.clone(),
        parent: File::open(&root).unwrap(),
        directory: File::open(&path).unwrap(),
        lock: None,
        persistent: true,
    };
    let namespaces = crate::scope_keeper::NamespaceFds {
        outer: File::open("/proc/self/ns/user").unwrap(),
        user: File::open("/proc/self/ns/user").unwrap(),
        net: File::open("/proc/self/ns/net").unwrap(),
        pid: None,
        net_path: PathBuf::new(),
    };
    let engine = Engine::bootstrap(
        &launch,
        Filesystem {
            args: &[],
            fds: &[],
        },
        storage,
        &runtime,
        &donor_lease,
        &process::Cancellation::default(),
        &namespaces,
    )
    .unwrap();
    assert_eq!(
        fs::read_to_string(root.join("lease-identity")).unwrap(),
        format!("{} {}", expected.dev(), expected.ino())
    );
    drop(donor_lease);
    assert!(
        donor.join("rooted-grant").exists(),
        "guardian guarded runtime ancestry instead of donor resources"
    );
    assert!(resource_exclusive(&donor).unwrap().is_none());
    drop(engine);
    assert!(resource_exclusive(&donor).unwrap().is_some());
}
