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
pub struct Cancellation(Arc<AtomicBool>);
impl Cancellation {
    pub fn cancel(&self) {
        self.0.store(true, Ordering::Relaxed);
    }
    pub fn check(&self) -> Result<()> {
        if self.0.load(Ordering::Relaxed) {
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
    cancel.check()?;
    let stdout = directory.join("command.out");
    let stderr = directory.join("command.err");
    command
        .stdin(Stdio::null())
        .stdout(File::create(&stdout)?)
        .stderr(File::create(&stderr)?)
        .process_group(0);
    let parent = unsafe { libc::getpid() };
    unsafe {
        command.pre_exec(move || {
            unix::cvt(libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL))?;
            if libc::getppid() != parent {
                return Err(std::io::Error::from_raw_os_error(libc::ESRCH));
            }
            unix::cvt(libc::syscall(libc::SYS_close_range, 3u32, u32::MAX, 4u32) as i32)?;
            Ok(())
        });
    }
    let mut child = CommandChild {
        child: command.spawn()?,
        reaped: false,
    };
    loop {
        cancel.check()?;
        if let Some(status) = child.finished()? {
            if !status.success() {
                return Err(
                    format!("host command failed ({status}): {}", diagnostic(&stderr)).into(),
                );
            }
            return Ok(fs::read_to_string(stdout)?.trim().to_string());
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
