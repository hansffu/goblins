//! Host tests: a real Session in Bubblewrap, with the Nix-built runtime and
//! package fixtures. They need user namespaces and the host's Nix store, which
//! the Nix build sandbox lacks, so they are ignored there; `just
//! test-rust-host` builds the fixtures and runs the ignored tests of `host`
//! modules. The checks are kernel backstops, not things a user does: a bind
//! source swapped after its check, a mount that fails partway, a trusted
//! placeholder replaced by a symlink, a store output that is a symlink, and
//! the mount helper's capabilities.
use super::*;
use goblins_runtime::binds::Declarations;
use std::io::Read;
use std::os::unix::fs::MetadataExt;

fn fixture(variable: &str) -> PathBuf {
    std::env::var_os(variable)
        .map(PathBuf::from)
        .unwrap_or_else(|| panic!("{variable} is unset; run host tests with `just test-rust-host`"))
}

/// The runtime fixture: the shell, helper and packages a session starts with,
/// and the Python interpreter its probes run with.
fn runtime() -> (Launch, PathBuf) {
    let text = fs::read(fixture("GOBLINS_TEST_RUNTIME")).unwrap();
    let value: Value = serde_json::from_slice(&text).unwrap();
    let python = PathBuf::from(value["python"].as_str().unwrap());
    (serde_json::from_value(value).unwrap(), python)
}

/// A started session and a line reader on its shell's output.
struct Shell {
    session: Session,
    python: PathBuf,
    pending: Vec<u8>,
}

impl Shell {
    fn start(configure: impl FnOnce(&mut Session)) -> Self {
        let (launch, python) = runtime();
        let mut session = Session::new(launch, None, Cancel::default()).unwrap();
        configure(&mut session);
        {
            let _forking = unix::forking();
            session.start(None).unwrap();
        }
        Self {
            session,
            python,
            pending: vec![],
        }
    }

    fn send(&mut self, line: &str) {
        let input = self.session.input.as_mut().unwrap();
        input.write_all(line.as_bytes()).unwrap();
        input.write_all(b"\n").unwrap();
    }

    /// The shell's next line of output, within 15 seconds.
    fn line(&mut self) -> String {
        let deadline = Instant::now() + Duration::from_secs(15);
        let output = self.session.output.as_mut().unwrap();
        loop {
            if let Some(end) = self.pending.iter().position(|b| *b == b'\n') {
                let line: Vec<u8> = self.pending.drain(..=end).collect();
                return String::from_utf8_lossy(&line).trim().to_string();
            }
            let left = deadline.saturating_duration_since(Instant::now());
            assert!(!left.is_zero(), "the sandbox printed no line in time");
            if unix::readable(output.as_raw_fd(), left.as_millis() as i32).unwrap() {
                let mut bytes = [0; 4096];
                let n = output.read(&mut bytes).unwrap();
                assert!(n > 0, "the sandbox shell closed its output");
                self.pending.extend_from_slice(&bytes[..n]);
            }
        }
    }

    fn run(&mut self, command: &str) -> String {
        self.send(command);
        self.line()
    }

    /// A Python program's one-line JSON answer, run inside the sandbox.
    fn probe(&mut self, code: &str) -> Value {
        let quoted = format!("'{}'", code.replace('\'', r"'\''"));
        let line = self.run(&format!("{} -c {quoted}", self.python.display()));
        serde_json::from_str(&line).unwrap_or_else(|_| panic!("unexpected sandbox output: {line}"))
    }

    fn current_generation(&self) -> PathBuf {
        fs::read_link(self.session.directory.join("packages/current")).unwrap()
    }
}

#[test]
#[ignore = "needs user namespaces and the host store; run with `just test-rust-host`"]
fn a_bind_source_swapped_after_its_check_still_mounts_the_checked_directory() {
    let root = fs::canonicalize(unix::temp_directory().unwrap()).unwrap();
    // A private directory the goblin must never see, and a granted one whose
    // ancestor someone with write access to the shared tree swaps for a link.
    let private = root.join("private");
    fs::create_dir_all(private.join("state")).unwrap();
    fs::write(private.join("state/sentinel"), "PRIVATE-CONTROL-DATA").unwrap();
    let shared = root.join("shared");
    let source = shared.join("parent/state");
    fs::create_dir_all(&source).unwrap();
    fs::write(source.join("original"), "ORIGINAL-GRANT").unwrap();
    let original = fs::metadata(&source).unwrap();
    let declarations = Declarations {
        ro_dirs: vec![source.display().to_string()],
        ..Default::default()
    };
    let plan = crate::binds::plan(
        &declarations,
        std::slice::from_ref(&private),
        &Cancellation::default(),
    )
    .unwrap();
    // Between the check and the mount: the same path now names private state.
    fs::rename(shared.join("parent"), shared.join("saved")).unwrap();
    symlink(&private, shared.join("parent")).unwrap();
    assert_eq!(fs::canonicalize(&source).unwrap(), private.join("state"));
    let mut shell = Shell::start(|session| session.binds = Some(Arc::new(plan)));
    let destination = source.display().to_string();
    // No descriptor in the sandbox refers to the source (Bubblewrap's pinned
    // O_PATH handle is gone), then the mount is the checked directory.
    let seen = shell.probe(&format!(
        r#"import os,json
leaks=[]
for pid in os.listdir("/proc"):
    if not pid.isdigit(): continue
    try: fds=os.listdir(f"/proc/{{pid}}/fd")
    except OSError: continue
    for fd in fds:
        try: s=os.stat(f"/proc/{{pid}}/fd/{{fd}}")
        except OSError: continue
        if (s.st_dev,s.st_ino)==({dev},{ino}): leaks.append(f"{{pid}}/{{fd}}")
print(json.dumps([leaks,open("{destination}/original").read(),os.path.exists("{destination}/sentinel")]))"#,
        dev = original.dev(),
        ino = original.ino(),
    ));
    assert_eq!(seen, json!([[], "ORIGINAL-GRANT", false]));
    // A live bind of the pinned object, not a copy.
    fs::write(shared.join("saved/state/original"), "HOST-UPDATE").unwrap();
    assert_eq!(
        shell.probe(&format!(
            r#"import json; print(json.dumps(open("{destination}/original").read()))"#
        )),
        json!("HOST-UPDATE")
    );
    drop(shell);
    fs::remove_dir_all(root).unwrap();
}

