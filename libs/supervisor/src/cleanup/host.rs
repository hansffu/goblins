//! Host tests: kernel backstops of storage cleanup. They need user and mount
//! namespaces, subordinate ids and the real mount helper, which the Nix build
//! sandbox lacks, so they are ignored there; `just test-rust-host` builds the
//! runtime fixture and runs the ignored tests of `host` modules.
//!
//! Each test runs part of itself again in a separate process (the "inner"
//! run, selected by `INNER`): inside Bubblewrap with a same-device bind mount,
//! or as the parent of a cleanup helper that the test kills.
use super::*;
use serde_json::Value;
use std::{
    ffi::OsStr,
    os::{fd::OwnedFd, unix::fs::PermissionsExt},
    process::Child,
    time::Instant,
};

/// The inner run's working root; set only in the inner run.
const INNER: &str = "GOBLINS_TEST_HOST_INNER";

/// A tool from the runtime fixture: `bwrap`, `helper` (the mount helper) or `shell`.
fn tool(name: &str) -> PathBuf {
    let path = std::env::var_os("GOBLINS_TEST_RUNTIME").unwrap_or_else(|| {
        panic!("GOBLINS_TEST_RUNTIME is unset; run host tests with `just test-rust-host`")
    });
    let value: Value = serde_json::from_slice(&fs::read(path).unwrap()).unwrap();
    PathBuf::from(value[name].as_str().unwrap())
}

fn inner_root() -> Option<PathBuf> {
    std::env::var_os(INNER).map(PathBuf::from)
}

/// This test again, as an inner run in `root`, optionally under `prefix`.
fn inner(test: &str, prefix: &[&OsStr], root: &Path) -> Command {
    let exe = std::env::current_exe().unwrap();
    let mut command = match prefix.split_first() {
        Some((program, args)) => {
            let mut command = Command::new(program);
            command.args(args).arg("--").arg(exe);
            command
        }
        None => Command::new(exe),
    };
    command
        .args([
            "--exact",
            test,
            "--ignored",
            "--nocapture",
            "--test-threads=1",
        ])
        .env(INNER, root)
        .env("XDG_CACHE_HOME", root.join("cache"))
        .env("XDG_STATE_HOME", root.join("history"));
    command
}

/// A private temporary root with an outside directory holding a payload.
fn fixture() -> (PathBuf, PathBuf) {
    let root = fs::canonicalize(unix::temp_directory().unwrap()).unwrap();
    let outside = root.join("outside");
    unix::private_directory(&outside).unwrap();
    fs::write(outside.join("payload"), "outside data\n").unwrap();
    (root, outside)
}

/// A minimal runtime manifest whose goblin uses `helper` for cleanup.
fn manifest(root: &Path, helper: &Path) -> PathBuf {
    let path = root.join("runtime.json");
    fs::write(
        &path,
        serde_json::json!({
            "api": 2, "helper_api": 6,
            "goblins": {"shell": {
                "build_spec": "/unused", "args": [], "env": {}, "client_package": "/unused",
                "helper": helper, "flake": "unused"
            }}
        })
        .to_string(),
    )
    .unwrap();
    path
}

/// Run `test` again in a private user, PID and mount namespace in which
/// `outside` is bind-mounted on `mounted`, a path on the same filesystem.
fn under_bind_mount(test: &str, root: &Path, outside: &Path, mounted: &Path) {
    // Device numbers cannot tell this mount from an ordinary directory.
    assert_eq!(
        fs::metadata(outside).unwrap().dev(),
        fs::metadata(mounted).unwrap().dev()
    );
    let bwrap = tool("bwrap");
    // The root bind would make /dev/null nodev; the inner run needs it.
    let prefix: Vec<&OsStr> = [
        bwrap.as_os_str(),
        OsStr::new("--die-with-parent"),
        OsStr::new("--unshare-user"),
        OsStr::new("--unshare-pid"),
        OsStr::new("--bind"),
        OsStr::new("/"),
        OsStr::new("/"),
        OsStr::new("--dev-bind"),
        OsStr::new("/dev/null"),
        OsStr::new("/dev/null"),
        OsStr::new("--proc"),
        OsStr::new("/proc"),
        OsStr::new("--bind"),
        outside.as_os_str(),
        mounted.as_os_str(),
    ]
    .into();
    // Output goes to files, so a deadline can be kept without draining pipes.
    let (stdout, stderr) = (root.join("inner-stdout"), root.join("inner-stderr"));
    let mut child = Killed({
        let _forking = unix::forking();
        inner(test, &prefix, root)
            .stdout(fs::File::create(&stdout).unwrap())
            .stderr(fs::File::create(&stderr).unwrap())
            .spawn()
            .unwrap()
    });
    // A cleanup that never returns fails the test instead of hanging the stage.
    let deadline = Instant::now() + Duration::from_secs(60);
    let status = loop {
        if let Some(status) = child.0.try_wait().unwrap() {
            break status;
        }
        assert!(Instant::now() < deadline, "the inner run did not finish in 60 s");
        std::thread::sleep(Duration::from_millis(10));
    };
    assert!(
        status.success(),
        "inner run failed: {}\n{}",
        fs::read_to_string(&stdout).unwrap_or_default(),
        fs::read_to_string(&stderr).unwrap_or_default()
    );
    assert_eq!(
        fs::read_to_string(outside.join("payload")).unwrap(),
        "outside data\n",
        "cleanup reached the contents of an outside bind mount"
    );
}

