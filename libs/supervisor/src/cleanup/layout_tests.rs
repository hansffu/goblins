//! Layout and lease rules need ordinary files, not a sandbox launch. Each
//! case has its own process so its XDG paths cannot affect parallel tests.
use super::*;
use std::{os::unix::fs::symlink, time::Instant};

fn isolated(name: &str, test: impl FnOnce(&Path)) {
    const INNER: &str = "GOBLINS_CLEANUP_LAYOUT_TEST";
    if let Some(root) = std::env::var_os(INNER) {
        test(Path::new(&root));
        return;
    }
    struct Child(std::process::Child);
    impl Drop for Child {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }
    let root = unix::temp_directory().unwrap();
    let log = root.join("test.log");
    let mut child = {
        let _fork = unix::forking();
        Child(
            Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    &format!("cleanup::layout_tests::{name}"),
                    "--nocapture",
                ])
                .env(INNER, &root)
                .env("XDG_CACHE_HOME", root.join("cache"))
                .env("XDG_STATE_HOME", root.join("history"))
                .stdout(File::create(&log).unwrap())
                .stderr(File::options().append(true).open(&log).unwrap())
                .spawn()
                .unwrap(),
        )
    };
    let deadline = Instant::now() + Duration::from_secs(10);
    let status = loop {
        if let Some(status) = child.0.try_wait().unwrap() {
            break status;
        }
        assert!(Instant::now() < deadline, "cleanup layout test timed out");
        std::thread::sleep(Duration::from_millis(5));
    };
    assert!(status.success(), "{}", fs::read_to_string(&log).unwrap());
    drop(child);
    fs::remove_dir_all(root).unwrap();
}

fn directory(path: &Path) {
    unix::private_directory(path).unwrap();
    fs::write(path.join("payload"), "retained").unwrap();
}
fn run(
    operation: impl FnOnce(&mut Context) -> Result<()>,
) -> (std::result::Result<(), String>, String) {
    let mut output = Vec::new();
    let error = File::options().write(true).open("/dev/null").unwrap();
    let cancel = process::Cancellation::default();
    let result = operation(&mut Context {
        output: &mut output,
        error: &error,
        cancel: &cancel,
    });
    (
        result.map_err(|e| e.to_string()),
        String::from_utf8(output).unwrap(),
    )
}
fn store(kind: &str, name: &str, leased: bool) -> PathBuf {
    let path = unix::cache_directory(kind, true).unwrap().join(name);
    directory(&path);
    if leased {
        drop(temporary_lease_at(&lock_root().unwrap(), kind, &path).unwrap());
    }
    path
}

#[test]
fn scope_selection_preflights_every_lock_and_refuses_aliases() {
    isolated(
        "scope_selection_preflights_every_lock_and_refuses_aliases",
        |root| {
            let a = store("scopes", "scope-a", false);
            let z = store("docker", "scope-z", false);
            let locks = docker::storage_root(true).unwrap().join("locks");
            unix::private_directory(&locks).unwrap();
            let lease = flock(&locks.join("z"), false).unwrap().unwrap();
            let inode = lease.metadata().unwrap().ino();
            let (result, _) = run(|c| clear_scope(None, None, false, c));
            assert!(result.unwrap_err().contains("in use"));
            assert!(a.exists() && z.exists());
            drop(lease);
            let (result, output) = run(|c| clear_scope(None, Some("a"), true, c));
            result.unwrap();
            assert!(output.contains(&format!("Would remove {}", a.display())));
            assert!(!output.contains(&z.display().to_string()));
            assert!(a.exists() && z.exists());
            assert_eq!(fs::metadata(locks.join("z")).unwrap().ino(), inode);
            let outside = root.join("outside");
            directory(&outside);
            symlink(
                &outside,
                scope_storage::root(true).unwrap().join("scope-link"),
            )
            .unwrap();
            assert!(
                run(|c| clear_scope(None, Some("link"), false, c))
                    .0
                    .unwrap_err()
                    .contains("unsafe scope")
            );
            assert_eq!(
                fs::read_to_string(outside.join("payload")).unwrap(),
                "retained"
            );
            assert!(
                run(|c| clear_scope(None, Some("../escape"), false, c))
                    .0
                    .unwrap_err()
                    .contains("invalid scope name")
            );
        },
    );
}

