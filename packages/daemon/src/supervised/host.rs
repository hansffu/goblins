//! Kernel checks of the real CLI: process groups, SIGINT, TOSTOP and
//! foreground-supervisor descriptor limits. Ordinary protocol cases are units.
use crate::supervised::tests::executable;
use goblins_unix as unix;
use serde_json::Value;
use std::{
    fs,
    io::Read,
    os::{
        fd::{AsRawFd, OwnedFd},
        unix::process::CommandExt,
    },
    path::{Path, PathBuf},
    process::{Child, Command, ExitStatus, Stdio},
    time::{Duration, Instant},
};
mod package_work;
struct Root(PathBuf);
impl Drop for Root {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}
struct Run {
    child: Child,
    peer: Option<OwnedFd>,
}
impl Drop for Run {
    fn drop(&mut self) {
        if let Some(peer) = &self.peer {
            unsafe {
                libc::syscall(
                    libc::SYS_pidfd_send_signal,
                    peer.as_raw_fd(),
                    libc::SIGKILL,
                    std::ptr::null::<libc::siginfo_t>(),
                    0,
                );
            }
        }
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}
impl Run {
    fn start(root: &Path, mode: &str, tty: Option<&OwnedFd>) -> Self {
        let controller = std::env::var_os("GOBLINS_TEST_CONTROLLER_EXECUTABLE")
            .expect("run CLI host tests with just test-rust-host");
        let pin = executable(root, mode);
        let mut command = Command::new(controller);
        command
            .arg("--state-dir")
            .arg(root.join("state"))
            .args(["scope", "clear", "--all", "--dry-run"])
            .env("GOBLINS_SUPERVISOR_RUNTIME", pin.path().unwrap())
            .env("XDG_CACHE_HOME", root.join("cache"))
            .env("XDG_STATE_HOME", root.join("history"));
        if let Some(tty) = tty {
            command
                .stdin(Stdio::from(tty.try_clone().unwrap()))
                .stdout(Stdio::from(tty.try_clone().unwrap()))
                .stderr(Stdio::from(tty.try_clone().unwrap()));
        } else {
            command
                .stdin(Stdio::null())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped());
        }
        let controlling_tty = tty.is_some();
        unsafe {
            command.pre_exec(move || {
                unix::cvt(libc::setsid())?;
                if controlling_tty {
                    libc::signal(libc::SIGTTOU, libc::SIG_DFL);
                    let mut mask: libc::sigset_t = std::mem::zeroed();
                    libc::sigemptyset(&mut mask);
                    libc::sigaddset(&mut mask, libc::SIGTTOU);
                    unix::cvt(libc::sigprocmask(
                        libc::SIG_UNBLOCK,
                        &mask,
                        std::ptr::null_mut(),
                    ))?;
                    unix::cvt(libc::ioctl(0, libc::TIOCSCTTY, 0))?;
                }
                Ok(())
            });
        }
        let child = {
            let _fork = unix::forking();
            command.spawn().unwrap()
        };
        Self { child, peer: None }
    }
    fn ready(&mut self, root: &Path) -> Value {
        let deadline = Instant::now() + Duration::from_secs(10);
        let path = root.join("cli-ready.json");
        while !path.exists() {
            assert!(
                self.child.try_wait().unwrap().is_none(),
                "CLI exited before its peer was ready"
            );
            assert!(Instant::now() < deadline, "maintenance peer did not start");
            std::thread::sleep(Duration::from_millis(2));
        }
        let value: Value = serde_json::from_slice(&fs::read(path).unwrap()).unwrap();
        self.peer = Some(
            unix::owned(unsafe {
                libc::syscall(
                    libc::SYS_pidfd_open,
                    value["pid"].as_u64().unwrap() as libc::pid_t,
                    0,
                ) as i32
            })
            .unwrap(),
        );
        value
    }
    fn exit(&mut self) -> ExitStatus {
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            if let Some(status) = self.child.try_wait().unwrap() {
                return status;
            }
            assert!(Instant::now() < deadline, "maintenance CLI did not exit");
            std::thread::sleep(Duration::from_millis(2));
        }
    }
}