/// A scope runtime directory `state/scopes/work` with its lock file, the way
/// a stopped scope process leaves it.
fn scope_runtime(root: &Path) -> (PathBuf, PathBuf) {
    let state = root.join("state");
    let selected = state.join("scopes/work");
    unix::private_directory(&selected).unwrap();
    fs::File::create(selected.join("lock")).unwrap();
    (state, selected)
}

#[derive(Clone, Copy)]
enum Caller {
    /// `goblins gc`, sweeping the state directory's scope runtimes.
    Gc,
    /// The daemon removing a stale scope runtime before a relaunch.
    Relaunch,
}

fn mounted_scope_runtime(test: &str, caller: Caller, mounted_root: bool) {
    if let Some(root) = inner_root() {
        let (state, selected) = (root.join("state"), root.join("state/scopes/work"));
        let error = match caller {
            Caller::Gc => {
                let cancel = process::Cancellation::default();
                let mut output = Vec::new();
                let error = File::create(root.join("gc-stderr")).unwrap();
                let mut context = Context {
                    output: &mut output,
                    error: &error,
                    cancel: &cancel,
                };
                gc(None, &state, false, &mut context).unwrap_err()
            }
            Caller::Relaunch => goblins_runtime::scope::remove_stale(&selected, false).unwrap_err(),
        };
        assert!(
            error.to_string().contains("Invalid cross-device link"),
            "{error}"
        );
        return;
    }
    let (root, outside) = fixture();
    let (_, selected) = scope_runtime(&root);
    let mounted = if mounted_root {
        selected.clone()
    } else {
        let mounted = selected.join("mounted");
        fs::create_dir(&mounted).unwrap();
        mounted
    };
    under_bind_mount(test, &root, &outside, &mounted);
    assert!(selected.exists());
    if mounted_root {
        assert!(
            !outside.join("lock").exists(),
            "opened a lock inside a mounted outside root"
        );
    }
    fs::remove_dir_all(root).unwrap();
}

#[test]
#[ignore = "needs user namespaces; run with `just test-rust-host`"]
fn gc_refuses_a_same_device_mounted_scope_subtree() {
    mounted_scope_runtime(
        "cleanup::host::gc_refuses_a_same_device_mounted_scope_subtree",
        Caller::Gc,
        false,
    );
}

#[test]
#[ignore = "needs user namespaces; run with `just test-rust-host`"]
fn relaunch_removal_refuses_a_same_device_mounted_scope_subtree() {
    mounted_scope_runtime(
        "cleanup::host::relaunch_removal_refuses_a_same_device_mounted_scope_subtree",
        Caller::Relaunch,
        false,
    );
}

#[test]
#[ignore = "needs user namespaces; run with `just test-rust-host`"]
fn gc_refuses_a_mounted_scope_root_before_opening_its_lock() {
    mounted_scope_runtime(
        "cleanup::host::gc_refuses_a_mounted_scope_root_before_opening_its_lock",
        Caller::Gc,
        true,
    );
}

#[test]
#[ignore = "needs user namespaces; run with `just test-rust-host`"]
fn relaunch_removal_refuses_a_mounted_scope_root_before_opening_its_lock() {
    mounted_scope_runtime(
        "cleanup::host::relaunch_removal_refuses_a_mounted_scope_root_before_opening_its_lock",
        Caller::Relaunch,
        true,
    );
}