#[test]
fn gc_selects_only_retired_leased_stores_and_unowned_server_resources() {
    isolated(
        "gc_selects_only_retired_leased_stores_and_unowned_server_resources",
        |root| {
            let live = store("scopes", "temporary-Abc123", true);
            let stale = store("docker", "session-Xyz987", true);
            let persistent = store("docker", "scope-kept", false);
            let unknown = store("scopes", "unrecognized", false);
            let legacy = store("scopes", "temporary-Old123", false);
            let malformed = store("scopes", "temporary-toolong", true);
            let lease = temporary_lease_at(&lock_root().unwrap(), "scopes", &live).unwrap();
            let scratch = lock_root().unwrap().join("cleanup-Abc123");
            directory(&scratch);
            let state = root.join("state");
            unix::private_directory(&state).unwrap();
            let session = state.join(format!("{}-s1", "a".repeat(32)));
            let resources = session.join("resources");
            unix::private_directory(&session).unwrap();
            directory(&resources);
            let resource_lease = flock(&resources.join(".gc-lease"), false).unwrap().unwrap();
            let daemon = flock(&state.join("daemon.lock"), true).unwrap().unwrap();
            let manifests = state.join("manifests");
            directory(&manifests);
            fs::write(state.join("server.log"), "log").unwrap();
            let history = root.join("history/goblins/sessions");
            fs::create_dir_all(&history).unwrap();
            fs::write(history.join("record.json"), "history").unwrap();
            let (result, output) = run(|c| gc(None, &state, true, c));
            result.unwrap();
            for path in [&stale, &scratch] {
                assert!(output.contains(&format!("Would remove {}", path.display())));
            }
            for path in [
                &live,
                &persistent,
                &unknown,
                &legacy,
                &malformed,
                &session,
                &manifests,
            ] {
                assert!(
                    !output.contains(&format!("Would remove {}", path.display())),
                    "{output}"
                );
                assert!(path.exists());
            }
            assert!(output.contains("running server"));
            drop(daemon);
            let (result, output) = run(|c| gc(None, &state, true, c));
            result.unwrap();
            assert!(output.contains("Keeping active session resources"));
            assert!(output.contains(&format!("Would remove {}", manifests.display())));
            drop(resource_lease);
            drop(lease);
            let (result, output) = run(|c| gc(None, &state, true, c));
            result.unwrap();
            for path in [&session, &live, &stale] {
                assert!(output.contains(&format!("Would remove {}", path.display())));
                assert!(path.exists(), "dry run deleted {}", path.display());
            }
            assert_eq!(fs::read_to_string(state.join("server.log")).unwrap(), "log");
            assert_eq!(
                fs::read_to_string(history.join("record.json")).unwrap(),
                "history"
            );
        },
    );
}

#[test]
fn gc_refuses_symlinked_roots_sessions_and_resource_directories() {
    isolated(
        "gc_refuses_symlinked_roots_sessions_and_resource_directories",
        |root| {
            let cache = unix::cache_directory("", true).unwrap();
            let outside = root.join("outside");
            directory(&outside);
            let state = root.join("state");
            symlink(&outside, cache.join("scopes")).unwrap();
            assert!(run(|c| gc(None, &state, true, c)).0.is_err());
            fs::remove_file(cache.join("scopes")).unwrap();
            unix::private_directory(&state).unwrap();
            let session = state.join(format!("{}-s1", "a".repeat(32)));
            symlink(&outside, &session).unwrap();
            assert!(
                run(|c| gc(None, &state, true, c))
                    .0
                    .unwrap_err()
                    .contains("unsafe server resource")
            );
            fs::remove_file(&session).unwrap();
            unix::private_directory(&session).unwrap();
            symlink(&outside, session.join("resources")).unwrap();
            assert!(
                run(|c| gc(None, &state, true, c))
                    .0
                    .unwrap_err()
                    .contains("unsafe session resources")
            );
            assert!(!outside.join(".gc-lease").exists());
            assert_eq!(
                fs::read_to_string(outside.join("payload")).unwrap(),
                "retained"
            );
        },
    );
}

#[test]
fn only_session_identifiers_are_server_gc_candidates() {
    assert!(session_name(&format!("{}-s123", "a".repeat(32))));
    for name in [
        "server.log",
        "manifests-old",
        "a-s1",
        "communications-aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa.jsonl",
        "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-s",
        "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-sx",
    ] {
        assert!(!session_name(name), "{name}");
    }
}
