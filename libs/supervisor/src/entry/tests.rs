//! Private child modes tested beside the entry point. The subprocess is this
//! test executable calling the real entry function, with only fd 3 inherited.
use goblins_runtime::channel::Channel;
use goblins_unix as unix;
use serde_json::{Value, json};
use std::{
    fs::{self, File},
    io::Read,
    os::{
        fd::{AsRawFd, OwnedFd},
        unix::{net::UnixStream, process::CommandExt},
    },
    path::{Path, PathBuf},
    process::{Child, Command, ExitStatus, Stdio},
    time::{Duration, Instant},
};

#[test]
#[ignore = "subprocess entry fixture"]
fn peer() {
    std::process::exit(if super::main().is_ok() { 0 } else { 1 });
}

struct Peer {
    child: Child,
    control: Channel,
    root: PathBuf,
}
impl Peer {
    fn start(
        root: PathBuf,
        method: &str,
        params: Value,
        mut fds: Vec<OwnedFd>,
        suffix: &[u8],
        nofile: bool,
    ) -> Self {
        let (parent, child) = UnixStream::pair().unwrap();
        let mut control = Channel::new(parent).unwrap();
        control
            .queue(
                &json!({"jsonrpc":"2.0","method":method,"params":params}),
                &mut fds,
            )
            .unwrap();
        control.flush().unwrap();
        assert!(!control.wants_write());
        if !suffix.is_empty() {
            assert_eq!(
                unsafe {
                    libc::send(
                        control.fd().unwrap(),
                        suffix.as_ptr().cast(),
                        suffix.len(),
                        libc::MSG_NOSIGNAL,
                    )
                },
                suffix.len() as isize
            );
        }
        let mut command = Command::new(std::env::current_exe().unwrap());
        command
            .args([
                "--exact",
                "entry::tests::peer",
                "--ignored",
                "--test-threads=1",
            ])
            .env("XDG_CACHE_HOME", root.join("cache"))
            .env("XDG_STATE_HOME", root.join("history"))
            .current_dir(&root)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        let fd = child.as_raw_fd();
        unsafe {
            command.pre_exec(move || {
                unix::cvt(libc::dup2(fd, 3))?;
                if nofile {
                    unix::cvt(libc::setrlimit(
                        libc::RLIMIT_NOFILE,
                        &libc::rlimit {
                            rlim_cur: 256,
                            rlim_max: 512,
                        },
                    ))?;
                }
                Ok(())
            });
        }
        let child = {
            let _fork = unix::forking();
            command.spawn().unwrap()
        };
        Self {
            child,
            control,
            root,
        }
    }
    fn send(&mut self, method: &str, params: Value, mut fds: Vec<OwnedFd>) {
        self.control
            .queue(
                &json!({"jsonrpc":"2.0","method":method,"params":params}),
                &mut fds,
            )
            .unwrap();
        self.control.flush().unwrap();
        assert!(!self.control.wants_write());
    }
    fn receive(&mut self) -> Option<Value> {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            if let Some(frame) = self.control.receive().unwrap() {
                assert!(frame.fds.is_empty());
                return Some(frame.value);
            }
            if self.control.closed() {
                return None;
            }
            assert!(Instant::now() < deadline, "child did not answer");
            std::thread::sleep(Duration::from_millis(2));
        }
    }
    fn drain(&mut self) -> Vec<Value> {
        let mut messages = vec![];
        while let Some(message) = self.receive() {
            messages.push(message);
        }
        messages
    }
    fn exit(&mut self) -> ExitStatus {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            if let Some(status) = self.child.try_wait().unwrap() {
                return status;
            }
            assert!(Instant::now() < deadline, "child did not exit");
            std::thread::sleep(Duration::from_millis(2));
        }
    }
}
impl Drop for Peer {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        let _ = fs::remove_dir_all(&self.root);
    }
}
fn fixture() -> PathBuf {
    unix::temp_directory().unwrap()
}
fn start(root: &Path) -> Value {
    json!({"supervisor_api":1,"source":{"kind":"Host","configuration":root.join("absent.json"),"name":"probe","scope":null,"dev_shell":null,"workspace":null,"cwd":null,"state":root,"history":null,"recreate":null,"sequence":1},"directory":root.join("runtime"),"dimensions":[24,80],"detached":true,"payload_nofile":null,"handoff":null})
}
fn missing_configuration(root: &Path) -> String {
    format!(
        "cannot open configuration {}: {}",
        root.join("absent.json").display(),
        std::io::Error::from_raw_os_error(libc::ENOENT)
    )
}
fn preflight(root: &Path) -> Value {
    fs::write(
        root.join("manifest.json"),
        json!({"api":2,"helper_api":6,"goblins":{}}).to_string(),
    )
    .unwrap();
    json!({"supervisor_api":1,"path":"manifest.json","directory":root.join("manifests"),"sequence":17})
}

