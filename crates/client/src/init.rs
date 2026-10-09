//! `goblins-init`: the root of a goblin's process tree inside the sandbox.
//! It starts the agent on the payload's terminal, runs host-requested execs
//! and reaps everything orphaned to it. Its control socket is inherited from
//! the daemon and is never visible as a file.
use goblins_protocol::init::{self, Command, Event};
use std::{
    collections::{BTreeMap, VecDeque},
    ffi::{CString, OsStr, OsString},
    io,
    os::{
        fd::{AsRawFd, FromRawFd, OwnedFd, RawFd},
        unix::ffi::OsStrExt,
    },
    time::{Duration, Instant},
};

/// Between SIGTERM and SIGKILL when an exec is cancelled.
const GRACE: Duration = Duration::from_secs(2);
/// How long execs may take to exit after a hangup when the agent exits.
const SHUTDOWN_GRACE: Duration = Duration::from_millis(500);

fn cvt(n: i32) -> io::Result<i32> {
    if n < 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(n)
    }
}

/// Exit code for a wait status, as a shell reports it.
fn exit_code(status: i32) -> i32 {
    if libc::WIFSIGNALED(status) {
        128 + libc::WTERMSIG(status)
    } else {
        libc::WEXITSTATUS(status)
    }
}

struct Exec {
    pid: i32,
    /// The leader has not been reaped, so its PID still names the group.
    leader: bool,
    kill_at: Option<Instant>,
}

/// Processes below init as (pid, process group), from /proc.
fn descendants() -> Vec<(i32, i32)> {
    let me = unsafe { libc::getpid() };
    let mut parents: BTreeMap<i32, Vec<(i32, i32)>> = BTreeMap::new();
    if let Ok(entries) = std::fs::read_dir("/proc") {
        for entry in entries.flatten() {
            let Some(pid) = entry
                .file_name()
                .to_str()
                .and_then(|n| n.parse::<i32>().ok())
            else {
                continue;
            };
            let Ok(stat) = std::fs::read_to_string(entry.path().join("stat")) else {
                continue;
            };
            // The command name may contain spaces and parentheses.
            let Some(rest) = stat.rfind(')').map(|i| &stat[i + 1..]) else {
                continue;
            };
            let fields: Vec<_> = rest.split_whitespace().collect();
            if let (Some(ppid), Some(pgrp)) = (
                fields.get(1).and_then(|f| f.parse::<i32>().ok()),
                fields.get(2).and_then(|f| f.parse::<i32>().ok()),
            ) {
                parents.entry(ppid).or_default().push((pid, pgrp));
            }
        }
    }
    let mut found = Vec::new();
    let mut queue = vec![me];
    while let Some(parent) = queue.pop() {
        for &(pid, pgrp) in parents.get(&parent).into_iter().flatten() {
            if pid != me && !found.iter().any(|(p, _)| *p == pid) {
                found.push((pid, pgrp));
                queue.push(pid);
            }
        }
    }
    found
}

struct Init {
    control: Option<OwnedFd>,
    outbox: VecDeque<Vec<u8>>,
    signals: OwnedFd,
    environment: Vec<(OsString, OsString)>,
    agent: i32,
    execs: BTreeMap<u64, Exec>,
}

