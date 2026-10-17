//! Supervisor for one approved host command (`goblin host run`). The daemon
//! starts it in the job's working directory and environment, with a private
//! lifetime pipe as stdin: when the daemon drops that pipe (cancellation, its
//! session ending, or the daemon dying), the job is stopped. As a subreaper it
//! collects every process the command leaves behind, and stops them whenever
//! the job ends, also after a normal exit, before it reports the outcome.
//!
//! goblins-mount-helper --host-job LOG STATUS LIMIT TIMEOUT -- ARGV...
use super::{cvt, pipe};
use std::{
    fs::{self, File, OpenOptions},
    io::{self, Write},
    os::{
        fd::{AsRawFd, RawFd},
        unix::{fs::OpenOptionsExt, process::CommandExt},
    },
    path::Path,
    process::{Command, Stdio},
    sync::atomic::{AtomicBool, Ordering},
    time::{Duration, Instant},
};

/// SIGTERM, SIGINT or SIGHUP: the daemon died (PR_SET_PDEATHSIG) or the
/// supervisor was told to stop. Either way, stop the job, then clean up.
static STOP: AtomicBool = AtomicBool::new(false);
extern "C" fn stop(_: i32) {
    STOP.store(true, Ordering::Relaxed);
}

/// How long the command's process group has to exit after SIGTERM.
const GRACE: Duration = Duration::from_secs(10);
/// After the command exits, output from what it left behind is still copied
/// until the pipe is idle this long.
const DRAIN_IDLE: Duration = Duration::from_millis(200);
/// Upper bound on stopping the processes left behind.
const CLEANUP: Duration = Duration::from_secs(10);
const TRUNCATED: &[u8] = b"\n[goblins: output limit reached; the rest was discarded]\n";

/// The job's outcome, written to STATUS once nothing it started is left.
#[derive(Default)]
struct Outcome {
    exit_code: Option<i32>,
    signal: Option<i32>,
    /// Why it did not run: "not found", "not executable" or another error.
    spawn_error: Option<String>,
    cancelled: bool,
    timed_out: bool,
    truncated: bool,
}
impl Outcome {
    fn json(&self) -> String {
        let text = |s: &Option<String>| {
            s.as_ref().map_or("null".into(), |s| {
                format!(
                    "\"{}\"",
                    s.chars()
                        .filter(|c| !c.is_control())
                        .collect::<String>()
                        .replace('\\', "\\\\")
                        .replace('"', "\\\"")
                )
            })
        };
        let number = |n: Option<i32>| n.map_or("null".into(), |n| n.to_string());
        format!(
            "{{\"exit_code\":{},\"signal\":{},\"spawn_error\":{},\"cancelled\":{},\"timed_out\":{},\"truncated\":{}}}\n",
            number(self.exit_code),
            number(self.signal),
            text(&self.spawn_error),
            self.cancelled,
            self.timed_out,
            self.truncated
        )
    }
}

/// What one read of the output pipe found.
#[derive(PartialEq)]
enum Read {
    Data,
    /// No writer is left.
    End,
    /// Nothing to read yet.
    Empty,
}

/// Copies the command's output to LOG, up to the limit.
struct Log {
    file: File,
    written: u64,
    limit: u64,
    truncated: bool,
}
impl Log {
    fn copy(&mut self, from: RawFd) -> io::Result<Read> {
        let mut bytes = [0u8; 65536];
        let n = unsafe { libc::read(from, bytes.as_mut_ptr().cast(), bytes.len()) };
        if n < 0 {
            let error = io::Error::last_os_error();
            return match error.kind() {
                io::ErrorKind::WouldBlock | io::ErrorKind::Interrupted => Ok(Read::Empty),
                _ => Err(error),
            };
        }
        let n = n as usize;
        if n == 0 {
            return Ok(Read::End);
        }
        let room = self.limit.saturating_sub(self.written) as usize;
        if room > 0 {
            self.file.write_all(&bytes[..n.min(room)])?;
            self.written += n.min(room) as u64;
        }
        if n > room && !self.truncated {
            self.truncated = true;
            self.file.write_all(TRUNCATED)?;
        }
        Ok(Read::Data)
    }
}