#[test]
#[ignore = "needs user namespaces and the mount helper; run with `just test-rust-host`"]
fn gc_refuses_a_mounted_session_subtree_in_the_helper() {
    const TEST: &str = "cleanup::host::gc_refuses_a_mounted_session_subtree_in_the_helper";
    let session = "a".repeat(32) + "-s1";
    if let Some(root) = inner_root() {
        let runtime = root.join("runtime.json");
        let cancel = process::Cancellation::default();
        let mut output = Vec::new();
        let error = File::create(root.join("gc-stderr")).unwrap();
        let mut context = Context {
            output: &mut output,
            error: &error,
            cancel: &cancel,
        };
        let failure = gc(Some(&runtime), &root.join("state"), false, &mut context).unwrap_err();
        assert!(failure.to_string().contains("cleanup failed"), "{failure}");
        // The helper's own diagnostic names the refused mount.
        let diagnostic = fs::read_to_string(root.join("gc-stderr")).unwrap();
        assert!(
            diagnostic.contains("Invalid cross-device link"),
            "{diagnostic}"
        );
        return;
    }
    let (root, outside) = fixture();
    manifest(&root, &tool("helper"));
    let directory = root.join("state").join(&session);
    let mounted = directory.join("mounted");
    unix::private_directory(&mounted).unwrap();
    fs::write(directory.join("payload"), "session data\n").unwrap();
    under_bind_mount(TEST, &root, &outside, &mounted);
    assert!(directory.exists());
    fs::remove_dir_all(root).unwrap();
}

/// Whether nobody holds `path`'s lock any more: probed, then let go.
fn unlocked(path: &Path) -> bool {
    flock(path, true).unwrap().is_some()
}