impl Init {
    fn emit(&mut self, event: Event) {
        // A stalled daemon cannot make init grow without bound.
        if self.control.is_some()
            && self.outbox.len() < 4096
            && let Ok(bytes) = serde_json::to_vec(&event)
        {
            self.outbox.push_back(bytes);
        }
    }
    fn flush(&mut self) {
        while let (Some(control), Some(bytes)) = (&self.control, self.outbox.front()) {
            match init::send(control.as_raw_fd(), bytes, &[]) {
                Ok(()) => {
                    self.outbox.pop_front();
                }
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => return,
                Err(_) => {
                    self.control = None;
                    self.outbox.clear();
                }
            }
        }
    }
    /// Signal an exec's process group. While the leader is unreaped its PID
    /// cannot be reused, so the group signal is exact; afterwards only the
    /// group's remaining descendants of init are signalled one by one.
    fn signal_group(exec: &Exec, signal: i32) -> bool {
        if exec.leader {
            return unsafe { libc::kill(-exec.pid, signal) } == 0;
        }
        let mut any = false;
        for (pid, pgrp) in descendants() {
            if pgrp == exec.pid {
                unsafe { libc::kill(pid, signal) };
                any = true;
            }
        }
        any
    }
    fn hang_up(exec: &Exec) -> bool {
        let live = Self::signal_group(exec, libc::SIGHUP);
        Self::signal_group(exec, libc::SIGTERM);
        Self::signal_group(exec, libc::SIGCONT);
        live
    }
    fn cancel(&mut self, id: u64) {
        let Some(exec) = self.execs.get_mut(&id) else {
            return;
        };
        let live = Self::hang_up(exec);
        if live && exec.kill_at.is_none() {
            exec.kill_at = Some(Instant::now() + GRACE);
        } else if !live && !exec.leader {
            self.execs.remove(&id);
        }
    }
    fn command(&mut self, command: Command, fds: Vec<OwnedFd>) {
        match command {
            Command::Spawn {
                id,
                argv,
                cwd,
                env,
                tty,
            } => {
                let event = match self.spawn(argv, cwd, env, tty, &fds) {
                    Ok(pid) => {
                        self.execs.insert(
                            id,
                            Exec {
                                pid,
                                leader: true,
                                kill_at: None,
                            },
                        );
                        Event::Spawned { id, pid }
                    }
                    Err((code, message)) => Event::Failed { id, code, message },
                };
                self.emit(event);
            }
            Command::Signal { id, signal } if init::forwardable(signal) => {
                if let Some(exec) = self.execs.get(&id) {
                    Self::signal_group(exec, signal);
                }
            }
            Command::Signal { .. } => (),
            Command::Cancel { id } => self.cancel(id),
            Command::KillAll {} => {
                for id in self.execs.keys().copied().collect::<Vec<_>>() {
                    self.cancel(id);
                }
            }
        }
    }
    fn spawn(
        &self,
        argv: Option<Vec<String>>,
        cwd: Option<String>,
        env: Vec<(String, String)>,
        tty: bool,
        fds: &[OwnedFd],
    ) -> Result<i32, (i32, String)> {
        let invalid = |m: &str| (126, m.to_string());
        if fds.len() != if tty { 1 } else { 3 } {
            return Err(invalid("wrong number of descriptors"));
        }
        if env.len() > init::MAX_ENV || env.iter().any(|(k, _)| !init::env_name(k)) {
            return Err(invalid("invalid environment"));
        }
        let mut environment = self.environment.clone();
        for (name, value) in env {
            environment.retain(|(k, _)| k.as_bytes() != name.as_bytes());
            environment.push((name.into(), value.into()));
        }
        let lookup = |name: &str| {
            environment
                .iter()
                .find(|(k, _)| k.as_bytes() == name.as_bytes())
                .map(|(_, v)| v.clone())
        };
        // No command: the sandbox's own shell, never the host's.
        let argv: Vec<OsString> = match argv {
            Some(argv) if argv.is_empty() || argv.len() > init::MAX_ARGS => {
                return Err(invalid("invalid command"));
            }
            Some(argv) => argv.into_iter().map(OsString::from).collect(),
            None => vec![lookup("SHELL").unwrap_or_else(|| "/bin/sh".into())],
        };
        let c = |s: &OsStr| CString::new(s.as_bytes()).map_err(|_| invalid("NUL in argument"));
        let args = argv.iter().map(|a| c(a)).collect::<Result<Vec<_>, _>>()?;
        let envp = environment
            .iter()
            .map(|(k, v)| {
                let mut entry = k.clone();
                entry.push("=");
                entry.push(v);
                c(&entry)
            })
            .collect::<Result<Vec<_>, _>>()?;
        let program = &argv[0];
        // Search the exec's own PATH, which execvp would not use.
        let candidates: Vec<CString> = if program.as_bytes().contains(&b'/') {
            vec![c(program)?]
        } else {
            lookup("PATH")
                .unwrap_or_else(|| "/run/goblins/bin:/bin".into())
                .as_bytes()
                .split(|b| *b == b':')
                .filter(|d| !d.is_empty())
                .filter_map(|d| {
                    let mut path = d.to_vec();
                    path.push(b'/');
                    path.extend_from_slice(program.as_bytes());
                    CString::new(path).ok()
                })
                .collect()
        };
        let cwd = cwd.map(|d| c(OsStr::new(&d))).transpose()?;
        let mut arg_ptrs: Vec<*const libc::c_char> = args.iter().map(|a| a.as_ptr()).collect();
        arg_ptrs.push(std::ptr::null());
        let mut env_ptrs: Vec<*const libc::c_char> = envp.iter().map(|e| e.as_ptr()).collect();
        env_ptrs.push(std::ptr::null());
        let mut report = [0; 2];
        cvt(unsafe { libc::pipe2(report.as_mut_ptr(), libc::O_CLOEXEC) })
            .map_err(|e| (126, e.to_string()))?;
        let (report_r, report_w) = unsafe {
            (
                OwnedFd::from_raw_fd(report[0]),
                OwnedFd::from_raw_fd(report[1]),
            )
        };
        let raw: Vec<RawFd> = fds.iter().map(AsRawFd::as_raw_fd).collect();
        let pid = unsafe { libc::fork() };
        if pid < 0 {
            return Err((126, io::Error::last_os_error().to_string()));
        }
        if pid == 0 {
            // The child of a single-threaded process: allocation is safe, but
            // keep to plain system calls until exec.
            unsafe {
                let report = report_w.as_raw_fd();
                let fail = |stage: u8| child_failed(report, stage);
                reset_signals();
                if libc::setsid() < 0 {
                    fail(0);
                }
                if tty {
                    if libc::ioctl(raw[0], libc::TIOCSCTTY, 0) < 0 {
                        fail(0);
                    }
                    for target in 0..3 {
                        if libc::dup2(raw[0], target) < 0 {
                            fail(0);
                        }
                    }
                } else {
                    for (target, &source) in raw.iter().enumerate() {
                        if libc::dup2(source, target as i32) < 0 {
                            fail(0);
                        }
                    }
                }
                if let Some(cwd) = &cwd
                    && libc::chdir(cwd.as_ptr()) < 0
                {
                    fail(1);
                }
                let mut denied = false;
                for candidate in &candidates {
                    libc::execve(candidate.as_ptr(), arg_ptrs.as_ptr(), env_ptrs.as_ptr());
                    let errno = *libc::__errno_location();
                    denied |= errno == libc::EACCES;
                }
                if denied {
                    *libc::__errno_location() = libc::EACCES;
                }
                fail(2);
            }
        }
        drop(report_w);
        let mut bytes = [0u8; 5];
        let n = loop {
            let n = unsafe { libc::read(report_r.as_raw_fd(), bytes.as_mut_ptr().cast(), 5) };
            if n >= 0 || io::Error::last_os_error().kind() != io::ErrorKind::Interrupted {
                break n;
            }
        };
        if n <= 0 {
            return Ok(pid);
        }
        // The child never ran the command: reap it here, not as an exec.
        unsafe { libc::waitpid(pid, std::ptr::null_mut(), 0) };
        let errno = i32::from_ne_bytes([bytes[1], bytes[2], bytes[3], bytes[4]]);
        let error = io::Error::from_raw_os_error(errno);
        let name = String::from_utf8_lossy(program.as_bytes()).into_owned();
        Err(match bytes[0] {
            0 => (126, format!("cannot start a session: {error}")),
            1 => (126, format!("cannot change directory: {error}")),
            _ if errno == libc::ENOENT => (127, format!("{name}: command not found")),
            _ => (126, format!("{name}: {error}")),
        })
    }
    /// Reap exited children. Returns the agent's wait status once it exits.
    fn reap(&mut self) -> Option<i32> {
        let mut agent = None;
        loop {
            let mut status = 0;
            let pid = unsafe { libc::waitpid(-1, &mut status, libc::WNOHANG) };
            if pid <= 0 {
                return agent;
            }
            if pid == self.agent {
                agent = Some(status);
                continue;
            }
            let Some((&id, _)) = self.execs.iter().find(|(_, e)| e.leader && e.pid == pid) else {
                continue;
            };
            let (code, signal) = if libc::WIFSIGNALED(status) {
                (None, Some(libc::WTERMSIG(status)))
            } else {
                (Some(libc::WEXITSTATUS(status)), None)
            };
            self.emit(Event::Exited { id, code, signal });
            let exec = self.execs.get_mut(&id).unwrap();
            exec.leader = false;
            // As when a terminal's session ends, the rest of the group hangs up.
            let live = Self::signal_group(exec, libc::SIGHUP);
            Self::signal_group(exec, libc::SIGCONT);
            if !live || exec.kill_at.is_none() {
                self.execs.remove(&id);
            }
        }
    }
    fn signals(&mut self) -> Option<i32> {
        let mut info: libc::signalfd_siginfo = unsafe { std::mem::zeroed() };
        let mut agent = None;
        while unsafe {
            libc::read(
                self.signals.as_raw_fd(),
                (&mut info as *mut libc::signalfd_siginfo).cast(),
                std::mem::size_of::<libc::signalfd_siginfo>(),
            )
        } > 0
        {
            match info.ssi_signo as i32 {
                libc::SIGCHLD => (),
                // A signal for init is meant for the goblin it runs.
                signal => unsafe {
                    libc::kill(-self.agent, signal);
                },
            }
        }
        if let Some(status) = self.reap() {
            agent = Some(status);
        }
        agent
    }
    fn deadlines(&mut self, now: Instant) {
        let due: Vec<u64> = self
            .execs
            .iter()
            .filter(|(_, e)| e.kill_at.is_some_and(|t| t <= now))
            .map(|(id, _)| *id)
            .collect();
        for id in due {
            let exec = self.execs.get_mut(&id).unwrap();
            exec.kill_at = None;
            Self::signal_group(exec, libc::SIGKILL);
            if !exec.leader {
                self.execs.remove(&id);
            }
        }
    }
    fn control(&mut self) {
        let Some(control) = &self.control else {
            return;
        };
        let mut buffer = vec![0; init::MAX_MESSAGE];
        match init::receive(control.as_raw_fd(), &mut buffer, libc::MSG_DONTWAIT) {
            Ok((0, _)) => {
                // The daemon went away; the goblin keeps running without execs.
                self.control = None;
                self.outbox.clear();
            }
            Ok((n, fds)) => {
                if let Ok(command) = serde_json::from_slice::<Command>(&buffer[..n]) {
                    self.command(command, fds);
                }
            }
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => (),
            // A truncated message is dropped with its descriptors.
            Err(e) if e.kind() == io::ErrorKind::Other => (),
            Err(_) => {
                self.control = None;
                self.outbox.clear();
            }
        }
    }
    /// The agent exited: hang up the execs, give them a moment, then kill
    /// everything left below init.
    fn shutdown(&mut self) {
        let mut live = false;
        for exec in self.execs.values() {
            live |= Self::hang_up(exec);
        }
        let end = Instant::now() + SHUTDOWN_GRACE;
        while live && Instant::now() < end && !self.execs.is_empty() {
            self.wait(Some(end));
            self.reap();
            self.flush();
        }
        for _ in 0..100 {
            let left = descendants();
            if left.is_empty() {
                break;
            }
            for (pid, _) in left {
                unsafe { libc::kill(pid, libc::SIGKILL) };
            }
            std::thread::sleep(Duration::from_millis(10));
            self.reap();
        }
        self.reap();
        self.flush();
    }
    fn wait(&self, deadline: Option<Instant>) {
        let mut fds = vec![libc::pollfd {
            fd: self.signals.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        }];
        if let Some(control) = &self.control {
            fds.push(libc::pollfd {
                fd: control.as_raw_fd(),
                events: libc::POLLIN
                    | if self.outbox.is_empty() {
                        0
                    } else {
                        libc::POLLOUT
                    },
                revents: 0,
            });
        }
        let timeout = deadline.map_or(-1, |d| {
            d.saturating_duration_since(Instant::now())
                .as_millis()
                .min(60_000) as i32
        });
        unsafe { libc::poll(fds.as_mut_ptr(), fds.len() as _, timeout) };
    }
}

