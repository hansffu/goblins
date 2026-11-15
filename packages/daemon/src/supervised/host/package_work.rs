//! Real daemon and supervisor coverage for package work that cannot finish.
use super::{Root, Run};
use crate::host::Client;
use goblins_jsonrpc as rpc;
use goblins_protocol::contract::{self, EndpointRole};
use goblins_unix as unix;
use serde_json::{Value, json};
use std::{
    fs,
    io::Write,
    os::{unix::fs::PermissionsExt, unix::net::UnixStream},
    path::{Path, PathBuf},
    process::{Command, Stdio},
    time::{Duration, Instant},
};

fn until(timeout: Duration, mut condition: impl FnMut() -> bool) {
    let deadline = Instant::now() + timeout;
    while !condition() {
        assert!(
            Instant::now() < deadline,
            "blocked package lifecycle timed out"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
}

fn executable(name: &str) -> PathBuf {
    std::env::split_paths(&std::env::var_os("PATH").unwrap())
        .map(|directory| directory.join(name))
        .find(|path| path.is_file())
        .unwrap_or_else(|| panic!("{name} missing from test PATH"))
}

struct Sandbox {
    stream: UnixStream,
    next: u64,
}
impl Sandbox {
    fn connect(state: &Path, session: &str) -> Self {
        let mut stream =
            UnixStream::connect(state.join(session).join("resources/request.sock")).unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(10)))
            .unwrap();
        contract::initialize(&mut stream, json!(1), EndpointRole::Sandbox).unwrap();
        Self { stream, next: 1 }
    }
    fn call(&mut self, method: &str, params: Value) -> Value {
        self.next += 1;
        rpc::exchange(&mut self.stream, json!(self.next), method, params).unwrap()
    }
    fn request(&mut self, package: &str) {
        self.next += 1;
        let frame = rpc::request(
            json!(self.next),
            "permissions.request",
            json!({
                "kind":"package", "package":package, "reason":"blocked worker test"
            }),
        );
        self.stream
            .write_all(&rpc::encode(&frame).unwrap())
            .unwrap();
    }
}

