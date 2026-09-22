//! Forward published TCP ports only when a live shell selects another network.
use crate::{
    Result,
    docker::Shared,
    process::{self, Cancellation},
    unix,
};
use std::{
    collections::BTreeMap,
    fs::File,
    io::{Read, Write},
    os::{
        fd::{AsRawFd, OwnedFd},
        unix::{fs::MetadataExt, net::UnixStream, process::CommandExt},
    },
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

pub(crate) struct Forward {
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}
struct Relay(Child);
impl Drop for Relay {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}
fn ports(scope: &Shared) -> Result<BTreeMap<u16, String>> {
    // Short procfd pathname also supports deeply nested nix develop TMPDIRs.
    let directory = File::open(scope.directory.join("docker-socket"))?;
    let mut socket = UnixStream::connect(format!(
        "/proc/self/fd/{}/docker.sock",
        directory.as_raw_fd()
    ))?;
    socket.set_read_timeout(Some(Duration::from_millis(500)))?;
    socket.set_write_timeout(Some(Duration::from_millis(500)))?;
    socket.write_all(b"GET /containers/json HTTP/1.0\r\nHost: docker\r\n\r\n")?;
    // Bound wall time as well as bytes: even a socket replaced by a scope
    // member must not hold controller shutdown hostage with a slow response.
    let deadline = Instant::now() + Duration::from_millis(500);
    let mut response = Vec::new();
    let mut buffer = [0u8; 8192];
    loop {
        let remaining = deadline
            .checked_duration_since(Instant::now())
            .ok_or("Docker port query timed out")?;
        socket.set_read_timeout(Some(remaining))?;
        let n = socket.read(&mut buffer)?;
        if n == 0 {
            break;
        }
        if response.len() + n > 1024 * 1024 {
            return Err("Docker port response too large".into());
        }
        response.extend_from_slice(&buffer[..n]);
    }
    let response = String::from_utf8(response)?;
    let (_, body) = response
        .split_once("\r\n\r\n")
        .ok_or("invalid Docker response")?;
    let containers: serde_json::Value = serde_json::from_str(body)?;
    let mut ports = BTreeMap::new();
    for container in containers.as_array().ok_or("invalid container list")? {
        for port in container["Ports"].as_array().into_iter().flatten() {
            if port["Type"] != "tcp" {
                continue;
            }
            let Some(public) = port["PublicPort"]
                .as_u64()
                .filter(|p| (1..=65535).contains(p))
            else {
                continue;
            };
            let ip = port["IP"].as_str().unwrap_or("0.0.0.0");
            let Ok(ip) = ip.parse::<std::net::Ipv4Addr>() else {
                continue;
            };
            ports.entry(public as u16).or_insert_with(|| {
                if ip.is_unspecified() {
                    "127.0.0.1".into()
                } else {
                    ip.to_string()
                }
            });
            if ports.len() >= 128 {
                return Ok(ports);
            }
        }
    }
    Ok(ports)
}
impl Forward {
    pub fn start(
        helper: PathBuf,
        home: Arc<Shared>,
        target: Arc<Shared>,
        directory: &Path,
    ) -> Result<Option<Self>> {
        if home.net.metadata()?.ino() == target.net.metadata()?.ino() {
            return Ok(None);
        }
        let stop = Arc::new(AtomicBool::new(false));
        let stopped = stop.clone();
        let directory = directory.to_path_buf();
        let thread = thread::spawn(move || {
            let mut relays: BTreeMap<u16, (String, Relay)> = BTreeMap::new();
            while !stopped.load(Ordering::Relaxed) {
                if let Ok(ports) = ports(&target) {
                    relays.retain(|port, (ip, relay)| {
                        ports.get(port) == Some(ip) && matches!(relay.0.try_wait(), Ok(None))
                    });
                    for (port, ip) in ports {
                        if stopped.load(Ordering::Relaxed) {
                            break;
                        }
                        if relays.contains_key(&port) {
                            continue;
                        }
                        let spawn = || -> Result<Relay> {
                            let mut command = Command::new(&helper);
                            let keep = [
                                home.user.as_raw_fd(),
                                home.net.as_raw_fd(),
                                target.user.as_raw_fd(),
                                target.net.as_raw_fd(),
                            ];
                            command
                                .arg("--docker-forward")
                                .args(keep.map(|fd| fd.to_string()))
                                .arg(port.to_string())
                                .arg(&ip)
                                .stdin(Stdio::null())
                                .stdout(Stdio::piped())
                                .stderr(File::create(
                                    directory.join(format!("docker-forward-{port}.log")),
                                )?);
                            let parent = unsafe { libc::getpid() };
                            unsafe {
                                command.pre_exec(move || {
                                    unix::cvt(libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL))?;
                                    if libc::getppid() != parent {
                                        return Err(std::io::Error::other("controller exited"));
                                    }
                                    unix::cvt(libc::syscall(
                                        libc::SYS_close_range,
                                        3u32,
                                        u32::MAX,
                                        4u32,
                                    ) as i32)?;
                                    for fd in keep {
                                        unix::cvt(libc::fcntl(fd, libc::F_SETFD, 0))?;
                                    }
                                    Ok(())
                                });
                            }
                            let mut relay = Relay(command.spawn()?);
                            let mut output =
                                File::from(OwnedFd::from(relay.0.stdout.take().unwrap()));
                            if process::helper_line(
                                &mut output,
                                Duration::from_secs(3),
                                &Cancellation::default(),
                            )? != "READY"
                            {
                                return Err("port forwarding failed".into());
                            }
                            Ok(relay)
                        };
                        if let Ok(relay) = spawn() {
                            relays.insert(port, (ip, relay));
                        }
                    }
                }
                for _ in 0..5 {
                    if stopped.load(Ordering::Relaxed) {
                        break;
                    }
                    thread::sleep(Duration::from_millis(20));
                }
            }
        });
        Ok(Some(Self {
            stop,
            thread: Some(thread),
        }))
    }
}
impl Drop for Forward {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}