#[test]
#[ignore = "needs user namespaces and the host store; run with `just test-rust-host`"]
fn a_granted_output_whose_closure_holds_a_symlink_output_runs() {
    let tool = fixture("GOBLINS_TEST_SYMLINK_TOOL");
    let mut shell = Shell::start(|_| ());
    let links: Vec<_> = shell
        .session
        .closure(std::slice::from_ref(&tool))
        .unwrap()
        .difference(&shell.session.mounted)
        .filter(|p| p.is_symlink())
        .cloned()
        .collect();
    assert_eq!(
        links.len(),
        1,
        "the fixture newly exposes one symlink-shaped output"
    );
    shell.session.grant("symlink-tool", Some(&tool)).unwrap();
    assert!(shell.session.alive());
    assert_eq!(shell.run("symlink-tool"), "symlink-output-ok");
    assert_eq!(
        shell.probe(&format!(
            r#"import os,json; print(json.dumps(os.path.isdir("{}/bin")))"#,
            links[0].display()
        )),
        json!(true)
    );
}

#[test]
#[ignore = "needs user namespaces and the host store; run with `just test-rust-host`"]
fn a_grant_that_fails_after_a_mount_keeps_the_session_and_a_retry_completes_it() {
    let jq = fixture("GOBLINS_TEST_JQ");
    let mut shell = Shell::start(|_| ());
    let error = shell
        .session
        .grant_with("jq", Some(&jq), |index, _| {
            if index == 1 {
                return Err("injected partial mount failure".into());
            }
            Ok(())
        })
        .unwrap_err();
    assert!(error.to_string().contains("partial"), "{error}");
    // The paths mounted so far stay; nothing was published.
    assert!(shell.session.alive());
    assert_eq!(shell.current_generation(), Path::new("generation-0"));
    shell.session.grant("jq", Some(&jq)).unwrap();
    assert_eq!(shell.run("jq -nc '{retried:true}'"), r#"{"retried":true}"#);
    // The grant is protected from the host's garbage collection.
    let rooted = shell
        .session
        .directory
        .join("roots")
        .join(jq.file_name().unwrap());
    assert!(rooted.is_symlink());
    let roots = Command::new("nix-store")
        .args(["--query", "--roots"])
        .arg(&jq)
        .output()
        .unwrap();
    assert!(
        String::from_utf8_lossy(&roots.stdout).contains(&rooted.display().to_string()),
        "{}",
        String::from_utf8_lossy(&roots.stdout)
    );
}

#[test]
#[ignore = "needs user namespaces and the host store; run with `just test-rust-host`"]
fn a_trusted_placeholder_replaced_by_a_symlink_is_not_mounted_through() {
    let jq = fixture("GOBLINS_TEST_JQ");
    let mut shell = Shell::start(|_| ());
    // Corrupt the TRUSTED side: even if destinations were mutable, the helper
    // opens them with openat2 and refuses symbolic links.
    let error = shell
        .session
        .grant_with("jq", Some(&jq), |index, destination| {
            if index == 1 {
                if destination.is_dir() {
                    fs::remove_dir(destination)?;
                } else {
                    fs::remove_file(destination)?;
                }
                symlink("/tmp", destination)?;
            }
            Ok(())
        })
        .unwrap_err()
        .to_string();
    assert!(
        error.contains("could not mount") && error.contains("symbolic links"),
        "{error}"
    );
    // The helper refuses that one mount and keeps serving.
    assert!(shell.session.alive());
    assert_eq!(shell.current_generation(), Path::new("generation-0"));
    assert_eq!(shell.run("echo alive"), "alive");
}

#[test]
#[ignore = "needs user namespaces and the host store; run with `just test-rust-host`"]
fn the_mount_helper_keeps_only_the_capabilities_it_mounts_with() {
    let shell = Shell::start(|_| ());
    let pid = shell.session.helper_pid().unwrap();
    let status = fs::read_to_string(format!("/proc/{pid}/status")).unwrap();
    let effective = status
        .lines()
        .find_map(|line| line.strip_prefix("CapEff:"))
        .map(|caps| u64::from_str_radix(caps.trim(), 16).unwrap())
        .unwrap();
    // CAP_SYS_ADMIN (21) to mount and CAP_SYS_CHROOT (18) to enter the
    // sandbox root.
    assert_eq!(effective, (1 << 21) | (1 << 18));
}
