//! Private scope namespaces exist independently of daemon activation.
use crate::{
    Result,
    config::Launch,
    docker::{map_ids, subordinate},
    process::{self, Cancellation},
    unix,
};
use std::{
    fs::{self, File},
    io::Write,
    os::{
        fd::AsRawFd,
        unix::{fs::MetadataExt, process::CommandExt},
    },
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    time::{Duration, Instant},
};

pub struct Scope {
    process: Child,
    lifetime: Option<File>,
    pub outer: File,
    pub user: File,
    pub net: File,
    pub net_path: PathBuf,
}
impl Scope {
    pub fn start(launch: &Launch, directory: &Path, cancel: &Cancellation) -> Result<Self> {
        let uid = unsafe { libc::getuid() };
        let gid = unsafe { libc::getgid() };
        if uid == 0 {
            return Err("run Docker-enabled Goblins as a non-root host user".into());
        }
        let subuid = subordinate("uid", uid)?;
        let subgid = subordinate("gid", gid)?;
        let state = directory.join("docker-scope");
        fs::create_dir(&state)?;
        let (info_r, info_w) = unix::pipe()?;
        let (gate_r, gate_w) = unix::pipe()?;
        let (life_r, life_w) = unix::pipe()?;
        let mut command = Command::new(&launch.bwrap);
        command
            .args([
                "--unshare-all",
                "--unshare-user",
                "--uid",
                "0",
                "--gid",
                "0",
                "--cap-add",
                "ALL",
                "--die-with-parent",
                "--new-session",
                "--clearenv",
                "--info-fd",
                &info_w.as_raw_fd().to_string(),
                "--userns-block-fd",
                &gate_r.as_raw_fd().to_string(),
                "--ro-bind",
                "/nix/store",
                "/nix/store",
                "--proc",
                "/proc",
                "--dev",
                "/dev",
                "--bind",
                &state.display().to_string(),
                "/run/goblins-scope",
                "--remount-ro",
                "/",
                "--",
            ])
            .arg(&launch.helper)
            .arg("--docker-scope")
            .stdin(Stdio::from(life_r))
            .stdout(Stdio::null())
            .stderr(File::create(directory.join("docker-scope.log"))?);
        let keep = [info_w.as_raw_fd(), gate_r.as_raw_fd()];
        unsafe {
            command.pre_exec(move || {
                unix::cvt(libc::syscall(libc::SYS_close_range, 3u32, u32::MAX, 4u32) as i32)?;
                for fd in keep {
                    unix::cvt(libc::fcntl(fd, libc::F_SETFD, 0))?;
                }
                Ok(())
            });
        }
        let mut child = command.spawn()?;
        drop(command);
        drop(info_w);
        drop(gate_r);
        let result = (|| -> Result<(File, File, File, PathBuf)> {
            let mut info = File::from(info_r);
            let mut json = String::new();
            for _ in 0..20 {
                let line = process::helper_line(&mut info, Duration::from_secs(10), cancel)?;
                json.push_str(&line);
                if line.trim() == "}" {
                    break;
                }
            }
            let info: serde_json::Value = serde_json::from_str(&json)?;
            let pid = u32::try_from(info["child-pid"].as_u64().ok_or("missing scope PID")?)?;
            map_ids("uid", pid, uid, subuid, directory, cancel)?;
            map_ids("gid", pid, gid, subgid, directory, cancel)?;
            File::from(gate_w).write_all(b"x")?;
            let deadline = Instant::now() + Duration::from_secs(15);
            while !state.join("ready").exists() {
                cancel.check()?;
                if child.try_wait()?.is_some() || Instant::now() >= deadline {
                    return Err("Docker scope startup failed".into());
                }
                std::thread::sleep(Duration::from_millis(20));
            }
            let nested: u32 = fs::read_to_string(state.join("ready"))?.trim().parse()?;
            let outer = File::open(format!("/proc/{pid}/ns/user"))?;
            let root = PathBuf::from(format!("/proc/{pid}/root"));
            let user = File::open(root.join(format!("proc/{nested}/ns/user")))?;
            let net = File::open(root.join(format!("proc/{nested}/ns/net")))?;
            let mut keeper = pid;
            for _ in 0..4 {
                let path = PathBuf::from(format!("/proc/{keeper}/ns/net"));
                if fs::metadata(&path)?.ino() == net.metadata()?.ino() {
                    return Ok((outer, user, net, path));
                }
                let children =
                    fs::read_to_string(format!("/proc/{keeper}/task/{keeper}/children"))?;
                let ids: Vec<_> = children.split_whitespace().collect();
                if ids.len() != 1 {
                    return Err("unexpected scope keeper children".into());
                }
                keeper = ids[0].parse()?;
            }
            Err("scope keeper identity mismatch".into())
        })();
        match result {
            Ok((outer, user, net, net_path)) => Ok(Self {
                process: child,
                lifetime: Some(File::from(life_w)),
                outer,
                user,
                net,
                net_path,
            }),
            Err(error) => {
                drop(life_w);
                let _ = child.kill();
                let _ = child.wait();
                Err(format!(
                    "{error}: {}",
                    process::diagnostic(&directory.join("docker-scope.log"))
                )
                .into())
            }
        }
    }
    pub fn alive(&mut self) -> bool {
        matches!(self.process.try_wait(), Ok(None))
    }
}
impl Drop for Scope {
    fn drop(&mut self) {
        self.lifetime.take();
        let _ = self.process.wait();
    }
}