/// A child process killed and reaped when the test ends, also on failure.
struct Killed(Child);
impl Drop for Killed {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn pidfd(pid: u32) -> OwnedFd {
    unix::owned(unsafe { libc::syscall(libc::SYS_pidfd_open, pid, 0) as i32 }).unwrap()
}

fn proc_text(pid: u32, name: &str) -> String {
    fs::read_to_string(format!("/proc/{pid}/{name}")).unwrap_or_default()
}

/// The helper's blocking system call: its number and first argument.
fn blocked_call(pid: u32) -> Option<(i64, u64)> {
    let text = proc_text(pid, "syscall");
    let mut words = text.split_whitespace();
    let number = words.next()?.parse().ok()?;
    let first = u64::from_str_radix(words.next()?.trim_start_matches("0x"), 16).ok()?;
    Some((number, first))
}

/// The `scope clear` maintenance of a scope `work`, killed while its real
/// cleanup helper waits: before its ids are mapped, or after.
fn helper_survives_no_parent(test: &str, mapped: bool) {
    if let Some(root) = inner_root() {
        // The cleanup parent: `scope clear work` with the wrapped helper. The
        // test kills this process; it never returns on its own.
        let cancel = process::Cancellation::default();
        let mut output = Vec::new();
        let error = File::from(unix::owned(unsafe { libc::dup(2) }).unwrap());
        let mut context = Context {
            output: &mut output,
            error: &error,
            cancel: &cancel,
        };
        let result = clear_scope(
            Some(&root.join("runtime.json")),
            Some("work"),
            false,
            &mut context,
        );
        panic!("the cleanup parent finished before it was killed: {result:?}");
    }
    let (root, outside) = fixture();
    let cache = root.join("cache/goblins");
    let selected = cache.join("scopes/scope-work");
    unix::private_directory(&selected).unwrap();
    fs::write(selected.join("payload"), "scope data\n").unwrap();
    std::os::unix::fs::symlink(&outside, selected.join("link")).unwrap();
    for directory in [cache.join("scopes"), cache.clone()] {
        fs::set_permissions(directory, fs::Permissions::from_mode(0o700)).unwrap();
    }
    // The wrapper records its pid and waits for the test before it becomes
    // the real helper, keeping the pid and the inherited descriptors.
    let helper = tool("helper");
    let release = root.join("release");
    unsafe {
        unix::cvt(libc::mkfifo(
            std::ffi::CString::new(release.as_os_str().as_encoded_bytes())
                .unwrap()
                .as_ptr(),
            0o600,
        ))
        .unwrap();
    }
    let wrapper = root.join("helper");
    fs::write(
        &wrapper,
        format!(
            "#!{}\necho $$ > {}\nread -r _ < {}\nexec {} \"$@\"\n",
            tool("shell").display(),
            root.join("helper.pid").display(),
            release.display(),
            helper.display()
        ),
    )
    .unwrap();
    fs::set_permissions(&wrapper, fs::Permissions::from_mode(0o700)).unwrap();
    manifest(&root, &wrapper);
    // After its ids are mapped the helper fails (the scope directory is gone)
    // and blocks writing that to a full stderr.
    let (errors, error_sink) = unix::pipe().unwrap();
    if mapped {
        let capacity = unsafe { libc::fcntl(error_sink.as_raw_fd(), libc::F_GETPIPE_SZ) };
        let fill = vec![b'F'; capacity as usize - b"goblins helper: ".len()];
        File::from(error_sink.try_clone().unwrap())
            .write_all(&fill)
            .unwrap();
    }
    let mut parent = Killed({
        let _forking = unix::forking();
        inner(test, &[], &root)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::from(error_sink))
            .spawn()
            .unwrap()
    });
    let deadline = Instant::now() + Duration::from_secs(10);
    let marker = root.join("helper.pid");
    let pid: u32 = loop {
        if let Some(pid) = fs::read_to_string(&marker)
            .ok()
            .and_then(|text| text.strip_suffix('\n').map(str::to_owned))
        {
            break pid.parse().unwrap();
        }
        assert!(
            parent.0.try_wait().unwrap().is_none(),
            "the cleanup parent exited"
        );
        assert!(Instant::now() < deadline, "no cleanup helper started");
        std::thread::sleep(Duration::from_millis(5));
    };
    let exited = pidfd(pid);
    let locks = [
        cache.join("locks/storage.lock"),
        cache.join("scopes/locks/work"),
    ];
    let inodes: Vec<_> = locks
        .iter()
        .map(|path| fs::metadata(path).unwrap().ino())
        .collect();
    for path in &locks {
        assert!(!unlocked(path), "{} is not held", path.display());
    }
    let saved = cache.join("scopes/saved-work");
    if mapped {
        fs::rename(&selected, &saved).unwrap();
    } else {
        // Without its parent the helper is never mapped or told to delete.
        unsafe { libc::kill(parent.0.id() as i32, libc::SIGSTOP) };
    }
    File::options()
        .write(true)
        .open(&release)
        .unwrap()
        .write_all(b"go\n")
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        assert!(
            !unix::readable(exited.as_raw_fd(), 0).unwrap(),
            "the helper exited before the phase under test"
        );
        let real = fs::read_link(format!("/proc/{pid}/exe")).is_ok_and(|exe| exe == helper);
        let uid_map = proc_text(pid, "uid_map");
        let call = blocked_call(pid);
        let reached = real
            && if mapped {
                // Mapped to this user and blocked writing its diagnostic.
                uid_map.split_whitespace().take(3).eq([
                    "0".to_owned(),
                    unsafe { libc::getuid() }.to_string(),
                    "1".to_owned(),
                ]
                .iter()
                .map(String::as_str))
                    && matches!(call, Some((libc::SYS_write | libc::SYS_writev, 2)))
            } else {
                // Ready, unmapped, waiting for its parent's go-ahead on stdin.
                uid_map.trim().is_empty() && call == Some((libc::SYS_read, 0))
            };
        if reached {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "the helper never reached the phase under test: {uid_map:?} {call:?}"
        );
        std::thread::sleep(Duration::from_millis(2));
    }
    // The parent dies without any cleanup of its own, stopped or not.
    parent.0.kill().unwrap();
    parent.0.wait().unwrap();
    assert!(
        unix::readable(exited.as_raw_fd(), 5000).unwrap(),
        "the cleanup helper outlived its parent"
    );
    for (path, inode) in locks.iter().zip(inodes) {
        assert_eq!(fs::metadata(path).unwrap().ino(), inode);
        assert!(unlocked(path), "{} stayed locked", path.display());
    }
    let retained = if mapped { &saved } else { &selected };
    assert_eq!(
        fs::read_to_string(retained.join("payload")).unwrap(),
        "scope data\n"
    );
    assert_eq!(
        fs::read_to_string(outside.join("payload")).unwrap(),
        "outside data\n"
    );
    drop(errors);
    fs::remove_dir_all(root).unwrap();
}

#[test]
#[ignore = "needs the mount helper; run with `just test-rust-host`"]
fn a_ready_helper_exits_and_releases_inherited_locks_when_its_parent_dies() {
    helper_survives_no_parent(
        "cleanup::host::a_ready_helper_exits_and_releases_inherited_locks_when_its_parent_dies",
        false,
    );
}

#[test]
#[ignore = "needs subordinate ids and the mount helper; run with `just test-rust-host`"]
fn a_mapped_helper_blocked_on_stderr_exits_and_releases_locks_when_its_parent_dies() {
    helper_survives_no_parent(
        "cleanup::host::a_mapped_helper_blocked_on_stderr_exits_and_releases_locks_when_its_parent_dies",
        true,
    );
}