#[test]
fn an_unjoined_scope_reports_idle_but_only_stop_ends_it() {
    let root = fixture();
    let directory = root.join("state/scopes/probe");
    let mut peer = Peer::start(
        root,
        "Setup",
        json!({"api":1,"name":"probe","directory":directory}),
        vec![],
        &[],
        false,
    );
    let (member, child) = UnixStream::pair().unwrap();
    peer.send("AddMember", json!({"fd":{"$fd":0}}), vec![child.into()]);
    drop(member);
    assert_eq!(
        peer.receive().unwrap(),
        json!({"jsonrpc":"2.0","method":"Idle","params":{"added":1}})
    );
    assert!(peer.child.try_wait().unwrap().is_none());
    peer.send("Stop", json!({}), vec![]);
    assert_eq!(peer.receive().unwrap()["method"], "Stopping");
    assert!(peer.exit().success());
    assert!(directory.join("lock").is_file());
}

#[test]
fn invalid_startup_modes_and_scalars_have_no_runtime_effects() {
    for changes in [
        json!({"supervisor_api":999}),
        json!({"dimensions":[0,80]}),
        json!({"directory":"relative"}),
        json!({"unexpected":true}),
        json!({"source":{"kind":"Member","scope":null,"owner_scope":null,"manifest":"/absent","configuration":"probe","auto":{},"state":"/unused","history":null}}),
    ] {
        let root = fixture();
        let mut params = start(&root);
        params
            .as_object_mut()
            .unwrap()
            .extend(changes.as_object().unwrap().clone());
        let mut peer = Peer::start(root, "SupervisorStart", params, vec![], &[], false);
        assert!(!peer.exit().success());
        assert!(!peer.root.join("runtime").exists());
        assert!(!peer.root.join("terminal.sock").exists());
    }
    let root = fixture();
    let directory = root.join("scope");
    let mut peer = Peer::start(
        root,
        "Setup",
        json!({"api":999,"name":"probe","directory":directory}),
        vec![],
        &[],
        false,
    );
    assert!(!peer.exit().success());
    assert!(!directory.exists());
    let root = fixture();
    let (reader, writer) = unix::pipe().unwrap();
    let mut peer = Peer::start(
        root,
        "Event.Mode",
        json!({"server":false,"invalid":{"$fd":0}}),
        vec![writer],
        &[],
        false,
    );
    assert!(!peer.exit().success());
    assert!(unix::readable(reader.as_raw_fd(), 100).unwrap());
    assert_eq!(File::from(reader).read(&mut [0]).unwrap(), 0);
}

#[test]
fn physical_payload_limits_precede_engine_creation_and_handoff_adoption() {
    let mut original: libc::rlimit = unsafe { std::mem::zeroed() };
    assert_eq!(
        unsafe { libc::getrlimit(libc::RLIMIT_NOFILE, &mut original) },
        0
    );
    for limit in [257, 513, 128] {
        let root = fixture();
        let resources = root.join("owned");
        unix::private_directory(&resources).unwrap();
        fs::write(resources.join("held"), "sender-owned").unwrap();
        let lock = resources.join(".gc-lease");
        let owner = unix::lock(&lock).unwrap();
        assert_eq!(unsafe { libc::flock(owner.as_raw_fd(), libc::LOCK_SH) }, 0);
        let transfer = File::options().read(true).write(true).open(&lock).unwrap();
        assert_eq!(
            unsafe { libc::flock(transfer.as_raw_fd(), libc::LOCK_SH | libc::LOCK_NB) },
            0
        );
        let (mut scope, child) = UnixStream::pair().unwrap();
        let mut params = start(&root);
        params["payload_nofile"] = json!(limit);
        params["source"] = json!({"kind":"Member","scope":{"$fd":0},"owner_scope":null,"manifest":root.join("absent.json"),"configuration":"probe","auto":{},"state":root,"history":null});
        params["handoff"] = json!({"transfer":99,"count":1,"workspace":{"$fd":1},"working_directory":null,"dev_shell":"null","docker":false,"leases":[{"directory":resources,"lock":{"$fd":2}}],"more":false});
        let workspace = File::open(&root).unwrap();
        let mut peer = Peer::start(
            root,
            "SupervisorStart",
            params,
            vec![child.into(), workspace.into(), transfer.into()],
            &[],
            true,
        );
        let messages = peer.drain();
        assert_eq!(peer.exit().success(), limit == 128);
        if limit == 128 {
            assert_eq!(
                messages
                    .iter()
                    .filter(|m| m["method"] == "Adopted")
                    .map(|m| m["params"].clone())
                    .collect::<Vec<_>>(),
                vec![json!({"transfer":99})]
            );
            assert_eq!(
                messages
                    .iter()
                    .filter(|m| m["method"] == "Event.Failed")
                    .count(),
                1
            );
            assert_eq!(
                messages
                    .iter()
                    .filter(|m| m["method"] == "Released")
                    .count(),
                1
            );
            let failure = messages
                .iter()
                .find(|m| m["method"] == "Event.Failed")
                .unwrap();
            assert_eq!(
                failure["params"]["error"],
                missing_configuration(&peer.root)
            );
        } else {
            assert!(messages.is_empty(), "invalid limit acknowledged ownership");
            assert!(!peer.root.join("runtime").exists());
            assert!(!peer.root.join("cache").exists());
        }
        assert!(unix::readable(scope.as_raw_fd(), 100).unwrap());
        assert_eq!(scope.read(&mut [0]).unwrap(), 0);
        assert_eq!(
            fs::read_to_string(resources.join("held")).unwrap(),
            "sender-owned"
        );
        assert_eq!(
            unsafe { libc::flock(owner.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) },
            0
        );
        let mut after: libc::rlimit = unsafe { std::mem::zeroed() };
        assert_eq!(
            unsafe { libc::getrlimit(libc::RLIMIT_NOFILE, &mut after) },
            0
        );
        assert_eq!(
            (after.rlim_cur, after.rlim_max),
            (original.rlim_cur, original.rlim_max)
        );
    }
}

