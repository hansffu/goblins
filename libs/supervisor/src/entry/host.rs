//! Host tests: kernel backstops of the supervisor executable's startup, run
//! against the built `goblins-supervisor-bwrap` that `just test-rust-host`
//! passes in `GOBLINS_TEST_SUPERVISOR_EXECUTABLE`. A supervisor closes
//! descriptors it was not given, dies with its parent even when stopped, and
//! refuses a control socket created by a process other than its parent.
use goblins_unix as unix;
use std::{
    io::{BufRead, BufReader, Read},
    os::{
        fd::{AsRawFd, OwnedFd},
        unix::{net::UnixStream, process::CommandExt},
    },
    path::PathBuf,
    process::{Child, Command, ExitStatus, Stdio},
    time::{Duration, Instant},
};

const INNER: &str = "GOBLINS_TEST_HOST_INNER";

fn supervisor() -> PathBuf {
    std::env::var_os("GOBLINS_TEST_SUPERVISOR_EXECUTABLE")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            panic!("GOBLINS_TEST_SUPERVISOR_EXECUTABLE is unset; run host tests with `just test-rust-host`")
        })
}

/// `program` with `control` as its control socket (fd 3) and `extra` at fd 100.
fn command(
    program: impl AsRef<std::ffi::OsStr>,
    control: &UnixStream,
    extra: Option<&OwnedFd>,
) -> Command {
    let control = control.as_raw_fd();
    let extra = extra.map(AsRawFd::as_raw_fd);
    let mut command = Command::new(program);
    command.stdin(Stdio::null()).stdout(Stdio::null());
    unsafe {
        command.pre_exec(move || {
            // dup2 leaves the copies inheritable.
            unix::cvt(libc::dup2(control, 3))?;
            if let Some(extra) = extra {
                unix::cvt(libc::dup2(extra, 100))?;
            }
            Ok(())
        });
    }
    command
}

/// A child's exit status, within 20 seconds.
fn exit(child: &mut Child) -> ExitStatus {
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        if let Some(status) = child.try_wait().unwrap() {
            return status;
        }
        assert!(Instant::now() < deadline, "the process did not exit");
        std::thread::sleep(Duration::from_millis(10));
    }
}

