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
    /// The exec's keeper: a subreaper child of init that holds everything the
    /// exec starts, even processes that leave its session and process group.
    keeper: i32,
    /// The command, which leads its own session and process group.
    pid: i32,
    /// The keeper reports the command's wait status here, once.
    status: Option<OwnedFd>,
    kill_at: Option<Instant>,
    /// Cancelled with everything the exec started, not only its group.
    everything: bool,
}

/// Processes below `root` as (pid, process group), from /proc.
fn descendants(root: i32) -> Vec<(i32, i32)> {
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
    let mut queue = vec![root];
    while let Some(parent) = queue.pop() {
        for &(pid, pgrp) in parents.get(&parent).into_iter().flatten() {
            if pid != root && !found.iter().any(|(p, _)| *p == pid) {
                found.push((pid, pgrp));
                queue.push(pid);
            }
        }
    }
    found
}

/// Read exactly `bytes.len()` bytes, or fewer at end of file.
fn read_full(fd: RawFd, bytes: &mut [u8]) -> usize {
    let mut done = 0;
    while done < bytes.len() {
        let n = unsafe { libc::read(fd, bytes[done..].as_mut_ptr().cast(), bytes.len() - done) };
        if n > 0 {
            done += n as usize;
        } else if n == 0 || io::Error::last_os_error().kind() != io::ErrorKind::Interrupted {
            break;
        }
    }
    done
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
    /// Signal the exec's process group, or with `everything` every process
    /// below its keeper. Only the keeper's own subtree is ever signalled,
    /// so a reused PID or process group elsewhere is never hit.
    fn signal(exec: &Exec, signal: i32, everything: bool) -> bool {
        let mut any = false;
        for (pid, pgrp) in descendants(exec.keeper) {
            if everything || pgrp == exec.pid {
                unsafe { libc::kill(pid, signal) };
                any = true;
            }
        }
        any
    }
    fn hang_up(exec: &Exec, everything: bool) -> bool {
        let live = Self::signal(exec, libc::SIGHUP, everything);
        Self::signal(exec, libc::SIGTERM, everything);
        Self::signal(exec, libc::SIGCONT, everything);
        live
    }
    fn cancel(&mut self, id: u64, everything: bool) {
        let Some(exec) = self.execs.get_mut(&id) else {
            return;
        };
        exec.everything |= everything;
        if Self::hang_up(exec, exec.everything) && exec.kill_at.is_none() {
            exec.kill_at = Some(Instant::now() + GRACE);
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
                    Ok((keeper, pid, status)) => {
                        self.execs.insert(
                            id,
                            Exec {
                                keeper,
                                pid,
                                status: Some(status),
                                kill_at: None,
                                everything: false,
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
                    Self::signal(exec, signal, false);
                }
            }
            Command::Signal { .. } => (),
            Command::Cancel { id, everything } => self.cancel(id, everything),
            // Everything any exec started, including processes that left its
            // group, and execs whose command already exited. Never the agent.
            Command::KillAll {} => {
                for id in self.execs.keys().copied().collect::<Vec<_>>() {
                    self.cancel(id, true);
                }
            }
        }
    }
    /// Start a keeper that runs the command. Returns the keeper, the command
    /// and the pipe the keeper reports the command's wait status on.
    fn spawn(
        &self,
        argv: Option<Vec<String>>,
        cwd: Option<String>,
        env: Vec<(String, String)>,
        tty: bool,
        fds: &[OwnedFd],
    ) -> Result<(i32, i32, OwnedFd), (i32, String)> {
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
        let pipe = || -> Result<(OwnedFd, OwnedFd), (i32, String)> {
            let mut fds = [0; 2];
            cvt(unsafe { libc::pipe2(fds.as_mut_ptr(), libc::O_CLOEXEC) })
                .map_err(|e| (126, e.to_string()))?;
            Ok(unsafe { (OwnedFd::from_raw_fd(fds[0]), OwnedFd::from_raw_fd(fds[1])) })
        };
        let (report_r, report_w) = pipe()?;
        let (status_r, status_w) = pipe()?;
        let raw: Vec<RawFd> = fds.iter().map(AsRawFd::as_raw_fd).collect();
        let keeper = unsafe { libc::fork() };
        if keeper < 0 {
            return Err((126, io::Error::last_os_error().to_string()));
        }
        if keeper == 0 {
            // The keeper: a fork of single-threaded init that never execs.
            unsafe {
                let report = report_w.as_raw_fd();
                let status = status_w.as_raw_fd();
                let fail = |stage: u8| child_failed(report, stage);
                libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL);
                // Orphans of the exec come here rather than to init.
                if libc::prctl(libc::PR_SET_CHILD_SUBREAPER, 1) < 0 {
                    fail(0);
                }
                let pid = libc::fork();
                if pid < 0 {
                    fail(0);
                }
                if pid == 0 {
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
                libc::write(status, (&pid as *const i32).cast(), 4);
                // Hold nothing but the status pipe: not the control socket,
                // the goblin's terminal or the command's stdio.
                if status > 0 {
                    libc::syscall(libc::SYS_close_range, 0u32, (status - 1) as u32, 0u32);
                }
                libc::syscall(libc::SYS_close_range, (status + 1) as u32, u32::MAX, 0u32);
                loop {
                    let mut wait = 0;
                    let child = libc::waitpid(-1, &mut wait, 0);
                    if child == pid {
                        libc::write(status, (&wait as *const i32).cast(), 4);
                        libc::close(status);
                    } else if child < 0 && *libc::__errno_location() != libc::EINTR {
                        libc::_exit(0);
                    }
                }
            }
        }
        drop(report_w);
        drop(status_w);
        let mut bytes = [0u8; 5];
        if read_full(report_r.as_raw_fd(), &mut bytes) == 0 {
            let mut pid = [0u8; 4];
            if read_full(status_r.as_raw_fd(), &mut pid) == 4 {
                let _ = unix_nonblocking(status_r.as_raw_fd());
                return Ok((keeper, i32::from_ne_bytes(pid), status_r));
            }
            bytes = [0, 0, 0, 0, 0];
            bytes[1..].copy_from_slice(&libc::EIO.to_ne_bytes());
        }
        // The command never ran: the keeper exits once it has reaped it.
        unsafe { libc::waitpid(keeper, std::ptr::null_mut(), 0) };
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
    /// The command of exec `id` exited (or its keeper vanished): report it,
    /// and hang up the rest of its group as when a terminal's session ends.
    fn exited(&mut self, id: u64) {
        let Some(exec) = self.execs.get_mut(&id) else {
            return;
        };
        let Some(status) = exec.status.take() else {
            return;
        };
        let mut wait = [0u8; 4];
        let (code, signal) = if read_full(status.as_raw_fd(), &mut wait) == 4 {
            let wait = i32::from_ne_bytes(wait);
            if libc::WIFSIGNALED(wait) {
                (None, Some(libc::WTERMSIG(wait)))
            } else {
                (Some(libc::WEXITSTATUS(wait)), None)
            }
        } else {
            (None, Some(libc::SIGKILL))
        };
        Self::signal(exec, libc::SIGHUP, false);
        Self::signal(exec, libc::SIGCONT, false);
        self.emit(Event::Exited { id, code, signal });
    }
    /// Read the statuses keepers reported.
    fn statuses(&mut self) {
        let ready: Vec<u64> = self
            .execs
            .iter()
            .filter(|(_, e)| {
                e.status
                    .as_ref()
                    .is_some_and(|s| unix_readable(s.as_raw_fd()))
            })
            .map(|(id, _)| *id)
            .collect();
        for id in ready {
            self.exited(id);
        }
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
            // A keeper exits once nothing the exec started is left.
            if let Some((&id, _)) = self.execs.iter().find(|(_, e)| e.keeper == pid) {
                self.exited(id);
                self.execs.remove(&id);
            }
        }
    }
    fn signals(&mut self) -> Option<i32> {
        let mut info: libc::signalfd_siginfo = unsafe { std::mem::zeroed() };
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
        // Statuses first: a keeper may report and exit in the same moment.
        self.statuses();
        self.reap()
    }
    fn deadlines(&mut self, now: Instant) {
        for exec in self.execs.values_mut() {
            if exec.kill_at.is_some_and(|t| t <= now) {
                // Repeat shortly while anything is left: a process may fork
                // between the scan and the signal.
                exec.kill_at = Self::signal(exec, libc::SIGKILL, exec.everything)
                    .then(|| now + Duration::from_millis(100));
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
            live |= Self::hang_up(exec, true);
        }
        let end = Instant::now() + SHUTDOWN_GRACE;
        while live && Instant::now() < end && !self.execs.is_empty() {
            self.wait(Some(end));
            self.signals();
            self.flush();
        }
        let me = unsafe { libc::getpid() };
        for _ in 0..100 {
            let left = descendants(me);
            if left.is_empty() {
                break;
            }
            for (pid, _) in left {
                unsafe { libc::kill(pid, libc::SIGKILL) };
            }
            std::thread::sleep(Duration::from_millis(10));
            self.signals();
        }
        self.signals();
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
        for status in self.execs.values().filter_map(|e| e.status.as_ref()) {
            fds.push(libc::pollfd {
                fd: status.as_raw_fd(),
                events: libc::POLLIN,
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

fn unix_nonblocking(fd: RawFd) -> io::Result<()> {
    let flags = cvt(unsafe { libc::fcntl(fd, libc::F_GETFL) })?;
    cvt(unsafe { libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) })?;
    Ok(())
}

fn unix_readable(fd: RawFd) -> bool {
    let mut p = libc::pollfd {
        fd,
        events: libc::POLLIN,
        revents: 0,
    };
    unsafe { libc::poll(&mut p, 1, 0) > 0 }
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
        state.statuses();
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
