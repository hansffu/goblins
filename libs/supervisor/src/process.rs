//! Sequential, cancellable host commands. No shell evaluates request text.
use crate::Result;
use goblins_unix as unix;
use std::{
    fs::{self, File},
    io::{Read, Seek, SeekFrom},
    os::{
        fd::{AsRawFd, OwnedFd, RawFd},
        unix::process::CommandExt,
    },
    path::Path,
    process::{Child, Command, Stdio},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
};
pub const WORKER_TICK: Duration = Duration::from_millis(20);
#[derive(Clone, Default)]
pub struct Cancellation {
    cancelled: Arc<AtomicBool>,
    parent: Option<Arc<AtomicBool>>,
    /// When the work times out, and its time limit for the message.
    deadline: Option<(Instant, Duration)>,
    /// While a scope handles one startup request, a following request (Leave)
    /// or EOF cancels it. The socket stays unread for the scope event loop.
    readable: Option<Arc<OwnedFd>>,
}
impl Cancellation {
    pub fn cancel(&self) {
        self.cancelled.store(true, Ordering::Relaxed);
    }
    /// A request preview can be cancelled without ending the owning session.
    pub fn child(&self) -> Self {
        Self {
            cancelled: Arc::default(),
            parent: Some(self.cancelled.clone()),
            deadline: self.deadline,
            readable: self.readable.clone(),
        }
    }
    /// The same cancellation, which also fails once `limit` has passed.
    pub fn with_limit(&self, limit: Duration) -> Self {
        Self {
            deadline: Some((Instant::now() + limit, limit)),
            ..self.clone()
        }
    }
    pub fn until_readable(fd: RawFd) -> Result<Self> {
        let fd = unix::owned(unsafe { libc::fcntl(fd, libc::F_DUPFD_CLOEXEC, 3) })?;
        Ok(Self {
            readable: Some(Arc::new(fd)),
            ..Self::default()
        })
    }
    pub fn check(&self) -> Result<()> {
        if self.cancelled.load(Ordering::Relaxed)
            || self
                .parent
                .as_ref()
                .is_some_and(|p| p.load(Ordering::Relaxed))
            || self.readable.as_ref().is_some_and(|fd| {
                let mut poll = libc::pollfd {
                    fd: fd.as_raw_fd(),
                    events: libc::POLLIN | libc::POLLRDHUP,
                    revents: 0,
                };
                let n = unsafe { libc::poll(&mut poll, 1, 0) };
                (n > 0 && poll.revents != 0)
                    || (n < 0
                        && std::io::Error::last_os_error().kind()
                            != std::io::ErrorKind::Interrupted)
            })
        {
            Err("session cancelled".into())
        } else if let Some((deadline, limit)) = self.deadline
            && Instant::now() >= deadline
        {
            Err(format!("evaluation timed out after {}", duration(limit)).into())
        } else {
            Ok(())
        }
    }
}
fn duration(limit: Duration) -> String {
    match limit.as_secs() {
        s if s >= 60 && s % 60 == 0 => format!("{} min", s / 60),
        s => format!("{s} s"),
    }
}
struct CommandChild {
    child: Child,
    reaped: bool,
}
impl CommandChild {
    fn kill_group(&self) {
        unsafe {
            libc::kill(-(self.child.id() as i32), libc::SIGKILL);
        }
    }
    fn finished(&mut self) -> Result<Option<std::process::ExitStatus>> {
        // Observe without reaping first. Keeping the child (even as a zombie)
        // reserves its PID while we terminate its process group, preventing a
        // signal to an unrelated group if the kernel recycled a reaped PID.
        let mut info: libc::siginfo_t = unsafe { std::mem::zeroed() };
        unix::cvt(unsafe {
            libc::waitid(
                libc::P_PID,
                self.child.id(),
                &mut info,
                libc::WEXITED | libc::WNOHANG | libc::WNOWAIT,
            )
        })?;
        if unsafe { info.si_pid() } == 0 {
            return Ok(None);
        }
        self.kill_group();
        let status = self.child.wait()?;
        self.reaped = true;
        Ok(Some(status))
    }
}
impl Drop for CommandChild {
    fn drop(&mut self) {
        // Also terminate descendants of the owned Nix client (the host Nix
        // daemon is not our child). Reap the direct child on every error path.
        if !self.reaped {
            self.kill_group();
            let _ = self.child.wait();
        }
    }
}
pub fn diagnostic(path: &Path) -> String {
    (|| -> std::io::Result<String> {
        let mut f = File::open(path)?;
        let length = f.metadata()?.len();
        f.seek(SeekFrom::Start(length.saturating_sub(12000)))?;
        let mut data = Vec::new();
        f.read_to_end(&mut data)?;
        Ok(String::from_utf8_lossy(&data).into_owned())
    })()
    .unwrap_or_default()
}
pub fn command(command: &mut Command, directory: &Path, cancel: &Cancellation) -> Result<String> {
    command_with_fds(command, directory, cancel, &[])
}
/// Descriptor authority is supplied only by trusted host implementations.
pub(crate) fn command_with_fds(
    command: &mut Command,
    directory: &Path,
    cancel: &Cancellation,
    fds: &[std::os::fd::RawFd],
) -> Result<String> {
    cancel.check()?;
    command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .process_group(0);
    let parent = unsafe { libc::getpid() };
    let keep = fds.to_vec();
    unsafe {
        command.pre_exec(move || {
            unix::cvt(libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL))?;
            if libc::getppid() != parent {
                return Err(std::io::Error::from_raw_os_error(libc::ESRCH));
            }
            unix::cvt(libc::syscall(libc::SYS_close_range, 3u32, u32::MAX, 4u32) as i32)?;
            for fd in &keep {
                unix::cvt(libc::fcntl(*fd, libc::F_SETFD, 0))?;
            }
            Ok(())
        });
    }
    let mut child = CommandChild {
        child: command.spawn()?,
        reaped: false,
    };
    // Wake as soon as the child exits, including when it closed its output
    // earlier. Keep waitid(WNOWAIT) for process-group identity and cleanup.
    // If pidfds are restricted, output readiness still avoids most polling.
    let pidfd =
        unix::owned(unsafe { libc::syscall(libc::SYS_pidfd_open, child.child.id(), 0) as i32 })
            .ok();
    let mut stdout = child.child.stdout.take().unwrap();
    let mut stderr = child.child.stderr.take().unwrap();
    unix::nonblocking(stdout.as_raw_fd())?;
    unix::nonblocking(stderr.as_raw_fd())?;
    let mut out = Vec::new();
    let mut err = Vec::new();
    let mut output_open = [true, true];
    loop {
        cancel.check()?;
        let status = child.finished()?;
        // Drain after observing exit too. Limits apply to captured bytes, not
        // Nix's own store/cache files. No process-wide file-size limit is used.
        for (index, (reader, buffer, limit, tail)) in [
            (
                &mut stdout as &mut dyn Read,
                &mut out,
                8 * 1024 * 1024,
                false,
            ),
            (&mut stderr as &mut dyn Read, &mut err, 1024 * 1024, true),
        ]
        .into_iter()
        .enumerate()
        {
            for _ in 0..128 {
                let mut bytes = [0; 8192];
                match reader.read(&mut bytes) {
                    Ok(0) => {
                        output_open[index] = false;
                        break;
                    }
                    Ok(n) => {
                        buffer.extend_from_slice(&bytes[..n]);
                        if buffer.len() > limit {
                            if tail {
                                buffer.drain(..buffer.len() - limit);
                            } else {
                                return Err("host command output exceeds 8 MiB".into());
                            }
                        }
                    }
                    Err(e)
                        if matches!(
                            e.kind(),
                            std::io::ErrorKind::WouldBlock | std::io::ErrorKind::Interrupted
                        ) =>
                    {
                        break;
                    }
                    Err(e) => return Err(e.into()),
                }
            }
        }
        if let Some(status) = status {
            fs::write(directory.join("command.out"), &out)?;
            fs::write(directory.join("command.err"), &err)?;
            if !status.success() {
                return Err(format!(
                    "host command failed ({status}): {}",
                    diagnostic(&directory.join("command.err"))
                )
                .into());
            }
            return Ok(String::from_utf8(out)?.trim().to_string());
        }
        let mut polls = [
            libc::pollfd {
                fd: pidfd.as_ref().map_or(-1, AsRawFd::as_raw_fd),
                events: libc::POLLIN,
                revents: 0,
            },
            libc::pollfd {
                fd: if output_open[0] {
                    stdout.as_raw_fd()
                } else {
                    -1
                },
                events: libc::POLLIN,
                revents: 0,
            },
            libc::pollfd {
                fd: if output_open[1] {
                    stderr.as_raw_fd()
                } else {
                    -1
                },
                events: libc::POLLIN,
                revents: 0,
            },
        ];
        // Cancellation/deadline checks remain bounded even for silent jobs.
        let result = unsafe {
            libc::poll(
                polls.as_mut_ptr(),
                polls.len() as _,
                WORKER_TICK.as_millis() as i32,
            )
        };
        if result < 0 {
            let error = std::io::Error::last_os_error();
            if error.kind() != std::io::ErrorKind::Interrupted {
                return Err(error.into());
            }
        }
    }
}
pub fn helper_line(file: &mut File, timeout: Duration, cancel: &Cancellation) -> Result<String> {
    let deadline = std::time::Instant::now() + timeout;
    let mut data = Vec::new();
    loop {
        cancel.check()?;
        if std::time::Instant::now() >= deadline {
            return Err("helper response timed out".into());
        }
        if !unix::readable(file.as_raw_fd(), 20)? {
            continue;
        }
        let mut byte = [0];
        if file.read(&mut byte)? == 0 {
            return Err("helper exited".into());
        }
        if byte[0] == b'\n' {
            return Ok(String::from_utf8(data)?);
        }
        data.push(byte[0]);
        if data.len() > 256 {
            return Err("oversized helper response".into());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn command_drains_output_and_stops_writers_after_the_child_exits() {
        let _fork = unix::forking();
        let directory = unix::temp_directory().unwrap();
        let mut shell = Command::new("sh");
        shell.current_dir(&directory).args([
            "-c",
            "sleep 60 & echo $! > writer.pid; head -c 262144 /dev/zero; printf tail; printf diagnostic >&2",
        ]);
        let output = command(&mut shell, &directory, &Cancellation::default()).unwrap();
        assert_eq!(output.len(), 262148);
        assert!(output.ends_with("tail"));
        assert_eq!(
            fs::read(directory.join("command.err")).unwrap(),
            b"diagnostic"
        );
        let pid = fs::read_to_string(directory.join("writer.pid")).unwrap();
        // An orphan may briefly remain a zombie until the host init reaps it.
        let deadline = Instant::now() + Duration::from_secs(2);
        loop {
            let stat = fs::read_to_string(format!("/proc/{}/stat", pid.trim()));
            if stat.is_err() || stat.unwrap().rsplit_once(") ").unwrap().1.starts_with('Z') {
                break;
            }
            assert!(Instant::now() < deadline, "background writer survived");
            std::thread::sleep(Duration::from_millis(2));
        }
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn a_silent_command_with_closed_streams_still_times_out() {
        let _fork = unix::forking();
        let directory = unix::temp_directory().unwrap();
        let error = command(
            Command::new("sh").args(["-c", "exec 1>&- 2>&-; sleep 60"]),
            &directory,
            &Cancellation::default().with_limit(Duration::from_millis(100)),
        )
        .unwrap_err();
        assert!(error.to_string().contains("evaluation timed out"));
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn command_output_limits_and_failure_diagnostics_are_preserved() {
        let _fork = unix::forking();
        let directory = unix::temp_directory().unwrap();
        let error = command(
            Command::new("sh").args(["-c", "head -c 8388609 /dev/zero"]),
            &directory,
            &Cancellation::default(),
        )
        .unwrap_err();
        assert_eq!(error.to_string(), "host command output exceeds 8 MiB");
        let error = command(
            Command::new("sh").args([
                "-c",
                "head -c 2097152 /dev/zero >&2; printf failure-tail >&2; exit 9",
            ]),
            &directory,
            &Cancellation::default(),
        )
        .unwrap_err();
        assert!(error.to_string().ends_with("failure-tail"));
        assert_eq!(
            fs::metadata(directory.join("command.err")).unwrap().len(),
            1024 * 1024
        );
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn a_time_limit_fails_checks_once_it_passes() {
        let _fork = unix::forking();
        let session = Cancellation::default();
        let limited = session.child().with_limit(Duration::ZERO);
        let error = limited.check().unwrap_err().to_string();
        assert_eq!(error, "evaluation timed out after 0 s");
        assert!(limited.child().check().is_err());
        assert!(session.check().is_ok());
        assert!(session.with_limit(Duration::from_secs(600)).check().is_ok());
        assert_eq!(duration(Duration::from_secs(600)), "10 min");
        assert_eq!(duration(Duration::from_secs(90)), "90 s");
        // Cancellation is still reported as such.
        session.cancel();
        assert_eq!(
            limited.check().unwrap_err().to_string(),
            "session cancelled"
        );
    }

    #[test]
    fn startup_cancellation_observes_next_request_and_eof_without_consuming_it() {
        use std::{
            io::{Read, Write},
            os::unix::net::UnixStream,
        };
        // A concurrent test's fork must not inherit the peer and delay EOF.
        let _no_forks = unix::no_forks();
        let (mut member, mut runtime) = UnixStream::pair().unwrap();
        let cancel = Cancellation::until_readable(member.as_raw_fd()).unwrap();
        cancel.check().unwrap();
        runtime.write_all(b"Leave").unwrap();
        assert!(cancel.check().is_err());
        assert!(cancel.child().check().is_err());
        let mut bytes = [0; 5];
        member.read_exact(&mut bytes).unwrap();
        assert_eq!(&bytes, b"Leave");
        cancel.check().unwrap();
        drop(runtime);
        assert!(cancel.check().is_err());
    }

    #[test]
    fn startup_cancellation_requires_the_last_peer_descriptor_to_close() {
        use std::{io::Read, os::unix::net::UnixStream};
        let _no_forks = unix::no_forks();
        let (mut member, runtime) = UnixStream::pair().unwrap();
        let retained = runtime.try_clone().unwrap();
        let cancel = Cancellation::until_readable(member.as_raw_fd()).unwrap();
        drop(runtime);
        // As with a peer copied by fork, the retained descriptor is still live.
        cancel.check().unwrap();
        drop(retained);
        assert!(cancel.check().is_err());
        assert!(cancel.child().check().is_err());
        // The cancellation probe left real EOF for the scope event loop.
        assert_eq!(member.read(&mut [0]).unwrap(), 0);
    }
}