#[test]
#[ignore = "needs the real CLI and host process groups; run just test-rust-host"]
fn sigint_cancels_a_separately_grouped_output_blocked_supervisor() {
    let root = Root(unix::temp_directory().unwrap());
    let mut run = Run::start(&root.0, "maintenance-cli-cancel", None);
    let peer = run.ready(&root.0);
    let pid = run.child.id() as libc::pid_t;
    assert_eq!(unsafe { libc::getpgid(pid) }, pid);
    assert_eq!(peer["pgid"], peer["pid"]);
    assert_ne!(peer["pgid"], pid);
    let started = Instant::now();
    assert_eq!(unsafe { libc::kill(-pid, libc::SIGINT) }, 0);
    assert_eq!(run.exit().code(), Some(1));
    assert!(started.elapsed() < Duration::from_millis(3500));
    assert!(
        unix::readable(run.peer.as_ref().unwrap().as_raw_fd(), 0).unwrap(),
        "supervisor still executing"
    );
    let mut output = vec![];
    run.child
        .stdout
        .take()
        .unwrap()
        .read_to_end(&mut output)
        .unwrap();
    assert!(!output.is_empty() && output.len() < 4 * 1024 * 1024);
    let mut error = vec![];
    run.child
        .stderr
        .take()
        .unwrap()
        .read_to_end(&mut error)
        .unwrap();
    assert_eq!(error, b"\xffstderr\ngoblins: session cancelled\n");
    assert!(!root.0.join("state").exists());
    assert!(!root.0.join("cache").exists());
}

#[test]
#[ignore = "needs a controlling PTY and TOSTOP; run just test-rust-host"]
fn tostop_allows_the_supervisor_and_its_separate_group_stderr_writer() {
    let root = Root(unix::temp_directory().unwrap());
    let (master, slave) = unix::pty(24, 80).unwrap();
    let mut attributes: libc::termios = unsafe { std::mem::zeroed() };
    assert_eq!(
        unsafe { libc::tcgetattr(slave.as_raw_fd(), &mut attributes) },
        0
    );
    attributes.c_oflag &= !libc::OPOST;
    attributes.c_lflag |= libc::TOSTOP;
    assert_eq!(
        unsafe { libc::tcsetattr(slave.as_raw_fd(), libc::TCSANOW, &attributes) },
        0
    );
    let mut run = Run::start(&root.0, "maintenance-cli-tty", Some(&slave));
    let peer = run.ready(&root.0);
    assert_eq!(
        unsafe { libc::tcgetpgrp(master.as_raw_fd()) },
        run.child.id() as i32
    );
    assert_eq!(peer["sid"], run.child.id());
    assert_ne!(peer["pgid"], run.child.id());
    let mut expected = b"\xfehelper\n\xffstderr\n".to_vec();
    expected.extend((0..4 * 1024 * 1024).map(|n| n as u8));
    let mut output = vec![];
    let mut reader = fs::File::from(master);
    let deadline = Instant::now() + Duration::from_secs(15);
    while output.len() < expected.len() {
        assert!(
            Instant::now() < deadline,
            "TOSTOP stopped a maintenance writer"
        );
        if unix::readable(reader.as_raw_fd(), 100).unwrap() {
            let mut bytes = [0; 65536];
            let left = (expected.len() - output.len()).min(bytes.len());
            let n = reader.read(&mut bytes[..left]).unwrap();
            assert!(n > 0, "maintenance output ended early");
            output.extend_from_slice(&bytes[..n]);
        }
    }
    assert_eq!(output, expected);
    assert_eq!(
        unsafe { libc::tcgetattr(slave.as_raw_fd(), &mut attributes) },
        0
    );
    assert_ne!(attributes.c_lflag & libc::TOSTOP, 0);
    assert!(run.exit().success());
    assert!(
        unix::readable(run.peer.as_ref().unwrap().as_raw_fd(), 0).unwrap(),
        "supervisor not reaped/exited"
    );
}

fn limits(pid: libc::pid_t) -> (libc::rlim_t, libc::rlim_t) {
    let mut value: libc::rlimit = unsafe { std::mem::zeroed() };
    assert_eq!(
        unsafe { libc::prlimit(pid, libc::RLIMIT_NOFILE, std::ptr::null(), &mut value) },
        0
    );
    (value.rlim_cur, value.rlim_max)
}
fn wait_for(mut condition: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(20);
    while !condition() {
        assert!(
            Instant::now() < deadline,
            "foreground supervisor condition timed out"
        );
        std::thread::sleep(Duration::from_millis(2));
    }
}