struct Fixture {
    // Stop the daemon before removing the private fixture directory, also on panic.
    _run: Run,
    client: Client,
    _root: Root,
    state: PathBuf,
    manifest: PathBuf,
    marker: PathBuf,
}
impl Fixture {
    fn new(kind: &str) -> Self {
        let root = Root(unix::temp_directory().unwrap());
        let state = root.0.join("state");
        let manifest = root.0.join("manifest.json");
        let marker = root.0.join("blocked");
        let bin = root.0.join("bin");
        fs::create_dir(&bin).unwrap();
        let real_nix = executable("nix");
        let python = executable("python3");
        let sleep = executable("sleep");
        let script = format!(
            "#!{}\nimport json, os, subprocess, sys, time\nargs = sys.argv[1:]\nblocked = ('eval' in args and any('.hello' in a for a in args)) if {} == 'preview' else ('build' in args and '--dry-run' not in args)\nif blocked and not os.path.exists({}):\n child = subprocess.Popen([{}, '60'])\n with open({} + '.tmp', 'w') as marker: marker.write(json.dumps([os.getpid(), child.pid]))\n os.replace({} + '.tmp', {})\n for _ in range(6000):\n  if os.path.exists({}): break\n  time.sleep(.01)\nos.execv({}, [{}, *args])\n",
            python.display(),
            json!(kind),
            json!(marker),
            json!(sleep),
            json!(marker),
            json!(marker),
            json!(marker),
            json!(marker.with_extension("release")),
            json!(real_nix),
            json!(real_nix),
        );
        let wrapper = bin.join("nix");
        fs::write(&wrapper, script).unwrap();
        fs::set_permissions(&wrapper, fs::Permissions::from_mode(0o700)).unwrap();
        let fixture =
            std::env::var_os("GOBLINS_TEST_EXEC_MANIFEST").expect("run just test-rust-host");
        let mut config: Value = serde_json::from_slice(&fs::read(fixture).unwrap()).unwrap();
        config["goblins"]["shell"]["args"] = json!([
            "--noprofile",
            "--norc",
            "-c",
            "exec /bin/sh -c 'read -r ready; exit 7'"
        ]);
        config["goblins"]["shell"]["allowed_run_targets"] = json!(["shell"]);
        fs::write(&manifest, config.to_string()).unwrap();
        let log = root.0.join("server.log");
        let output = fs::File::create(&log).unwrap();
        let mut command =
            Command::new(std::env::var_os("GOBLINS_TEST_CONTROLLER_EXECUTABLE").unwrap());
        command
            .arg("--runtime")
            .arg(&manifest)
            .arg("--state-dir")
            .arg(&state)
            .args(["server", "start", "--foreground"])
            .env(
                "GOBLINS_SUPERVISOR_RUNTIME",
                std::env::var_os("GOBLINS_TEST_SUPERVISOR_EXECUTABLE").unwrap(),
            )
            .env("XDG_CACHE_HOME", root.0.join("cache"))
            .env("XDG_STATE_HOME", root.0.join("history"))
            .env(
                "PATH",
                std::env::join_paths(
                    std::iter::once(bin)
                        .chain(std::env::split_paths(&std::env::var_os("PATH").unwrap())),
                )
                .unwrap(),
            )
            .stdin(Stdio::null())
            .stdout(Stdio::from(output.try_clone().unwrap()))
            .stderr(Stdio::from(output));
        let child = {
            let _fork = unix::forking();
            command.spawn().unwrap()
        };
        let mut run = Run { child, peer: None };
        until(Duration::from_secs(20), || {
            assert!(
                run.child.try_wait().unwrap().is_none(),
                "{}",
                fs::read_to_string(&log).unwrap()
            );
            fs::read_to_string(&log).unwrap().contains("daemon ready")
        });
        let client = Client::connect(&state).unwrap();
        Self {
            _run: run,
            client,
            _root: root,
            state,
            manifest,
            marker,
        }
    }
    fn start(&mut self, name: &str) -> String {
        let launch = self
            .client
            .call(
                "sessions.start",
                json!({
                    "key":name, "name":"shell", "agent_name":name,
                    "configuration":self.manifest, "rows":24, "cols":100,
                }),
            )
            .unwrap();
        let session = launch["session"].as_str().unwrap().to_owned();
        self.running(&session);
        session
    }
    fn record(&mut self, session: &str) -> Value {
        self.client
            .call("sessions.get", json!({"session":session}))
            .unwrap()
    }
    fn running(&mut self, session: &str) {
        until(Duration::from_secs(90), || {
            let record = self.record(session);
            assert_ne!(record["state"], "failed", "{record}");
            record["state"] == "running"
        });
    }
    fn pending(&mut self, session: &str) -> Value {
        let mut record = None;
        until(Duration::from_secs(20), || {
            record = self
                .client
                .call(
                    "permissions.list",
                    json!({"session":session,"state":"pending"}),
                )
                .unwrap()
                .as_array()
                .unwrap()
                .first()
                .cloned();
            record.is_some()
        });
        record.unwrap()
    }
    fn decide(&mut self, record: &Value, approved: bool) {
        self.client
            .call(
                "permissions.decide",
                json!({
                    "session":record["session"], "request":record["id"],
                    "approval":record["approval"], "approved":approved,
                }),
            )
            .unwrap();
    }
    fn blocked_pids(&self) -> [u32; 2] {
        until(Duration::from_secs(20), || self.marker.exists());
        let pids: Vec<u32> = serde_json::from_slice(&fs::read(&self.marker).unwrap()).unwrap();
        pids.try_into().unwrap()
    }
    fn reaped(&self, pids: [u32; 2]) {
        until(Duration::from_secs(5), || {
            !Path::new(&format!("/proc/{}", pids[0])).exists()
        });
        for pid in pids {
            let stat = fs::read_to_string(format!("/proc/{pid}/stat"));
            if let Ok(stat) = stat {
                let state = stat
                    .rsplit_once(')')
                    .unwrap()
                    .1
                    .split_whitespace()
                    .next()
                    .unwrap();
                assert_eq!(state, "Z", "owned process {pid} still runs");
            }
        }
    }
    fn stop(&mut self, session: &str) {
        self.client
            .call("sessions.stop", json!({"session":session}))
            .unwrap();
        until(Duration::from_secs(5), || {
            self.record(session)["state"] == "stopped"
        });
    }
}