/// Wait up to `timeout` for `fds`; the returned flags say which are readable
/// or hung up.
fn wait(fds: &[RawFd], timeout: Duration) -> io::Result<Vec<bool>> {
    let mut polls: Vec<_> = fds
        .iter()
        .map(|&fd| libc::pollfd {
            fd,
            events: libc::POLLIN,
            revents: 0,
        })
        .collect();
    let n = unsafe {
        libc::poll(
            polls.as_mut_ptr(),
            polls.len() as libc::nfds_t,
            timeout.as_millis() as i32,
        )
    };
    if n < 0 {
        let error = io::Error::last_os_error();
        if error.kind() == io::ErrorKind::Interrupted {
            return Ok(vec![false; fds.len()]);
        }
        return Err(error);
    }
    Ok(polls.iter().map(|p| p.revents != 0).collect())
}

/// The command's status once it exited, without reaping it: the PID stays
/// reserved while its process group is signalled.
fn exited(pid: i32) -> io::Result<Option<(Option<i32>, Option<i32>)>> {
    let mut info: libc::siginfo_t = unsafe { std::mem::zeroed() };
    cvt(unsafe {
        libc::waitid(
            libc::P_PID,
            pid as libc::id_t,
            &mut info,
            libc::WEXITED | libc::WNOHANG | libc::WNOWAIT,
        )
    })?;
    if unsafe { info.si_pid() } == 0 {
        return Ok(None);
    }
    let status = unsafe { info.si_status() };
    Ok(Some(if info.si_code == libc::CLD_EXITED {
        (Some(status), None)
    } else {
        (None, Some(status))
    }))
}

/// Children of this process: the command and whatever reparented to us.
fn children() -> Vec<i32> {
    let me = std::process::id();
    fs::read_to_string(format!("/proc/self/task/{me}/children"))
        .unwrap_or_default()
        .split_whitespace()
        .filter_map(|pid| pid.parse().ok())
        .collect()
}

/// Reap orphans that already exited, as init would. A PID namespace whose
/// init exits waits until all its processes are reaped, and processes that
/// joined it from outside are reparented here, not to its init.
fn reap_orphans(command: i32) {
    for pid in children() {
        if pid != command {
            unsafe { libc::waitpid(pid, std::ptr::null_mut(), libc::WNOHANG) };
        }
    }
}