#[test]
#[ignore = "needs user namespaces and the host store; run just test-rust-host"]
fn the_foreground_supervisor_keeps_payload_limits_and_raises_its_own_capacity() {
    use crate::host::Client;
    use serde_json::json;
    use std::os::unix::net::UnixStream;
    let original = limits(0);
    let fixture = std::env::var_os("GOBLINS_TEST_EXEC_MANIFEST").expect("run just test-rust-host");
    let config: Value = serde_json::from_slice(&fs::read(fixture).unwrap()).unwrap();
    for soft in [128, 512] {
        let root = Root(unix::temp_directory().unwrap());
        let state = root.0.join("state");
        let manifest = root.0.join("manifest.json");
        let mut config = config.clone();
        config["goblins"]["shell"]["args"] = json!([
            "--noprofile",
            "--norc",
            "-c",
            "printf 'NOFILE:%s:%s\n' \"$(ulimit -Sn)\" \"$(ulimit -Hn)\"; exec sleep 600"
        ]);
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
            .stdin(Stdio::null())
            .stdout(Stdio::from(output.try_clone().unwrap()))
            .stderr(Stdio::from(output));
        unsafe {
            command.pre_exec(move || {
                unix::cvt(libc::setsid())?;
                unix::cvt(libc::setrlimit(
                    libc::RLIMIT_NOFILE,
                    &libc::rlimit {
                        rlim_cur: soft,
                        rlim_max: 512,
                    },
                ))?;
                Ok(())
            });
        }
        let child = {
            let _fork = unix::forking();
            command.spawn().unwrap()
        };
        let mut run = Run { child, peer: None };
        wait_for(|| {
            assert!(
                run.child.try_wait().unwrap().is_none(),
                "{}",
                fs::read_to_string(&log).unwrap()
            );
            fs::read_to_string(&log).unwrap().contains("daemon ready")
        });
        let mut client = Client::connect(&state).unwrap();
        let daemon = run.child.id() as libc::pid_t;
        assert_eq!(limits(daemon), (512, 512));
        let launch=client.call("sessions.start",json!({"key":"nofile","name":"shell","agent_name":format!("nofile-{soft}"),"configuration":manifest,"rows":24,"cols":100})).unwrap();
        let session = launch["session"].as_str().unwrap();
        let mut record = Value::Null;
        wait_for(|| {
            record = client
                .call("sessions.get", json!({"session":session}))
                .unwrap();
            assert_ne!(record["state"], "failed", "{record}");
            record["state"] == "running"
        });
        let children =
            fs::read_to_string(format!("/proc/{daemon}/task/{daemon}/children")).unwrap();
        let children: Vec<libc::pid_t> = children
            .split_whitespace()
            .map(|s| s.parse().unwrap())
            .collect();
        assert_eq!(children.len(), 1, "{children:?}");
        let supervisor = children[0];
        assert_eq!(limits(supervisor), (512, 512));
        let payload = record["identity"]["pid"].as_u64().unwrap();
        let mut terminal = UnixStream::connect(launch["terminal"].as_str().unwrap()).unwrap();
        terminal
            .set_read_timeout(Some(Duration::from_secs(10)))
            .unwrap();
        assert_eq!(
            goblins_jsonrpc::read(&mut terminal).unwrap()["result"]["attached"],
            true
        );
        let mut output = vec![];
        while !output.contains(&b'\n') {
            let mut bytes = [0; 4096];
            let n = terminal.read(&mut bytes).unwrap();
            assert!(n > 0, "payload exited before reporting its limits");
            output.extend_from_slice(&bytes[..n]);
        }
        assert_eq!(
            String::from_utf8_lossy(&output).trim(),
            format!("NOFILE:{soft}:512")
        );
        assert_eq!(limits(daemon), (512, 512));
        assert_eq!(limits(supervisor), (512, 512));
        client
            .call("sessions.stop", json!({"session":session}))
            .unwrap();
        wait_for(|| !state.join(session).join("resources").exists());
        let mut trailing = vec![];
        terminal.read_to_end(&mut trailing).unwrap();
        drop(terminal);
        wait_for(|| !Path::new(&format!("/proc/{supervisor}")).exists());
        wait_for(|| !Path::new(&format!("/proc/{payload}")).exists());
        assert_eq!(limits(0), original);
        client.call("server.stop", json!({})).unwrap();
        assert!(run.exit().success());
    }
}