fn handled_signals() -> libc::sigset_t {
    unsafe {
        let mut set: libc::sigset_t = std::mem::zeroed();
        libc::sigemptyset(&mut set);
        for signal in [libc::SIGCHLD, libc::SIGTERM, libc::SIGINT, libc::SIGHUP] {
            libc::sigaddset(&mut set, signal);
        }
        set
    }
}

/// Report why a forked child could not run its command, then exit.
fn child_failed(report: RawFd, stage: u8) -> ! {
    unsafe {
        let errno = *libc::__errno_location();
        let mut bytes = [stage, 0, 0, 0, 0];
        bytes[1..].copy_from_slice(&errno.to_ne_bytes());
        libc::write(report, bytes.as_ptr().cast(), bytes.len());
        libc::_exit(127);
    }
}

/// Children start with no blocked signals and default dispositions.
unsafe fn reset_signals() {
    unsafe {
        let mut empty: libc::sigset_t = std::mem::zeroed();
        libc::sigemptyset(&mut empty);
        libc::sigprocmask(libc::SIG_SETMASK, &empty, std::ptr::null_mut());
        libc::signal(libc::SIGPIPE, libc::SIG_DFL);
    }
}

/// `goblins-init FD -- PROGRAM [ARGS...]`
pub fn run(args: &[String]) -> Result<i32, String> {
    let (fd, program) = match args {
        [fd, separator, program @ ..] if separator == "--" && !program.is_empty() => (
            fd.parse::<RawFd>()
                .map_err(|_| "invalid control descriptor")?,
            program,
        ),
        _ => return Err("usage: goblins-init FD -- PROGRAM [ARGS...]".into()),
    };
    if fd < 3 {
        return Err("invalid control descriptor".into());
    }
    let mut kind: i32 = 0;
    let mut length = std::mem::size_of::<i32>() as libc::socklen_t;
    if unsafe {
        libc::getsockopt(
            fd,
            libc::SOL_SOCKET,
            libc::SO_TYPE,
            (&mut kind as *mut i32).cast(),
            &mut length,
        )
    } < 0
        || kind != libc::SOCK_SEQPACKET
    {
        return Err("the control descriptor is not a seqpacket socket".into());
    }
    let control = unsafe { OwnedFd::from_raw_fd(fd) };
    let error = |e: io::Error| e.to_string();
    unsafe {
        cvt(libc::fcntl(fd, libc::F_SETFD, libc::FD_CLOEXEC)).map_err(error)?;
        // Not dumpable: the goblin cannot open /proc/<init>/fd as the same uid.
        cvt(libc::prctl(libc::PR_SET_DUMPABLE, 0)).map_err(error)?;
        cvt(libc::prctl(libc::PR_SET_CHILD_SUBREAPER, 1)).map_err(error)?;
        let set = handled_signals();
        cvt(libc::sigprocmask(
            libc::SIG_BLOCK,
            &set,
            std::ptr::null_mut(),
        ))
        .map_err(error)?;
        libc::signal(libc::SIGPIPE, libc::SIG_IGN);
    }
    let signals = unsafe {
        let set = handled_signals();
        OwnedFd::from_raw_fd(
            cvt(libc::signalfd(
                -1,
                &set,
                libc::SFD_CLOEXEC | libc::SFD_NONBLOCK,
            ))
            .map_err(error)?,
        )
    };
    unsafe { cvt(libc::fcntl(fd, libc::F_SETFL, libc::O_NONBLOCK)).map_err(error)? };
    let args = program
        .iter()
        .map(|a| CString::new(a.as_bytes()).map_err(|_| "NUL in argument".to_string()))
        .collect::<Result<Vec<_>, _>>()?;
    let mut pointers: Vec<*const libc::c_char> = args.iter().map(|a| a.as_ptr()).collect();
    pointers.push(std::ptr::null());
    let agent = unsafe { libc::fork() };
    if agent < 0 {
        return Err(io::Error::last_os_error().to_string());
    }
    if agent == 0 {
        unsafe {
            reset_signals();
            // The agent leads its own session with the payload terminal as
            // its controlling terminal; init is in no terminal's session.
            libc::setsid();
            if libc::isatty(0) == 1 {
                libc::ioctl(0, libc::TIOCSCTTY, 0);
            }
            libc::execvp(pointers[0], pointers.as_ptr());
            let message = format!(
                "goblins-init: cannot execute {}: {}\n",
                program[0],
                io::Error::last_os_error()
            );
            libc::write(2, message.as_ptr().cast(), message.len());
            libc::_exit(127);
        }
    }
    let mut state = Init {
        control: Some(control),
        outbox: VecDeque::new(),
        signals,
        environment: std::env::vars_os().collect(),
        agent,
        execs: BTreeMap::new(),
    };
    let status = loop {
        let deadline = state.execs.values().filter_map(|e| e.kill_at).min();
        state.wait(deadline);
        if let Some(status) = state.signals() {
            break status;
        }
        state.control();
        state.deadlines(Instant::now());
        state.flush();
    };
    state.shutdown();
    Ok(exit_code(status))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn wait_statuses_map_to_shell_exit_codes() {
        assert_eq!(exit_code(7 << 8), 7);
        assert_eq!(exit_code(libc::SIGKILL), 128 + libc::SIGKILL);
        assert!(run(&["1".into(), "--".into(), "sh".into()]).is_err());
        assert!(run(&["x".into()]).is_err());
    }
}
