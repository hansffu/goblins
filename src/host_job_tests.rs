//! The `--host-job` supervisor, run as the daemon runs it: a lifetime pipe
//! on stdin, the job's directory as working directory.
use std::{
    fs,
    os::unix::process::CommandExt,
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    time::{Duration, Instant},
};

fn directory(name: &str) -> PathBuf {
    let path = std::env::temp_dir().join(format!("goblins-host-job-{name}-{}", std::process::id()));
    let _ = fs::remove_dir_all(&path);
    fs::create_dir_all(&path).unwrap();
    path
}

/// Start the supervisor for `sh -c SCRIPT` in `dir`; its stdin is the
/// lifetime pipe, held open until the child's stdin is dropped.
fn start(dir: &Path, limit: u64, timeout: u64, script: &str, file_size: Option<u64>) -> Child {
    let mut command = Command::new(env!("CARGO_BIN_EXE_goblins-mount-helper"));
    command
        .arg("--host-job")
        .arg(dir.join("output.log"))
        .arg(dir.join("status.json"))
        .arg(limit.to_string())
        .arg(timeout.to_string())
        .args(["--", "sh", "-c", script])
        .current_dir(dir)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::piped());
    if let Some(bytes) = file_size {
        unsafe {
            command.pre_exec(move || {
                let limit = libc::rlimit {
                    rlim_cur: bytes,
                    rlim_max: bytes,
                };
                if libc::setrlimit(libc::RLIMIT_FSIZE, &limit) != 0 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
    }
    command.spawn().unwrap()
}

fn finished(child: &mut Child, within: Duration) -> std::process::ExitStatus {
    let deadline = Instant::now() + within;
    loop {
        if let Some(status) = child.try_wait().unwrap() {
            return status;
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            panic!("the supervisor did not finish within {within:?}");
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

fn status(dir: &Path) -> String {
    fs::read_to_string(dir.join("status.json")).unwrap()
}

fn alive(dir: &Path, file: &str) -> bool {
    let pid: i32 = fs::read_to_string(dir.join(file))
        .unwrap()
        .trim()
        .parse()
        .unwrap();
    unsafe { libc::kill(pid, 0) == 0 }
}

/// A process that keeps writing after the command exited ends the job
/// anyway, and is stopped with it.
#[test]
fn a_chatty_background_process_does_not_keep_the_job_alive() {
    let dir = directory("chatty");
    let script = "(while :; do echo tick; sleep 0.05; done) & echo $! > orphan.pid; exit 0";
    let mut job = start(&dir, 1 << 20, 0, script, None);
    assert!(finished(&mut job, Duration::from_secs(8)).success());
    assert!(status(&dir).contains("\"exit_code\":0"), "{}", status(&dir));
    assert!(!alive(&dir, "orphan.pid"));
    // Cancelled while its output drains: also bounded.
    let dir = directory("chatty-cancel");
    let mut job = start(&dir, 1 << 20, 0, script, None);
    std::thread::sleep(Duration::from_millis(300));
    drop(job.stdin.take());
    assert!(finished(&mut job, Duration::from_secs(8)).success());
    assert!(!alive(&dir, "orphan.pid"));
}

/// A command that closed its output does not make the supervisor spin.
#[test]
fn closed_output_is_not_polled_again() {
    let dir = directory("closed");
    let mut job = start(&dir, 1 << 20, 0, "exec >/dev/null 2>&1; sleep 2", None);
    std::thread::sleep(Duration::from_millis(1500));
    let stat = fs::read_to_string(format!("/proc/{}/stat", job.id())).unwrap();
    let fields: Vec<u64> = stat
        .rsplit_once(')')
        .unwrap()
        .1
        .split_whitespace()
        .skip(11)
        .take(2)
        .map(|f| f.parse().unwrap())
        .collect();
    // utime + stime in clock ticks (usually 100 per second).
    assert!(
        fields[0] + fields[1] < 20,
        "the supervisor used {fields:?} ticks"
    );
    assert!(finished(&mut job, Duration::from_secs(8)).success());
}

/// A log that cannot be written fails the supervisor, which still stops
/// everything the command started first.
#[test]
fn a_failing_log_still_stops_the_job() {
    let dir = directory("log-failure");
    let script = "echo $$ > command.pid; head -c 4096 /dev/zero; exec sleep 300";
    let mut job = start(&dir, 65536, 0, script, Some(1024));
    let status = finished(&mut job, Duration::from_secs(15));
    assert!(!status.success());
    assert!(!dir.join("status.json").exists());
    assert!(!alive(&dir, "command.pid"));
}

/// The request's variables reach the command, not the supervisor, so an
/// override such as LD_PRELOAD cannot take over supervision.
#[test]
fn request_variables_reach_only_the_command() {
    let dir = directory("env");
    let mut command = Command::new(env!("CARGO_BIN_EXE_goblins-mount-helper"));
    let mut job = command
        .arg("--host-job")
        .arg(dir.join("output.log"))
        .arg(dir.join("status.json"))
        .args([
            "65536",
            "0",
            "--env",
            "GOBLINS_TEST_MARK",
            "for the command",
        ])
        .args([
            "--",
            "sh",
            "-c",
            "echo \"MARK=$GOBLINS_TEST_MARK\"; sleep 1",
        ])
        .current_dir(&dir)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .spawn()
        .unwrap();
    std::thread::sleep(Duration::from_millis(300));
    let supervisor = fs::read(format!("/proc/{}/environ", job.id())).unwrap();
    assert!(!String::from_utf8_lossy(&supervisor).contains("GOBLINS_TEST_MARK"));
    assert!(finished(&mut job, Duration::from_secs(8)).success());
    let output = fs::read_to_string(dir.join("output.log")).unwrap();
    assert!(output.contains("MARK=for the command"), "{output}");
}
