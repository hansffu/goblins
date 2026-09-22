//! Sequential, cancellable host commands. No shell evaluates request text.
use crate::{Result, unix};
use std::{
    fs::{self, File},
    io::{Read, Seek, SeekFrom},
    os::{fd::AsRawFd, unix::process::CommandExt},
    path::Path,
    process::{Child, Command, Stdio},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    thread,
    time::Duration,
};
pub const WORKER_TICK: Duration = Duration::from_millis(20);
#[derive(Clone, Default)]
pub struct Cancellation {
    cancelled: Arc<AtomicBool>,
    parent: Option<Arc<AtomicBool>>,
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
        }
    }
    pub fn check(&self) -> Result<()> {
        if self.cancelled.load(Ordering::Relaxed)
            || self
                .parent
                .as_ref()
                .is_some_and(|p| p.load(Ordering::Relaxed))
        {
            Err("session cancelled".into())
        } else {
            Ok(())
        }
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
    let mut stdout = child.child.stdout.take().unwrap();
    let mut stderr = child.child.stderr.take().unwrap();
    unix::nonblocking(stdout.as_raw_fd())?;
    unix::nonblocking(stderr.as_raw_fd())?;
    let mut out = Vec::new();
    let mut err = Vec::new();
    loop {
        cancel.check()?;
        let status = child.finished()?;
        // Drain after observing exit too. Limits apply to captured bytes, not
        // Nix's own store/cache files. No process-wide file-size limit is used.
        for (reader, buffer, limit, tail) in [
            (
                &mut stdout as &mut dyn Read,
                &mut out,
                8 * 1024 * 1024,
                false,
            ),
            (&mut stderr as &mut dyn Read, &mut err, 1024 * 1024, true),
        ] {
            for _ in 0..128 {
                let mut bytes = [0; 8192];
                match reader.read(&mut bytes) {
                    Ok(0) => break,
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
        thread::sleep(WORKER_TICK);
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