fn owner_exits_during_blocked_package_work(kind: &str) {
    let mut fixture = Fixture::new(kind);
    let owner = fixture.start("owner");
    let other = fixture.start("other");
    Sandbox::connect(&fixture.state, &owner).call("raids.create", json!({"raid":"crew"}));
    let member = Sandbox::connect(&fixture.state, &owner).call(
        "sessions.start",
        json!({
            "key":"member", "name":"shell", "detached":true
        }),
    )["session"]
        .as_str()
        .unwrap()
        .to_owned();
    fixture.running(&member);
    let mut owner_rpc = Sandbox::connect(&fixture.state, &owner);
    owner_rpc.request("hello");
    let owner_request = fixture.pending(&owner);
    if kind == "build" {
        fixture.decide(&owner_request, true);
    }
    let pids = fixture.blocked_pids();
    assert_eq!(
        fixture
            .client
            .call("permissions.get", json!({"request":owner_request["id"]}))
            .unwrap()["state"],
        if kind == "build" {
            "realizing"
        } else {
            "pending"
        }
    );
    let mut other_rpc = Sandbox::connect(&fixture.state, &other);
    other_rpc.request("tree");
    let other_request = fixture.pending(&other);
    let started = Instant::now();
    fixture.decide(&other_request, false);
    let reply = rpc::read(&mut other_rpc.stream).unwrap();
    assert_eq!(reply["result"]["status"], "denied", "{reply}");
    assert!(started.elapsed() < Duration::from_secs(5));
    let mut terminal =
        UnixStream::connect(fixture.record(&owner)["terminal"].as_str().unwrap()).unwrap();
    terminal
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    assert_eq!(
        rpc::read(&mut terminal).unwrap()["result"]["attached"],
        true
    );
    terminal.write_all(b"go\n").unwrap();
    until(Duration::from_secs(5), || {
        fixture.record(&owner)["state"] == "stopped"
    });
    assert_eq!(fixture.record(&owner)["exit_code"], 7);
    fixture.reaped(pids);
    assert_eq!(fixture.record(&member)["state"], "running");
    assert_eq!(fixture.record(&other)["state"], "running");
    assert!(fixture.state.join(&owner).join("resources").exists());
    fixture.stop(&member);
    until(Duration::from_secs(5), || {
        !fixture.state.join(&owner).join("resources").exists()
    });
    fixture.stop(&other);
}

#[test]
#[ignore = "needs user namespaces and the host store; run just test-rust-host"]
fn blocked_preview_allows_other_approval_and_owner_exit() {
    owner_exits_during_blocked_package_work("preview");
}

#[test]
#[ignore = "needs user namespaces and the host store; run just test-rust-host"]
fn blocked_build_allows_other_approval_and_owner_exit() {
    owner_exits_during_blocked_package_work("build");
}

#[test]
#[ignore = "needs user namespaces and the host store; run just test-rust-host"]
fn blocked_approved_build_allows_member_launch_and_owner_stop() {
    let mut fixture = Fixture::new("build");
    let owner = fixture.start("owner");
    let other = fixture.start("other");
    Sandbox::connect(&fixture.state, &owner).call("raids.create", json!({"raid":"crew"}));
    let mut owner_rpc = Sandbox::connect(&fixture.state, &owner);
    owner_rpc.request("hello");
    let owner_request = fixture.pending(&owner);
    fixture.decide(&owner_request, true);
    let pids = fixture.blocked_pids();
    assert_eq!(
        fixture
            .client
            .call("permissions.get", json!({"request":owner_request["id"]}))
            .unwrap()["state"],
        "realizing"
    );
    let member = Sandbox::connect(&fixture.state, &owner).call(
        "sessions.start",
        json!({
            "key":"member", "name":"shell", "detached":true
        }),
    )["session"]
        .as_str()
        .unwrap()
        .to_owned();
    fixture.running(&member);
    assert!(Path::new(&format!("/proc/{}", pids[0])).exists());
    let mut other_rpc = Sandbox::connect(&fixture.state, &other);
    other_rpc.request("tree");
    let other_request = fixture.pending(&other);
    let started = Instant::now();
    fixture.decide(&other_request, false);
    let reply = rpc::read(&mut other_rpc.stream).unwrap();
    assert_eq!(reply["result"]["status"], "denied", "{reply}");
    assert!(started.elapsed() < Duration::from_secs(5));
    fixture.stop(&owner);
    fixture.reaped(pids);
    drop(owner_rpc);
    assert!(fixture.state.join(&owner).join("resources").exists());
    assert_eq!(fixture.record(&member)["state"], "running");
    assert_eq!(fixture.record(&other)["state"], "running");
    fixture.stop(&member);
    until(Duration::from_secs(5), || {
        !fixture.state.join(&owner).join("resources").exists()
    });
    fixture.stop(&other);
}

#[test]
#[ignore = "needs user namespaces and the host store; run just test-rust-host"]
fn disconnect_cancels_blocked_preview_and_allows_the_next_request() {
    let mut fixture = Fixture::new("preview");
    let owner = fixture.start("owner");
    let other = fixture.start("other");
    let mut requester = Sandbox::connect(&fixture.state, &owner);
    requester.request("hello");
    let record = fixture.pending(&owner);
    let pids = fixture.blocked_pids();
    requester.stream.write_all(b"x").unwrap();
    drop(requester);
    until(Duration::from_secs(5), || {
        fixture
            .client
            .call("permissions.get", json!({"request":record["id"]}))
            .unwrap()["state"]
            == "withdrawn"
    });
    fixture.reaped(pids);
    let mut next = Sandbox::connect(&fixture.state, &owner);
    next.request("tree");
    let pending = fixture.pending(&owner);
    fixture.decide(&pending, false);
    let reply = rpc::read(&mut next.stream).unwrap();
    assert_eq!(reply["result"]["status"], "denied", "{reply}");
    assert_eq!(fixture.record(&other)["state"], "running");
    fixture.stop(&owner);
    fixture.stop(&other);
}
