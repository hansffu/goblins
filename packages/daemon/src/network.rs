//! Open internet via agent-sandbox's pasta dependency, in a private namespace.
use crate::{Result, process, unix};
use std::{
    fs::{self, File},
    os::{fd::AsRawFd, unix::process::CommandExt},
    path::Path,
    process::{Child, ChildStdin, Command, Stdio},
    time::Duration,
};

pub struct Network {
    process: Child,
    input: Option<ChildStdin>,
    pub user: File,
    pub net: File,
}
impl Network {
    pub fn attach(
        pasta: &Path,
        user: File,
        net: File,
        net_path: &Path,
        directory: &Path,
        cancel: &process::Cancellation,
    ) -> Result<Self> {
        let mut command = Command::new(pasta);
        command
            .args([
                "-f",
                "-q",
                "-4",
                "--config-net",
                "--no-map-gw",
                "-a",
                "10.0.2.1",
                "-g",
                "10.0.2.2",
                "-n",
                "255.255.255.0",
                "--dns-forward",
                "10.0.2.3",
                "-t",
                "none",
                "-u",
                "none",
                "-T",
                "none",
                "-U",
                "none",
                "--userns",
                &format!("/proc/{}/fd/{}", std::process::id(), user.as_raw_fd()),
                "--netns",
                &net_path.display().to_string(),
                "--pid",
                &directory.join("network.pid").display().to_string(),
            ])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(File::create(directory.join("network.log"))?);
        let parent = unsafe { libc::getpid() };
        unsafe {
            command.pre_exec(move || {
                unix::cvt(libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL))?;
                if libc::getppid() != parent {
                    return Err(std::io::ErrorKind::Interrupted.into());
                }
                unix::cvt(libc::syscall(libc::SYS_close_range, 3u32, u32::MAX, 4u32) as i32)?;
                Ok(())
            });
        }
        let mut result = Self {
            process: command.spawn()?,
            input: None,
            user,
            net,
        };
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        loop {
            cancel.check()?;
            if result.process.try_wait()?.is_some() || std::time::Instant::now() >= deadline {
                return Err(format!(
                    "Docker network startup failed: {}",
                    process::diagnostic(&directory.join("network.log"))
                )
                .into());
            }
            if directory.join("network.pid").exists() {
                return Ok(result);
            }
            std::thread::sleep(Duration::from_millis(20));
        }
    }
    pub fn start(
        pasta: &Path,
        shell: &Path,
        directory: &Path,
        cancel: &process::Cancellation,
    ) -> Result<Self> {
        // The keeper owns the namespaces; EOF on its private stdin ends it.
        // No payload or daemon protocol passes through pasta's stdio.
        let mut command = Command::new(pasta);
        command
            .args([
                "-f",
                "-q",
                "-4",
                "--config-net",
                "--no-map-gw",
                "-a",
                "10.0.2.1",
                "-g",
                "10.0.2.2",
                "-n",
                "255.255.255.0",
                "--dns-forward",
                "10.0.2.3",
                "-t",
                "none",
                "-u",
                "none",
                "-T",
                "none",
                "-U",
                "none",
                "--",
            ])
            .arg(shell)
            .args(["-c", "printf 'READY\\n'; read -r _goblins_network_lifetime"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(File::create(directory.join("network.log"))?);
        let parent = unsafe { libc::getpid() };
        unsafe {
            command.pre_exec(move || {
                unix::cvt(libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL))?;
                if libc::getppid() != parent {
                    return Err(std::io::ErrorKind::Interrupted.into());
                }
                Ok(())
            });
        }
        let mut child = command.spawn()?;
        let input = child.stdin.take();
        let output = child.stdout.take().unwrap();
        let mut output = File::from(std::os::fd::OwnedFd::from(output));
        let setup = (|| -> Result<(File, File)> {
            if process::helper_line(&mut output, Duration::from_secs(10), cancel)? != "READY" {
                return Err("invalid pasta namespace readiness".into());
            }
            // The command sees PID 1 in its own PID namespace. Obtain the host
            // PID from our still-owned child, never from agent input.
            let children = fs::read_to_string(format!("/proc/{0}/task/{0}/children", child.id()))?;
            let ids: Vec<_> = children.split_whitespace().collect();
            if ids.len() != 1 || child.try_wait()?.is_some() {
                return Err("pasta namespace keeper exited".into());
            }
            let pid: u32 = ids[0].parse()?;
            let user = File::open(format!("/proc/{pid}/ns/user"))?;
            let net = File::open(format!("/proc/{pid}/ns/net"))?;
            if child.try_wait()?.is_some() {
                return Err("pasta exited during namespace setup".into());
            }
            Ok((user, net))
        })();
        match setup {
            Ok((user, net)) => Ok(Self {
                process: child,
                input,
                user,
                net,
            }),
            Err(error) => {
                drop(input);
                let _ = child.kill();
                let _ = child.wait();
                Err(format!(
                    "network startup failed: {error}: {}",
                    process::diagnostic(&directory.join("network.log"))
                )
                .into())
            }
        }
    }
    pub fn fds(&self) -> [i32; 2] {
        [self.user.as_raw_fd(), self.net.as_raw_fd()]
    }
}
impl Drop for Network {
    fn drop(&mut self) {
        self.input.take();
        let _ = self.process.kill();
        let _ = self.process.wait();
    }
}