#[test]
fn worker_failure_reports_failed_stopped_and_released_in_order() {
    for limit in [None, Some(128)] {
        let root = fixture();
        let mut params = start(&root);
        params["payload_nofile"] = json!(limit);
        let mut peer = Peer::start(root, "SupervisorStart", params, vec![], &[], true);
        let messages = peer.drain();
        assert!(peer.exit().success());
        let methods: Vec<_> = messages
            .iter()
            .map(|m| m["method"].as_str().unwrap())
            .collect();
        for method in ["Event.Failed", "Event.Stopped", "Released"] {
            assert_eq!(methods.iter().filter(|m| **m == method).count(), 1);
        }
        assert!(
            methods.iter().position(|m| *m == "Event.Failed")
                < methods.iter().position(|m| *m == "Event.Stopped")
        );
        assert!(
            methods.iter().position(|m| *m == "Event.Stopped")
                < methods.iter().position(|m| *m == "Released")
        );
        let error = messages
            .iter()
            .find(|m| m["method"] == "Event.Failed")
            .unwrap()["params"]["error"]
            .as_str()
            .unwrap();
        assert_eq!(error, missing_configuration(&peer.root));
    }
}

#[test]
fn preflight_adopts_an_unrooted_relative_manifest_and_rejects_wrong_terminal_input() {
    for mode in [
        "valid",
        "wrong",
        "duplicate",
        "partial",
        "multipart",
        "api",
        "directory",
        "invalid-manifest",
    ] {
        let root = fixture();
        let mut params = preflight(&root);
        if mode == "api" {
            params["supervisor_api"] = json!(2);
        }
        if mode == "directory" {
            params["directory"] = json!("relative");
        }
        if mode == "invalid-manifest" {
            fs::write(
                root.join("manifest.json"),
                json!({"api":1,"helper_api":6,"goblins":{}}).to_string(),
            )
            .unwrap();
        }
        let mut peer = Peer::start(root, "Preflight", params, vec![], &[], false);
        if ["api", "directory"].contains(&mode) {
            assert!(!peer.exit().success());
            assert!(peer.receive().is_none());
        } else if mode == "invalid-manifest" {
            assert_eq!(
                peer.receive().unwrap(),
                json!({"jsonrpc":"2.0","method":"PreflightFailed","params":{"error":"incompatible manifest/helper API"}})
            );
            assert!(peer.exit().success());
            assert!(peer.receive().is_none());
        } else {
            let reply = peer.receive().unwrap();
            assert_eq!(reply["method"], "Event.Manifest");
            let params = &reply["params"];
            assert_eq!(params["path"], "manifest.json");
            assert_eq!(params["sequence"], 17);
            assert!(params["root"].is_null() && params["owner"].is_null());
            let transfer = params["transfer"].as_u64().unwrap();
            assert!(transfer > 0);
            if mode == "multipart" {
                let (reader, writer) = unix::pipe().unwrap();
                peer.send("Command.Start",json!({"handoff":{"transfer":99,"count":1,"workspace":{"$fd":0},"working_directory":null,"dev_shell":"null","docker":false,"leases":[],"more":true}}),vec![writer]);
                assert!(peer.receive().is_none());
                assert!(unix::readable(reader.as_raw_fd(), 0).unwrap());
                assert_eq!(File::from(reader).read(&mut [0]).unwrap(), 0);
            } else if ["duplicate", "partial"].contains(&mode) {
                let body =
                    json!({"jsonrpc":"2.0","method":"Adopted","params":{"transfer":transfer}})
                        .to_string();
                let mut bytes = format!(
                    "Content-Length: {}\r\nDescriptor-Count: 0\r\n\r\n{body}",
                    body.len()
                )
                .into_bytes();
                if mode == "duplicate" {
                    bytes.extend(bytes.clone());
                } else {
                    bytes.push(b'C');
                }
                assert_eq!(
                    unsafe {
                        libc::send(
                            peer.control.fd().unwrap(),
                            bytes.as_ptr().cast(),
                            bytes.len(),
                            libc::MSG_NOSIGNAL,
                        )
                    },
                    bytes.len() as isize
                );
            } else {
                peer.send(
                    "Adopted",
                    json!({"transfer":transfer + u64::from(mode == "wrong")}),
                    vec![],
                );
            }
            assert_eq!(peer.exit().success(), mode == "valid");
            if mode == "valid" {
                assert!(peer.receive().is_none());
            }
        }
        assert!(!peer.root.join("manifests").exists());
        assert!(!peer.root.join("terminal.sock").exists());
    }
}