/// Stop everything the command started. Killing a child reparents its own
/// children to this subreaper, so repeat until none is left.
fn stop_all(group: i32, deadline: Instant) {
    loop {
        unsafe { libc::kill(-group, libc::SIGKILL) };
        for pid in children() {
            unsafe { libc::kill(pid, libc::SIGKILL) };
        }
        loop {
            let pid = unsafe { libc::waitpid(-1, std::ptr::null_mut(), libc::WNOHANG) };
            if pid < 0 {
                if io::Error::last_os_error().raw_os_error() == Some(libc::ECHILD) {
                    return;
                }
                break;
            }
            if pid == 0 {
                break;
            }
        }
        if Instant::now() >= deadline {
            return;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
}

pub fn run() -> io::Result<i32> {
    let args: Vec<String> = std::env::args().collect();
    if args.len() < 8 || args[6] != "--" {
        return Err(io::Error::other(
            "usage: --host-job LOG STATUS LIMIT TIMEOUT -- ARGV...",
        ));
    }
    let (log, status) = (Path::new(&args[2]), Path::new(&args[3]));
    let limit: u64 = args[4].parse().map_err(io::Error::other)?;
    let timeout: u64 = args[5].parse().map_err(io::Error::other)?;
    let argv = &args[7..];
    cvt(unsafe { libc::prctl(libc::PR_SET_CHILD_SUBREAPER, 1) })?;
    for signal in [libc::SIGTERM, libc::SIGINT, libc::SIGHUP] {
        unsafe { libc::signal(signal, stop as *const () as libc::sighandler_t) };
    }
    let mut outcome = Outcome::default();
    let mut output = Log {
        file: OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(log)?,
        written: 0,
        limit,
        truncated: false,
    };
    let (read, write) = pipe()?;
    let flags = cvt(unsafe { libc::fcntl(read.as_raw_fd(), libc::F_GETFL) })?;
    cvt(unsafe { libc::fcntl(read.as_raw_fd(), libc::F_SETFL, flags | libc::O_NONBLOCK) })?;
    // Null stdin replaces the daemon's lifetime pipe in the command.
    let mut command = Command::new(&argv[0]);
    command
        .args(&argv[1..])
        .stdin(Stdio::null())
        .stdout(Stdio::from(write.try_clone()?))
        .stderr(Stdio::from(write))
        .process_group(0);
    let child = match command.spawn() {
        Ok(child) => child,
        Err(e) => {
            outcome.spawn_error = Some(match e.kind() {
                io::ErrorKind::NotFound => "not found".into(),
                io::ErrorKind::PermissionDenied => "not executable".into(),
                _ => e.to_string(),
            });
            write_status(status, &outcome)?;
            return Ok(0);
        }
    };
    drop(command);
    let pid = child.id() as i32;
    let started = Instant::now();
    let output_fd = read.as_raw_fd();
    let mut terminated: Option<Instant> = None;
    let mut lifetime_open = true;
    let ended = loop {
        let fds: &[RawFd] = if lifetime_open {
            &[output_fd, 0]
        } else {
            &[output_fd]
        };
        let ready = wait(fds, Duration::from_millis(100))?;
        if ready[0] {
            output.copy(output_fd)?;
        }
        if lifetime_open && ready[1] || STOP.load(Ordering::Relaxed) {
            // Data or hang-up: the daemon no longer wants this job.
            lifetime_open = false;
            outcome.cancelled = true;
        }
        reap_orphans(pid);
        if let Some(status) = exited(pid)? {
            break status;
        }
        if terminated.is_none()
            && !outcome.cancelled
            && timeout > 0
            && started.elapsed() >= Duration::from_secs(timeout)
        {
            outcome.timed_out = true;
        }
        if terminated.is_none() && (outcome.cancelled || outcome.timed_out) {
            unsafe { libc::kill(-pid, libc::SIGTERM) };
            terminated = Some(Instant::now());
        }
        if terminated.is_some_and(|at| at.elapsed() >= GRACE) {
            unsafe { libc::kill(-pid, libc::SIGKILL) };
        }
    };
    (outcome.exit_code, outcome.signal) = ended;
    // Output from what the command left behind, until it goes quiet.
    let mut idle = Instant::now();
    while idle.elapsed() < DRAIN_IDLE {
        reap_orphans(pid);
        if wait(&[output_fd], DRAIN_IDLE)?[0] {
            if output.copy(output_fd)? == Read::End {
                break;
            }
            idle = Instant::now();
        }
    }
    stop_all(pid, Instant::now() + CLEANUP);
    // Every writer is gone now: copy what is left.
    while output.copy(output_fd)? == Read::Data {}
    drop((child, read));
    outcome.truncated = output.truncated;
    write_status(status, &outcome)?;
    Ok(0)
}

fn write_status(status: &Path, outcome: &Outcome) -> io::Result<()> {
    let staged = status.with_extension("json.tmp");
    fs::write(&staged, outcome.json())?;
    fs::rename(staged, status)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn outcomes_are_json() {
        let outcome = Outcome {
            exit_code: Some(3),
            spawn_error: Some("a \"b\"\n".into()),
            ..Default::default()
        };
        assert_eq!(
            outcome.json(),
            "{\"exit_code\":3,\"signal\":null,\"spawn_error\":\"a \\\"b\\\"\",\"cancelled\":false,\"timed_out\":false,\"truncated\":false}\n"
        );
    }
}