/// A child process killed and reaped when the test ends, also on failure.
struct Killed(Child);
impl Drop for Killed {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

#[test]
#[ignore = "needs the built supervisor; run with `just test-rust-host`"]
fn startup_closes_descriptors_it_was_not_given() {
    let (control, child) = UnixStream::pair().unwrap();
    let (read, write) = unix::pipe().unwrap();
    let mut scope = Killed({
        let _forking = unix::forking();
        command(supervisor(), &child, Some(&write)).spawn().unwrap()
    });
    drop((child, write));
    // The pipe's last writer was the supervisor's fd 100.
    assert!(
        unix::readable(read.as_raw_fd(), 10_000).unwrap(),
        "the supervisor kept an unrelated pipe"
    );
    assert_eq!(std::fs::File::from(read).read(&mut [0]).unwrap(), 0);
    assert!(
        scope.0.try_wait().unwrap().is_none(),
        "the supervisor should await its first message"
    );
    drop(control);
    assert!(exit(&mut scope.0).success());
}

#[test]
#[ignore = "needs the built supervisor; run with `just test-rust-host`"]
fn the_parent_dying_kills_even_a_stopped_supervisor() {
    if std::env::var_os(INNER).is_some() {
        // The parent: it creates the control socket, starts the supervisor,
        // reports its pid once startup is done, and waits to be killed.
        let (control, child) = UnixStream::pair().unwrap();
        let (ready, ready_sink) = unix::pipe().unwrap();
        let scope = command(supervisor(), &child, Some(&ready_sink))
            .spawn()
            .unwrap();
        drop((child, ready_sink));
        // EOF: the supervisor has set its parent-death signal and closed fd 100.
        assert_eq!(std::fs::File::from(ready).read(&mut [0]).unwrap(), 0);
        println!("SUPERVISOR {}", scope.id());
        let _keep = control;
        loop {
            std::thread::sleep(Duration::from_secs(60));
        }
    }
    let mut parent = Killed({
        let _forking = unix::forking();
        Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "entry::host::the_parent_dying_kills_even_a_stopped_supervisor",
                "--ignored",
                "--nocapture",
                "--test-threads=1",
            ])
            .env(INNER, "1")
            .stdout(Stdio::piped())
            .spawn()
            .unwrap()
    });
    // If the parent never reports, kill it: its stdout closes and the read
    // below fails instead of waiting forever.
    let reported = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let watchdog = {
        // A pidfd, not the pid: if the test fails first and the parent is
        // reaped, the signal reaches nobody instead of a reused pid.
        let (reported, pidfd) = (
            reported.clone(),
            unix::owned(unsafe { libc::syscall(libc::SYS_pidfd_open, parent.0.id(), 0) as i32 })
                .unwrap(),
        );
        std::thread::spawn(move || {
            let deadline = Instant::now() + Duration::from_secs(30);
            while Instant::now() < deadline {
                if reported.load(std::sync::atomic::Ordering::Relaxed) {
                    return;
                }
                std::thread::sleep(Duration::from_millis(10));
            }
            unsafe {
                libc::syscall(
                    libc::SYS_pidfd_send_signal,
                    pidfd.as_raw_fd(),
                    libc::SIGKILL,
                    std::ptr::null::<libc::siginfo_t>(),
                    0,
                )
            };
        })
    };
    let mut lines = BufReader::new(parent.0.stdout.take().unwrap()).lines();
    let pid: u32 = loop {
        let line = lines.next().expect("the parent exited").unwrap();
        // The test harness may print its own text before ours on this line.
        if let Some((_, pid)) = line.split_once("SUPERVISOR ") {
            break pid.parse().unwrap();
        }
    };
    reported.store(true, std::sync::atomic::Ordering::Relaxed);
    watchdog.join().unwrap();
    let exited =
        unix::owned(unsafe { libc::syscall(libc::SYS_pidfd_open, pid, 0) as i32 }).unwrap();
    let signal = |signal: libc::c_int| unsafe {
        libc::syscall(
            libc::SYS_pidfd_send_signal,
            exited.as_raw_fd(),
            signal,
            std::ptr::null::<libc::siginfo_t>(),
            0,
        )
    };
    signal(libc::SIGSTOP);
    let deadline = Instant::now() + Duration::from_secs(10);
    while !std::fs::read_to_string(format!("/proc/{pid}/status"))
        .unwrap()
        .lines()
        .any(|line| line.starts_with("State:") && line.contains('T'))
    {
        assert!(Instant::now() < deadline, "the supervisor was not stopped");
        std::thread::sleep(Duration::from_millis(10));
    }
    parent.0.kill().unwrap();
    parent.0.wait().unwrap();
    // Stopped, it cannot see its control socket close: only the kernel's
    // parent-death signal ends it.
    let died = unix::readable(exited.as_raw_fd(), 10_000).unwrap();
    signal(libc::SIGKILL);
    assert!(died, "a stopped supervisor survived its parent");
}

#[test]
#[ignore = "needs the built supervisor and the runtime fixture's shell; run with `just test-rust-host`"]
fn a_control_socket_from_another_process_than_the_parent_is_refused() {
    let runtime: serde_json::Value = serde_json::from_slice(
        &std::fs::read(
            std::env::var_os("GOBLINS_TEST_RUNTIME").expect("GOBLINS_TEST_RUNTIME is unset"),
        )
        .unwrap(),
    )
    .unwrap();
    // This test creates the socket; a shell in between becomes the parent.
    let (_control, child) = UnixStream::pair().unwrap();
    let mut shell = command(runtime["shell"].as_str().unwrap(), &child, None);
    shell.args(["-c", "\"$0\"; exit $?"]).arg(supervisor());
    let mut shell = Killed({
        let _forking = unix::forking();
        shell.spawn().unwrap()
    });
    drop(child);
    assert!(
        !exit(&mut shell.0).success(),
        "the supervisor accepted a socket its parent did not create"
    );
}