#[test]
fn maintenance_uses_explicit_output_and_keeps_operation_errors_in_the_protocol() {
    for mode in [
        "valid",
        "invalid-name",
        "api",
        "operation",
        "extra",
        "readonly",
        "cancel",
        "broken",
    ] {
        let root = fixture();
        let selected = root.join("cache/goblins/scopes/scope-work");
        if mode == "valid" {
            for path in [
                root.join("cache"),
                root.join("cache/goblins"),
                root.join("cache/goblins/scopes"),
                selected.clone(),
            ] {
                unix::private_directory(&path).unwrap();
            }
            fs::write(selected.join("payload"), "keep").unwrap();
        }
        let stdout = root.join("stdout");
        let output = File::create(&stdout).unwrap();
        let flags = unsafe { libc::fcntl(output.as_raw_fd(), libc::F_GETFL) };
        let error = File::create(root.join("stderr")).unwrap();
        let fd: OwnedFd = if mode == "readonly" {
            File::open(&stdout).unwrap().into()
        } else if mode == "broken" {
            let (read, write) = unix::pipe().unwrap();
            drop(read);
            write
        } else {
            output.try_clone().unwrap().into()
        };
        let mut params = json!({"supervisor_api":1,"operation":{"kind":"ClearScope","runtime":null,"name":if mode == "invalid-name" { json!("../invalid") } else if mode == "valid" { json!("work") } else { Value::Null },"dry_run":true},"stdout":{"$fd":0},"stderr":{"$fd":1}});
        if mode == "api" {
            params["supervisor_api"] = json!(2);
        }
        if mode == "operation" {
            params["operation"] = json!({"kind":"Unknown"});
        }
        if mode == "extra" {
            params["extra"] = json!(true);
        }
        let mut peer = Peer::start(
            root,
            "Maintenance",
            params,
            vec![fd, error.into()],
            if mode == "cancel" { b"C" } else { b"" },
            false,
        );
        if ["api", "operation", "extra", "readonly", "cancel"].contains(&mode) {
            assert!(!peer.exit().success());
            assert!(!peer.root.join("cache").exists());
            assert_eq!(fs::read(&stdout).unwrap(), b"");
        } else {
            let reply = peer.receive().unwrap();
            assert_eq!(reply["method"], "MaintenanceFinished");
            if mode == "invalid-name" {
                assert_eq!(
                    reply["params"]["result"],
                    json!({"Err":"invalid scope name"})
                );
                assert!(!peer.root.join("cache").exists());
            } else if mode == "broken" {
                assert!(
                    reply["params"]["result"]["Err"]
                        .as_str()
                        .unwrap()
                        .contains("Broken pipe")
                );
            } else {
                assert_eq!(reply["params"]["result"], json!({"Ok":null}));
                assert_eq!(
                    fs::read_to_string(&stdout).unwrap(),
                    format!("Would remove {}\n", selected.display())
                );
                assert_eq!(
                    fs::read_to_string(selected.join("payload")).unwrap(),
                    "keep"
                );
            }
            assert!(peer.exit().success());
        }
        assert_eq!(
            unsafe { libc::fcntl(output.as_raw_fd(), libc::F_GETFL) },
            flags
        );
        assert_eq!(fs::read(peer.root.join("stderr")).unwrap(), b"");
        assert!(!peer.root.join("terminal.sock").exists());
    }
}
