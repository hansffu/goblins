//! Terminal I/O stays with the runtime after the sandbox stops, until its
//! retained output and frontend handoff no longer need it.
use crate::{engine::EVENTS, screen::Matcher, server_log::ServerLog, terminal::Terminal};
use goblins_protocol::contract::{self, Completion};
use goblins_runtime::{
    Event, Replay, TerminalId, TerminalStatus,
    terminal::{self, Call, Condition},
};
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, VecDeque},
    io::Write,
    os::{fd::OwnedFd, unix::net::UnixStream},
    path::{Path, PathBuf},
    time::{Duration, Instant},
};

pub(crate) const UPGRADE: &str =
    "this goblin starts a client at each attach; upgrade Goblins to attach";
/// After its program exits, how long a terminal keeps reading its output,
/// and then how long an attached frontend gets to read what is queued.
const DRAIN: Duration = Duration::from_secs(1);
/// The lines of the screen an exit report holds.
const EXIT_REPORT_LINES: usize = 20;

/// What a terminal the supervisor emulates keeps beyond its PTY.
struct Pane {
    /// How the program ended, once init said so.
    exit: Option<(Option<i32>, Option<i32>)>,
    exited_at: Option<Instant>,
    waits: Vec<Wait>,
    /// The waits and the exit are reported; the frontend is on its last
    /// second.
    closing: Option<Instant>,
}
struct Wait {
    request: u64,
    /// None waits for the exit alone.
    matcher: Option<Matcher>,
    /// What `lines` asked for, if anything.
    lines: Option<usize>,
    deadline: Instant,
}
impl Wait {
    fn scan(&self) -> usize {
        self.lines.unwrap_or(terminal::DEFAULT_LINES)
    }
}
struct Entry {
    io: Terminal,
    reported: TerminalStatus,
    exposed: bool,
    exited: bool,
    pane: Option<Pane>,
}

fn matched(line: String, cut: bool) -> Value {
    json!(contract::TerminalWaitMatchedResult {
        status: contract::TerminalWaitMatchedResultStatus::Matched,
        line,
        truncated: cut.then_some(true)
    })
}
/// The answer to a wait of a terminal that has ended.
fn exit_result(entry: &mut Entry, wait: &Wait) -> Value {
    if let Some(matcher) = &wait.matcher
        && let Some((line, cut)) = entry.io.screen().and_then(|s| s.find(matcher, wait.scan()))
    {
        return matched(line, cut);
    }
    let (code, signal) = entry.pane.as_ref().and_then(|p| p.exit).unwrap_or_default();
    let lines = wait.lines.unwrap_or(terminal::EXIT_LINES);
    let (text, cut) = entry
        .io
        .screen()
        .map(|s| s.last_lines(lines))
        .unwrap_or_default();
    json!(contract::TerminalWaitExitedResult {
        status: contract::TerminalWaitExitedResultStatus::Exited,
        code,
        signal,
        text,
        truncated: cut.then_some(true)
    })
}

impl Entry {
    /// Advance a terminal the supervisor emulates: answer the waits its
    /// output or the clock settled, and when its program has exited and its
    /// output is read, report the waits left and the exit. Returns whether
    /// the exit was reported by this call.
    fn pane_tick(&mut self, id: u64, ended_goblin: bool, events: &mut VecDeque<Event>) -> bool {
        let now = Instant::now();
        let fed = self.io.take_fed();
        let eof = self.io.eof;
        let Some(pane) = self.pane.as_mut() else {
            return false;
        };
        if ended_goblin && pane.exit.is_none() {
            pane.exit = Some((None, None));
            pane.exited_at = Some(now);
        }
        let drained = pane
            .exited_at
            .is_some_and(|at| eof || ended_goblin || now >= at + DRAIN);
        if pane.closing.is_some() {
            return false;
        }
        let waits = std::mem::take(&mut pane.waits);
        let mut kept = vec![];
        let mut stalled = false;
        for wait in waits {
            if events.len() == EVENTS {
                // Answered when there is room.
                stalled = true;
                kept.push(wait);
                continue;
            }
            let found = fed
                .then(|| {
                    let matcher = wait.matcher.as_ref()?;
                    self.io.screen()?.find(matcher, wait.scan())
                })
                .flatten();
            let result = if drained {
                Some(exit_result(self, &wait))
            } else if let Some((line, cut)) = found {
                Some(matched(line, cut))
            } else if now >= wait.deadline {
                Some(json!(contract::TerminalWaitTimeoutResult {
                    status: contract::TerminalWaitTimeoutResultStatus::Timeout
                }))
            } else {
                None
            };
            match result {
                Some(result) => events.push_back(Event::TerminalResult {
                    request: wait.request,
                    result: Ok(result),
                }),
                None => kept.push(wait),
            }
        }
        if stalled {
            // The output they have not seen is still new to them.
            self.io.refeed();
        }
        let pane = self.pane.as_mut().unwrap();
        pane.waits = kept;
        let mut reported = false;
        if drained && pane.waits.is_empty() && events.len() < EVENTS {
            let (code, signal) = pane.exit.unwrap_or_default();
            events.push_back(Event::Exited { id, code, signal });
            pane.closing = Some(now + DRAIN);
            reported = true;
        }
        reported
    }
}
#[derive(Default)]
pub(crate) struct Terminals {
    entries: BTreeMap<TerminalId, Entry>,
    log: Option<ServerLog>,
    log_path: Option<PathBuf>,
    cursor: Option<TerminalId>,
    /// The size the main terminal's PTY is opened with.
    size: (u16, u16),
}
impl Terminals {
    /// The main terminal, whose PTY is opened with `size` (rows, columns).
    pub fn new(directory: &Path, detached: bool, size: (u16, u16)) -> crate::Result<Self> {
        Self::with_listener(directory, detached, None, size)
    }
    pub fn with_listener(
        directory: &Path,
        detached: bool,
        listener: Option<OwnedFd>,
        size: (u16, u16),
    ) -> crate::Result<Self> {
        let path = directory.join("terminal.sock");
        let mut io = match listener {
            Some(fd) => Terminal::from_listener(&path, std::os::unix::net::UnixListener::from(fd))?,
            None => Terminal::new(&path)?,
        };
        io.detached = detached;
        io.hold();
        let mut this = Self {
            log_path: Some(directory.join("server.log")),
            size,
            ..Default::default()
        };
        this.insert(TerminalId::Main, io, true);
        Ok(this)
    }
    fn insert(&mut self, id: TerminalId, io: Terminal, exposed: bool) {
        self.entries.insert(
            id,
            Entry {
                reported: io.status(),
                io,
                exposed,
                exited: false,
                pane: None,
            },
        );
    }
    /// A terminal the supervisor emulates, for process `id`.
    /// With `exit_report`, request ID that is answered with the exit and the
    /// last lines of the screen when the program ends: a wait for the exit
    /// that exists from the start.
    pub fn pane(
        &mut self,
        id: u64,
        master: OwnedFd,
        rows: u16,
        cols: u16,
        exit_report: Option<u64>,
    ) -> crate::Result<()> {
        self.insert(
            TerminalId::Process(id),
            Terminal::emulated(master, rows, cols)?,
            true,
        );
        let waits = exit_report
            .map(|request| Wait {
                request,
                matcher: None,
                lines: Some(EXIT_REPORT_LINES),
                // No timeout: only the exit answers it.
                deadline: Instant::now() + Duration::from_secs(100 * 365 * 24 * 3600),
            })
            .into_iter()
            .collect();
        self.entries.get_mut(&TerminalId::Process(id)).unwrap().pane = Some(Pane {
            exit: None,
            exited_at: None,
            waits,
            closing: None,
        });
        Ok(())
    }
    /// Process `id` has a terminal here.
    pub fn has(&self, id: u64) -> bool {
        self.entries.contains_key(&TerminalId::Process(id))
    }
    /// Process `id` is a terminal the supervisor emulates, and still here.
    pub fn is_pane(&self, id: u64) -> bool {
        self.entries
            .get(&TerminalId::Process(id))
            .is_some_and(|e| e.pane.is_some())
    }
    /// Process `id` is gone: whether a terminal the supervisor emulates
    /// holds it back until its output is read, so the exit is reported
    /// then.
    pub fn pane_exited(&mut self, id: u64, code: Option<i32>, signal: Option<i32>) -> bool {
        let Some(pane) = self
            .entries
            .get_mut(&TerminalId::Process(id))
            .and_then(|e| e.pane.as_mut())
        else {
            return false;
        };
        pane.exit.get_or_insert((code, signal));
        pane.exited_at.get_or_insert_with(Instant::now);
        true
    }
    /// Answer or hold a call on terminal `id`.
    pub fn call(&mut self, request: u64, id: u64, call: Call, events: &mut VecDeque<Event>) {
        if let Some(result) = self.answer(request, id, call) {
            events.push_back(Event::TerminalResult { request, result });
        }
    }
    fn answer(&mut self, request: u64, id: u64, call: Call) -> Option<Result<Value, String>> {
        let Some(entry) = self
            .entries
            .get_mut(&TerminalId::Process(id))
            .filter(|e| e.pane.as_ref().is_some_and(|p| p.closing.is_none()))
        else {
            return Some(Err("no such terminal".into()));
        };
        if let Err(message) = terminal::validate(&call) {
            return Some(Err(message));
        }
        let exited = entry.pane.as_ref().is_some_and(|p| p.exit.is_some());
        match call {
            Call::Read { source, lines } => {
                Some(Ok(entry.io.screen()?.read(source, lines)))
            }
            Call::Send { text, keys } => {
                if exited {
                    return Some(Err("the terminal's program has ended".into()));
                }
                let bytes = entry.io.screen()?.input(text.as_deref(), &keys);
                Some(
                    entry
                        .io
                        .input(&bytes)
                        .map(|()| json!(contract::ServerStopResult { accepted: true }))
                        .map_err(|e| e.to_string()),
                )
            }
            Call::Wait {
                condition,
                timeout,
                lines,
            } => {
                let matcher = match condition {
                    Condition::Match(text) => Some(Matcher::Text(text)),
                    Condition::Regex(pattern) => match terminal::compile(&pattern) {
                        Ok(regex) => Some(Matcher::Regex(regex)),
                        Err(message) => return Some(Err(message)),
                    },
                    Condition::Exit => None,
                };
                let wait = Wait {
                    request,
                    matcher,
                    lines,
                    deadline: Instant::now() + Duration::from_millis(timeout),
                };
                // Output already there matches.
                if let Some(matcher) = &wait.matcher
                    && let Some((line, cut)) = entry.io.screen()?.find(matcher, wait.scan())
                {
                    return Some(Ok(matched(line, cut)));
                }
                entry.pane.as_mut()?.waits.push(wait);
                None
            }
        }
    }
    /// Forget a wait nobody wants any more.
    pub fn cancel_wait(&mut self, request: u64) {
        for pane in self.entries.values_mut().filter_map(|e| e.pane.as_mut()) {
            pane.waits.retain(|w| w.request != request);
        }
    }
    pub fn mode(&mut self, server: bool) {
        if let Some(main) = self.entries.get_mut(&TerminalId::Main) {
            if server {
                main.io.refuse(UPGRADE);
            } else {
                main.io.release();
            }
        }
    }
    pub fn started(&mut self, master: OwnedFd, server: bool) -> crate::Result<()> {
        if server {
            self.log = Some(ServerLog::new(
                master,
                self.log_path.clone().ok_or("missing server log path")?,
            )?);
            if let Some(main) = self.entries.get_mut(&TerminalId::Main) {
                main.io.refuse(UPGRADE);
                main.io.detached = true;
            }
        } else if let Some(main) = self.entries.get_mut(&TerminalId::Main) {
            main.io.emulate(self.size.0, self.size.1);
            main.io.master(master)?;
        }
        Ok(())
    }
    pub fn failed(&mut self) {
        if let Some(main) = self.entries.get_mut(&TerminalId::Main) {
            main.io.failed_start();
        }
    }
    pub fn process(&mut self, id: u64, master: OwnedFd, rows: u16, cols: u16) -> crate::Result<()> {
        self.insert(
            TerminalId::Process(id),
            Terminal::client(master, rows, cols)?,
            false,
        );
        Ok(())
    }
    pub fn spawn_failed(&mut self, id: u64) {
        self.entries.remove(&TerminalId::Process(id));
    }
    pub fn exited(&mut self, id: u64) {
        if let Some(entry) = self.entries.get_mut(&TerminalId::Process(id)) {
            entry.exited = true;
        }
    }
    pub fn init_lost(&mut self) {
        for (id, entry) in &mut self.entries {
            if matches!(id, TerminalId::Process(_)) {
                entry.exited = true;
                if let Some(pane) = &mut entry.pane {
                    pane.exit.get_or_insert((None, None));
                    pane.exited_at.get_or_insert_with(Instant::now);
                }
            }
        }
    }
    pub fn attach(
        &mut self,
        id: TerminalId,
        mut stream: UnixStream,
        rows: u16,
        cols: u16,
        replay: Replay,
        events: &mut VecDeque<Event>,
    ) {
        assert!(
            events.len() < crate::engine::EVENTS,
            "attach needs event capacity"
        );
        let mut diagnostic = None;
        let result: crate::Result<()> = match self.entries.get_mut(&id) {
            Some(entry) => {
                entry.exposed = true;
                stream.try_clone().map_err(Into::into).and_then(|peer| {
                    // The snapshot keeps the size the screen had: the
                    // frontend restores it, then resizes as the screen does.
                    entry.io.attach(
                        peer,
                        serde_json::json!(contract::attach_exchanges::Reply::ID),
                        replay == Replay::Snapshot,
                    )?;
                    // Only an accepted consumer may change the size. An
                    // ioctl error does not turn an accepted stream into a
                    // second, contradictory refusal frame.
                    diagnostic = entry.io.resize(rows, cols).err().map(|e| e.to_string());
                    if entry.io.screen().is_some() {
                        // Format for the accepted frontend's dimensions.
                        if replay == Replay::Vt {
                            entry.io.redraw();
                        }
                        entry.io.winch();
                    }
                    Ok(())
                })
            }
            None => Err("terminal already completed".into()),
        };
        if let Err(error) = result {
            // The daemon has already reserved the slot; report the actual
            // state even when the terminal disappeared before Attach arrived.
            let _ = stream.set_nonblocking(true);
            let error = contract::completion_error::<contract::attach_exchanges::Reply>(
                &error.to_string(),
            );
            if let Ok(bytes) = goblins_jsonrpc::encode(&error) {
                let _ = stream.write_all(&bytes);
            }
        }
        let status = self
            .entries
            .get(&id)
            .map(|e| e.io.status())
            .unwrap_or(TerminalStatus {
                complete: true,
                ..Default::default()
            });
        if let Some(entry) = self.entries.get_mut(&id) {
            entry.reported = status;
        }
        events.push_back(Event::Terminal {
            terminal: id,
            status,
            error: diagnostic,
        });
    }
    pub fn resize(&mut self, id: TerminalId, rows: u16, cols: u16, events: &mut VecDeque<Event>) {
        if let Some(entry) = self.entries.get_mut(&id) {
            if let Err(error) = entry.io.resize(rows, cols) {
                events.push_back(Event::Terminal {
                    terminal: id,
                    status: entry.io.status(),
                    error: Some(error.to_string()),
                });
            }
        }
    }
    pub fn detach(&mut self, id: TerminalId, events: &mut VecDeque<Event>) {
        assert!(
            events.len() < crate::engine::EVENTS,
            "detach needs event capacity"
        );
        if let Some(entry) = self.entries.get_mut(&id) {
            let error = entry.io.detach().err().map(|e| e.to_string());
            entry.reported = entry.io.status();
            events.push_back(Event::Terminal {
                terminal: id,
                status: entry.reported,
                error,
            });
        }
    }
    /// Advance every terminal. Returns the processes of emulated terminals
    /// whose exit was just reported: everything they started is to be
    /// cancelled, as a descendant may still hold the PTY.
    pub fn tick(&mut self, ended: bool, events: &mut VecDeque<Event>) -> Vec<u64> {
        let mut cancel = vec![];
        if events.len() == crate::engine::EVENTS {
            return cancel;
        }
        if let Some(log) = &mut self.log {
            log.tick();
        }
        let ids: Vec<_> = self.entries.keys().copied().collect();
        let split = self
            .cursor
            .map_or(0, |cursor| ids.partition_point(|id| *id <= cursor));
        for id in ids[split..].iter().chain(&ids[..split]).copied() {
            if events.len() == crate::engine::EVENTS {
                break;
            }
            self.cursor = Some(id);
            let entry = self.entries.get_mut(&id).unwrap();
            let error = entry.io.tick().err().map(|e| e.to_string());
            if error.is_some() {
                entry.io.interrupted = true;
            }
            let mut left = false;
            if let TerminalId::Process(process) = id
                && entry.pane.is_some()
            {
                if entry.pane_tick(process, ended, events) {
                    cancel.push(process);
                }
                // The frontend of an ended terminal reads what is queued
                // for one more second.
                if let Some(closing) = entry.pane.as_ref().and_then(|p| p.closing) {
                    if entry.io.connected() && Instant::now() >= closing {
                        entry.io.close_stream();
                    }
                    left = !entry.io.connected();
                }
                if events.len() == crate::engine::EVENTS {
                    // No room to report: the next tick does.
                    continue;
                }
            }
            let mut status = entry.io.status();
            let remove = if entry.pane.is_some() {
                left
            } else {
                (ended && (!entry.exposed || entry.io.can_evict()))
                    || (matches!(id, TerminalId::Process(_))
                        && entry.exited
                        && !status.connected
                        && entry.io.can_evict())
            };
            if remove {
                status.connected = false;
                status.attached = false;
            }
            if entry.exposed && (status != entry.reported || error.is_some()) {
                events.push_back(Event::Terminal {
                    terminal: id,
                    status,
                    error,
                });
                entry.reported = status;
            }
            if remove {
                self.entries.remove(&id);
            }
        }
        cancel
    }
    pub fn status(&self, id: TerminalId) -> TerminalStatus {
        self.entries
            .get(&id)
            .map(|e| e.io.status())
            .unwrap_or(TerminalStatus {
                eof: true,
                complete: true,
                ..Default::default()
            })
    }
    pub fn drained(&self) -> bool {
        self.entries.is_empty() && self.log.as_ref().is_none_or(ServerLog::drained)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use goblins_unix as unix;
    use std::{
        fs,
        io::{Read, Write},
        os::fd::AsRawFd,
    };
    #[test]
    fn configuration_holds_the_legacy_socket_then_refuses_server_frontends() {
        let directory = unix::temp_directory().unwrap();
        let mut terminals = Terminals::new(&directory, false, (24, 80)).unwrap();
        let mut events = VecDeque::new();
        let mut frontend = UnixStream::connect(directory.join("terminal.sock")).unwrap();
        terminals.tick(false, &mut events);
        assert!(!unix::readable(frontend.as_raw_fd(), 0).unwrap());
        terminals.mode(true);
        terminals.tick(false, &mut events);
        assert_eq!(
            goblins_jsonrpc::read(&mut frontend).unwrap()["error"]["message"],
            UPGRADE
        );
        assert!(!terminals.entries[&TerminalId::Main].io.connected());
        drop(terminals);
        assert!(!directory.join("terminal.sock").exists());
        fs::remove_dir_all(directory).unwrap();
    }
    #[test]
    fn a_stopped_payload_keeps_its_output_for_a_late_legacy_frontend() {
        let _no_forks = unix::no_forks();
        let directory = unix::temp_directory().unwrap();
        let mut terminals = Terminals::new(&directory, false, (24, 80)).unwrap();
        let (master, slave) = unix::pty(24, 80).unwrap();
        terminals.started(master, false).unwrap();
        let mut slave = fs::File::from(slave);
        slave.write_all(b"retained\0payload").unwrap();
        drop(slave);
        let mut events = VecDeque::new();
        terminals.tick(true, &mut events);
        assert!(!terminals.drained(), "unread output retains the runtime");
        let mut frontend = UnixStream::connect(directory.join("terminal.sock")).unwrap();
        // One read per bounded tick: output and EOF can be separate reads.
        for _ in 0..4 {
            terminals.tick(true, &mut events);
        }
        assert_eq!(
            goblins_jsonrpc::read(&mut frontend).unwrap()["result"]["attached"],
            true
        );
        let mut output = vec![];
        frontend.read_to_end(&mut output).unwrap();
        // The frontend gets the screen, where the terminal ignored the NUL.
        let mut screen = crate::screen::Screen::new(24, 80);
        screen.feed(&output);
        assert_eq!(screen.visible(), ["retainedpayload"]);
        assert!(terminals.drained());
        assert!(events.iter().any(|event| matches!(event, Event::Terminal { terminal: TerminalId::Main, status, .. } if status.complete && !status.connected && !status.attached)));
        fs::remove_dir_all(directory).unwrap();
    }
    #[test]
    fn process_commands_preserve_resize_and_half_closed_attachment_ownership() {
        let directory = unix::temp_directory().unwrap();
        let mut terminals = Terminals::default();
        let (master, slave) = unix::pty(24, 80).unwrap();
        terminals.process(9, master, 24, 80).unwrap();
        let (server, mut frontend) = UnixStream::pair().unwrap();
        let id = TerminalId::Process(9);
        let mut events = VecDeque::new();
        terminals.attach(id, server, 30, 100, Replay::Vt, &mut events);
        terminals.resize(id, 40, 120, &mut events);
        let mut size: libc::winsize = unsafe { std::mem::zeroed() };
        unix::cvt(unsafe { libc::ioctl(slave.as_raw_fd(), libc::TIOCGWINSZ, &mut size) }).unwrap();
        assert_eq!((size.ws_row, size.ws_col), (40, 120));
        terminals.tick(false, &mut events);
        assert_eq!(
            goblins_jsonrpc::read(&mut frontend).unwrap()["result"]["attached"],
            true
        );
        terminals.detach(id, &mut events);
        let status = terminals.entries[&id].io.status();
        assert!(status.connected && status.detached && !status.attached);
        assert_eq!(frontend.read(&mut [0; 1]).unwrap(), 0);
        let (second, mut refused) = UnixStream::pair().unwrap();
        terminals.attach(id, second, 20, 80, Replay::Vt, &mut events);
        assert_eq!(
            goblins_jsonrpc::read(&mut refused).unwrap()["error"]["message"],
            "terminal is already attached"
        );
        unix::cvt(unsafe { libc::ioctl(slave.as_raw_fd(), libc::TIOCGWINSZ, &mut size) }).unwrap();
        assert_eq!(
            (size.ws_row, size.ws_col),
            (40, 120),
            "refusal must not resize the attached terminal"
        );
        frontend.shutdown(std::net::Shutdown::Both).unwrap();
        terminals.tick(false, &mut events);
        assert!(!terminals.entries[&id].io.connected());
        let (next, mut accepted) = UnixStream::pair().unwrap();
        terminals.attach(id, next, 20, 80, Replay::Vt, &mut events);
        terminals.tick(false, &mut events);
        assert_eq!(
            goblins_jsonrpc::read(&mut accepted).unwrap()["result"]["attached"],
            true
        );
        accepted.shutdown(std::net::Shutdown::Both).unwrap();
        terminals.exited(9);
        terminals.tick(false, &mut events);
        assert!(!terminals.entries.contains_key(&id));
        // Attaching after eviction clears the daemon's tentative reservation.
        let (missing, mut refused) = UnixStream::pair().unwrap();
        terminals.attach(id, missing, 20, 80, Replay::Vt, &mut events);
        assert_eq!(
            goblins_jsonrpc::read(&mut refused).unwrap()["error"]["message"],
            "terminal already completed"
        );
        assert!(
            matches!(events.back(), Some(Event::Terminal { status, .. }) if status.complete && !status.connected)
        );
        fs::remove_dir_all(directory).unwrap();
    }
    #[test]
    fn a_server_log_drains_after_stop_before_the_runtime_ends() {
        let _no_forks = unix::no_forks();
        let directory = unix::temp_directory().unwrap();
        let mut terminals = Terminals::new(&directory, false, (24, 80)).unwrap();
        let (output, input) = unix::pipe().unwrap();
        terminals.started(output, true).unwrap();
        let mut input = fs::File::from(input);
        input.write_all(b"final\0diagnostic").unwrap();
        let mut events = VecDeque::new();
        terminals.tick(true, &mut events);
        assert!(
            !terminals.drained(),
            "the server still owns its output writer"
        );
        drop(input);
        terminals.tick(true, &mut events);
        assert!(terminals.drained());
        assert_eq!(
            fs::read(directory.join("server.log")).unwrap(),
            b"final\0diagnostic"
        );
        fs::remove_dir_all(directory).unwrap();
    }

    /// A slave that passes bytes through untouched, so input is not echoed.
    fn raw(slave: &std::os::fd::OwnedFd) {
        unsafe {
            let mut mode: libc::termios = std::mem::zeroed();
            libc::tcgetattr(slave.as_raw_fd(), &mut mode);
            libc::cfmakeraw(&mut mode);
            libc::tcsetattr(slave.as_raw_fd(), libc::TCSANOW, &mode);
        }
        unix::nonblocking(slave.as_raw_fd()).unwrap();
    }
    fn result(events: &mut VecDeque<Event>, request: u64) -> Result<Value, String> {
        let at = events
            .iter()
            .position(|e| matches!(e, Event::TerminalResult { request: r, .. } if *r == request))
            .unwrap_or_else(|| panic!("no result for {request}"));
        match events.remove(at) {
            Some(Event::TerminalResult { result, .. }) => result,
            _ => unreachable!(),
        }
    }
    fn wait(condition: Condition, timeout: u64) -> Call {
        Call::Wait {
            condition,
            timeout,
            lines: None,
        }
    }
    fn read(terminals: &mut Terminals, id: u64, events: &mut VecDeque<Event>) -> Value {
        terminals.call(
            99,
            id,
            Call::Read {
                source: terminal::Source::Recent,
                lines: None,
            },
            events,
        );
        result(events, 99).unwrap()
    }
    fn pane(id: u64) -> (Terminals, std::os::fd::OwnedFd, VecDeque<Event>) {
        let mut terminals = Terminals::default();
        let (master, slave) = unix::pty(24, 80).unwrap();
        raw(&slave);
        terminals.pane(id, master, 24, 80, None).unwrap();
        (terminals, slave, VecDeque::new())
    }
    fn print(slave: &std::os::fd::OwnedFd, text: &str) {
        let mut slave = fs::File::from(slave.try_clone().unwrap());
        slave.write_all(text.as_bytes()).unwrap();
    }

    #[test]
    fn a_pane_reads_without_a_frontend_and_answers_reads_and_waits() {
        let _no_forks = unix::no_forks();
        let (mut terminals, slave, mut events) = pane(5);
        print(&slave, "building\r\nstep 1 done\r\n");
        terminals.tick(false, &mut events);
        let read = read(&mut terminals, 5, &mut events);
        assert_eq!(read["text"], "building\nstep 1 done");
        // Output already there matches at once.
        terminals.call(1, 5, wait(Condition::Match("step 1".into()), 5000), &mut events);
        assert_eq!(result(&mut events, 1).unwrap()["line"], "step 1 done");
        // Later output matches after it arrives.
        terminals.call(2, 5, wait(Condition::Regex("^ready$".into()), 5000), &mut events);
        terminals.tick(false, &mut events);
        assert!(events.is_empty());
        print(&slave, "ready\r\n");
        terminals.tick(false, &mut events);
        assert_eq!(
            result(&mut events, 2).unwrap(),
            json!({"status":"matched","line":"ready"})
        );
        // A wait nobody satisfies times out; a cancelled one answers nothing.
        terminals.call(3, 5, wait(Condition::Match("never".into()), 0), &mut events);
        terminals.call(4, 5, wait(Condition::Match("never".into()), 0), &mut events);
        terminals.cancel_wait(4);
        terminals.tick(false, &mut events);
        assert_eq!(result(&mut events, 3).unwrap(), json!({"status":"timeout"}));
        assert!(events.is_empty());
        // Calls on a terminal that is not here, or with bad parameters, fail.
        terminals.call(6, 77, wait(Condition::Exit, 1), &mut events);
        assert_eq!(result(&mut events, 6).unwrap_err(), "no such terminal");
        terminals.call(7, 5, wait(Condition::Regex("(".into()), 1), &mut events);
        assert!(result(&mut events, 7).is_err());
    }

    #[test]
    fn a_chatty_program_never_blocks_with_nobody_attached() {
        let _no_forks = unix::no_forks();
        let (mut terminals, slave, mut events) = pane(5);
        let chunk = "x".repeat(79) + "\r\n";
        let mut slave_file = fs::File::from(slave.try_clone().unwrap());
        let mut written = 0;
        // Far more than the PTY and the old queue limit hold.
        while written < 4 * 1024 * 1024 {
            match slave_file.write(chunk.as_bytes()) {
                Ok(n) => written += n,
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => (),
                Err(e) => panic!("{e}"),
            }
            terminals.tick(false, &mut events);
            events.clear();
        }
        print(&slave, "last line\r\n");
        for _ in 0..8 {
            terminals.tick(false, &mut events);
        }
        let read = read(&mut terminals, 5, &mut events);
        assert!(read["text"].as_str().unwrap().ends_with("last line"));
    }

    #[test]
    fn send_reaches_the_program_in_the_modes_it_set() {
        let _no_forks = unix::no_forks();
        let (mut terminals, slave, mut events) = pane(5);
        print(&slave, "\x1b[?1h\x1b[?2004h");
        terminals.tick(false, &mut events);
        terminals.call(
            1,
            5,
            Call::Send {
                text: Some("a b".into()),
                keys: vec!["up".into(), "enter".into()],
            },
            &mut events,
        );
        assert_eq!(result(&mut events, 1).unwrap(), json!({"accepted":true}));
        terminals.tick(false, &mut events);
        let mut input = vec![0; 64];
        let n = fs::File::from(slave.try_clone().unwrap()).read(&mut input).unwrap();
        assert_eq!(&input[..n], b"\x1b[200~a b\x1b[201~\x1bOA\r");
        // One unknown key refuses the whole call.
        terminals.call(
            2,
            5,
            Call::Send {
                text: Some("x".into()),
                keys: vec!["enter".into(), "bogus".into()],
            },
            &mut events,
        );
        assert!(result(&mut events, 2).is_err());
        terminals.tick(false, &mut events);
        assert!(fs::File::from(slave.try_clone().unwrap()).read(&mut input).is_err());
    }

    #[test]
    fn an_exit_settles_waits_from_the_output_read_before_it_is_reported() {
        let _no_forks = unix::no_forks();
        let (mut terminals, slave, mut events) = pane(5);
        terminals.call(1, 5, wait(Condition::Exit, 60_000), &mut events);
        terminals.call(2, 5, wait(Condition::Match("marker".into()), 60_000), &mut events);
        terminals.call(3, 5, wait(Condition::Match("absent".into()), 60_000), &mut events);
        // The exit event comes before the last output has been read.
        print(&slave, "output\r\nmarker\r\n");
        assert!(terminals.pane_exited(5, Some(3), None));
        assert!(terminals.tick(false, &mut events).is_empty(), "the PTY is not at EOF yet");
        assert!(events.iter().all(|e| !matches!(e, Event::Exited { .. })));
        drop(slave);
        // EOF: the output is in, so the waits and the exit are reported.
        let mut cancelled = vec![];
        for _ in 0..3 {
            cancelled.extend(terminals.tick(false, &mut events));
        }
        assert_eq!(cancelled, [5], "everything the program started is cancelled once");
        let exited = result(&mut events, 1).unwrap();
        assert_eq!(exited["status"], "exited");
        assert_eq!(exited["code"], 3);
        assert_eq!(exited["signal"], Value::Null);
        assert_eq!(exited["text"], "output\nmarker");
        assert_eq!(result(&mut events, 2).unwrap()["status"], "matched");
        assert_eq!(result(&mut events, 3).unwrap()["status"], "exited");
        let at = events.iter().position(|e| matches!(e, Event::Exited { id: 5, code: Some(3), signal: None }));
        assert!(at.is_some(), "the exit is reported after the waits");
        assert!(!terminals.has(5), "with nobody attached the terminal is gone at once");
        terminals.call(4, 5, wait(Condition::Exit, 1), &mut events);
        assert_eq!(result(&mut events, 4).unwrap_err(), "no such terminal");
    }

    #[test]
    fn an_exit_report_is_a_wait_from_the_start_that_answers_before_the_exit() {
        let _no_forks = unix::no_forks();
        let mut terminals = Terminals::default();
        let (master, slave) = unix::pty(24, 80).unwrap();
        raw(&slave);
        terminals.pane(5, master, 24, 80, Some(9)).unwrap();
        let mut events = VecDeque::new();
        // The program says something and ends before anyone has asked
        // anything: the report still has its last lines.
        let lines: String = (0..30).map(|n| format!("line {n}\r\n")).collect();
        print(&slave, &lines);
        assert!(terminals.pane_exited(5, None, Some(9)));
        drop(slave);
        for _ in 0..3 {
            terminals.tick(false, &mut events);
        }
        let report = result(&mut events, 9).unwrap();
        assert_eq!(report["status"], "exited");
        assert_eq!((report["code"].clone(), report["signal"].clone()), (Value::Null, json!(9)));
        let text = report["text"].as_str().unwrap();
        assert_eq!(text.lines().count(), 20);
        assert!(text.starts_with("line 10\n") && text.ends_with("line 29"), "{text}");
        assert!(events.iter().any(|e| matches!(e, Event::Exited { id: 5, .. })));
    }

    /// What the program reads from its input, if anything.
    fn input_of(slave: &std::os::fd::OwnedFd) -> Option<Vec<u8>> {
        let mut input = vec![0; 128];
        match fs::File::from(slave.try_clone().unwrap()).read(&mut input) {
            Ok(n) => Some(input[..n].to_vec()),
            Err(_) => None,
        }
    }

    #[test]
    fn a_program_nobody_is_attached_to_gets_its_basic_queries_answered() {
        let _no_forks = unix::no_forks();
        let (mut terminals, slave, mut events) = pane(5);
        // A shell at startup: its device attributes, then where it is.
        print(&slave, "hello\x1b[c\x1b[6n");
        terminals.tick(false, &mut events);
        assert_eq!(input_of(&slave).unwrap(), b"\x1b[?62;22c\x1b[1;6R");
        // Ghostty also answers secondary device attributes and private CPR.
        print(&slave, "\x1b[>c\x1b[?6n\x1b]10;?\x07");
        terminals.tick(false, &mut events);
        assert_eq!(input_of(&slave).unwrap(), b"\x1b[>1;0;0c\x1b[?1;6;1R");
        // An attached frontend answers for itself: the query reaches it.
        let id = TerminalId::Process(5);
        let (server, mut client) = UnixStream::pair().unwrap();
        terminals.attach(id, server, 24, 80, Replay::Vt, &mut events);
        terminals.tick(false, &mut events);
        let _ = frontend(&mut client, 24, 80);
        print(&slave, "\x1b[c");
        terminals.tick(false, &mut events);
        let mut query = [0; 3];
        client.read_exact(&mut query).unwrap();
        assert_eq!(&query, b"\x1b[c");
        assert_eq!(input_of(&slave), None, "the supervisor stays out of it");
        // Detached again, it answers again.
        terminals.detach(id, &mut events);
        drop(client);
        terminals.tick(false, &mut events);
        print(&slave, "\x1b[5n");
        terminals.tick(false, &mut events);
        assert_eq!(input_of(&slave).unwrap(), b"\x1b[0n");
    }

    #[test]
    fn a_program_that_never_reads_cannot_make_the_supervisor_queue_without_bound() {
        let _no_forks = unix::no_forks();
        let (mut terminals, slave, mut events) = pane(5);
        let flood = "\x1b[c".repeat(600);
        for _ in 0..400 {
            print(&slave, &flood);
            terminals.tick(false, &mut events);
        }
        let queued = terminals.entries[&TerminalId::Process(5)].io.queued_input();
        assert!(queued <= 65536, "{queued}");
    }

    #[test]
    fn a_descendant_holding_the_pty_delays_the_exit_by_one_second() {
        let _no_forks = unix::no_forks();
        let (mut terminals, slave, mut events) = pane(5);
        terminals.call(1, 5, wait(Condition::Exit, 60_000), &mut events);
        print(&slave, "late\r\n");
        terminals.pane_exited(5, None, Some(9));
        terminals.tick(false, &mut events);
        assert!(events.is_empty());
        std::thread::sleep(DRAIN + Duration::from_millis(100));
        terminals.tick(false, &mut events);
        let exited = result(&mut events, 1).unwrap();
        assert_eq!(exited["signal"], 9);
        assert_eq!(exited["text"], "late");
        drop(slave);
    }

    #[test]
    fn a_stopping_goblin_settles_every_terminal() {
        let _no_forks = unix::no_forks();
        let (mut terminals, slave, mut events) = pane(5);
        terminals.call(1, 5, wait(Condition::Exit, 60_000), &mut events);
        terminals.tick(true, &mut events);
        assert_eq!(result(&mut events, 1).unwrap()["status"], "exited");
        assert!(events.iter().any(|e| matches!(e, Event::Exited { id: 5, code: None, signal: None })));
        assert!(terminals.drained());
        drop(slave);
    }

    #[test]
    fn results_wait_for_event_capacity() {
        let _no_forks = unix::no_forks();
        let (mut terminals, slave, mut events) = pane(5);
        for request in 1..=3 {
            terminals.call(request, 5, wait(Condition::Match("go".into()), 60_000), &mut events);
        }
        print(&slave, "go\r\n");
        while events.len() < crate::engine::EVENTS - 1 {
            events.push_back(Event::PayloadExited);
        }
        terminals.tick(false, &mut events);
        assert_eq!(events.len(), crate::engine::EVENTS, "one answer fits");
        events.clear();
        terminals.tick(false, &mut events);
        terminals.tick(false, &mut events);
        let answered = events
            .iter()
            .filter(|e| matches!(e, Event::TerminalResult { .. }))
            .count();
        assert_eq!(answered, 2, "the others were not lost");
    }

    /// Read a frontend's stream as a screen, after its handshake frame.
    fn frontend(stream: &mut UnixStream, rows: u16, cols: u16) -> crate::screen::Screen {
        stream.set_read_timeout(Some(Duration::from_millis(200))).unwrap();
        assert_eq!(goblins_jsonrpc::read(stream).unwrap()["result"]["attached"], true);
        let mut bytes = vec![];
        let _ = stream.read_to_end(&mut bytes);
        let mut screen = crate::screen::Screen::new(rows, cols);
        screen.feed(&bytes);
        screen
    }

    #[test]
    fn a_frontend_gets_the_screen_and_detaching_keeps_the_terminal() {
        let _no_forks = unix::no_forks();
        let (mut terminals, slave, mut events) = pane(5);
        let output: String = (0..40).map(|i| format!("line {i}\r\n")).collect();
        print(&slave, &output);
        print(&slave, "\x1b[?2004h");
        terminals.tick(false, &mut events);
        let id = TerminalId::Process(5);
        let (server, mut client) = UnixStream::pair().unwrap();
        terminals.attach(id, server, 24, 80, Replay::Vt, &mut events);
        terminals.tick(false, &mut events);
        let mut screen = frontend(&mut client, 24, 80);
        assert_eq!(screen.recent(100).last().unwrap(), "line 39");
        assert_eq!(screen.recent(100)[0], "line 0");
        // Output while attached reaches the frontend and the screen.
        print(&slave, "live\r\n");
        terminals.tick(false, &mut events);
        let mut live = [0; 6];
        client.read_exact(&mut live).unwrap();
        assert_eq!(&live, b"live\r\n");
        // A second frontend is refused; detaching ends the stream only.
        let (second, mut refused) = UnixStream::pair().unwrap();
        terminals.attach(id, second, 24, 80, Replay::Vt, &mut events);
        assert_eq!(
            goblins_jsonrpc::read(&mut refused).unwrap()["error"]["message"],
            "terminal is already attached"
        );
        terminals.detach(id, &mut events);
        assert_eq!(client.read(&mut [0; 1]).unwrap(), 0);
        drop(client);
        terminals.tick(false, &mut events);
        assert!(terminals.has(5));
        assert_eq!(terminals.status(id), TerminalStatus::default());
        // Output while nobody is attached is on the screen of the next one.
        print(&slave, "while away\r\n");
        terminals.tick(false, &mut events);
        let (server, mut client) = UnixStream::pair().unwrap();
        terminals.attach(id, server, 24, 80, Replay::Vt, &mut events);
        terminals.tick(false, &mut events);
        let mut screen = frontend(&mut client, 24, 80);
        assert_eq!(screen.recent(100).last().unwrap(), "while away");
        assert!(screen.input(Some("p"), &[]).starts_with(b"\x1b[200~"), "input modes come back");
    }

    #[test]
    fn the_frontend_of_an_exited_terminal_reads_its_queue_before_the_stream_closes() {
        let _no_forks = unix::no_forks();
        let (mut terminals, slave, mut events) = pane(5);
        let id = TerminalId::Process(5);
        let (server, mut client) = UnixStream::pair().unwrap();
        terminals.attach(id, server, 24, 80, Replay::Vt, &mut events);
        terminals.tick(false, &mut events);
        print(&slave, "goodbye\r\n");
        drop(slave);
        terminals.pane_exited(5, Some(0), None);
        for _ in 0..4 {
            terminals.tick(false, &mut events);
        }
        assert!(events.iter().any(|e| matches!(e, Event::Exited { id: 5, .. })));
        assert!(terminals.has(5), "the frontend still holds its stream");
        let screen = frontend(&mut client, 24, 80);
        assert!(screen.visible().contains(&"goodbye".to_string()));
        drop(client);
        terminals.tick(false, &mut events);
        assert!(!terminals.has(5));
        assert!(terminals.drained());
    }

    #[test]
    fn a_frontend_that_does_not_read_is_cut_off_after_one_more_second() {
        let _no_forks = unix::no_forks();
        let (mut terminals, slave, mut events) = pane(5);
        let id = TerminalId::Process(5);
        let (server, client) = UnixStream::pair().unwrap();
        terminals.attach(id, server, 24, 80, Replay::Vt, &mut events);
        terminals.tick(false, &mut events);
        // More than a socket buffer holds, which nobody reads.
        let line = "y".repeat(79) + "\r\n";
        let mut slave_file = fs::File::from(slave.try_clone().unwrap());
        for _ in 0..4000 {
            let _ = slave_file.write(line.as_bytes());
            terminals.tick(false, &mut events);
        }
        terminals.pane_exited(5, Some(0), None);
        drop(slave_file);
        drop(slave);
        for _ in 0..4 {
            terminals.tick(false, &mut events);
        }
        assert!(terminals.has(5));
        // The queue is full, so the exit waits out its second of reading.
        std::thread::sleep(DRAIN + Duration::from_millis(100));
        terminals.tick(false, &mut events);
        assert!(events.iter().any(|e| matches!(e, Event::Exited { id: 5, .. })));
        assert!(terminals.has(5), "the frontend gets one more second");
        std::thread::sleep(DRAIN + Duration::from_millis(100));
        terminals.tick(false, &mut events);
        terminals.tick(false, &mut events);
        assert!(!terminals.has(5), "closed even though it was not read");
        drop(client);
    }

    #[test]
    fn attaching_to_a_cleared_screen_at_a_smaller_size_keeps_the_prompt_visible() {
        let _no_forks = unix::no_forks();
        let mut terminals = Terminals::default();
        let (master, slave) = unix::pty(40, 120).unwrap();
        raw(&slave);
        terminals.pane(5, master, 40, 120, None).unwrap();
        let mut events = VecDeque::new();
        for i in 0..45 {
            print(&slave, &format!("line {i}\r\n"));
        }
        print(&slave, "\x1b[H\x1b[2Jprompt> ");
        terminals.tick(false, &mut events);

        let (server, mut client) = UnixStream::pair().unwrap();
        terminals.attach(
            TerminalId::Process(5),
            server,
            24,
            100,
            Replay::Vt,
            &mut events,
        );
        terminals.tick(false, &mut events);

        let mut screen = frontend(&mut client, 24, 100);
        assert_eq!(screen.visible(), ["prompt>"]);
        assert_eq!(screen.recent(100).first().unwrap(), "line 0");
        screen.feed(b"typed");
        assert_eq!(screen.visible(), ["prompt> typed"]);
    }

    #[test]
    fn attaching_from_a_smaller_frontend_keeps_the_newest_output_readable() {
        let _no_forks = unix::no_forks();
        let mut terminals = Terminals::default();
        let (master, slave) = unix::pty(40, 120).unwrap();
        raw(&slave);
        terminals.pane(5, master, 40, 120, None).unwrap();
        let mut events = VecDeque::new();
        let output: String = (0..39).map(|i| format!("line {i}\r\n")).collect();
        print(&slave, &output);
        print(&slave, "READY");
        terminals.tick(false, &mut events);
        let id = TerminalId::Process(5);
        let (server, mut client) = UnixStream::pair().unwrap();
        terminals.attach(id, server, 24, 100, Replay::Vt, &mut events);
        terminals.tick(false, &mut events);
        let _ = frontend(&mut client, 24, 100);
        // The screen was cut to the frontend's size without losing READY.
        terminals.call(1, 5, Call::Read { source: terminal::Source::Visible, lines: None }, &mut events);
        let visible = result(&mut events, 1).unwrap();
        assert_eq!((visible["rows"].as_u64(), visible["cols"].as_u64()), (Some(24), Some(100)));
        assert!(visible["text"].as_str().unwrap().ends_with("READY"));
        terminals.call(2, 5, wait(Condition::Match("READY".into()), 5000), &mut events);
        assert_eq!(result(&mut events, 2).unwrap()["line"], "READY");
        // A later resize does the same.
        print(&slave, "\r\nmore\r\nlast");
        terminals.tick(false, &mut events);
        terminals.resize(id, 10, 100, &mut events);
        terminals.call(3, 5, Call::Read { source: terminal::Source::Recent, lines: Some(3) }, &mut events);
        assert_eq!(result(&mut events, 3).unwrap()["text"], "READY\nmore\nlast");
        terminals.call(4, 5, Call::Read { source: terminal::Source::Visible, lines: None }, &mut events);
        assert!(result(&mut events, 4).unwrap()["text"].as_str().unwrap().ends_with("last"));
    }

    /// The main terminal of a goblin whose PTY opens at `rows` by `cols`,
    /// started on that PTY.
    fn main(
        directory: &Path,
        detached: bool,
        rows: u16,
        cols: u16,
    ) -> (Terminals, std::os::fd::OwnedFd, VecDeque<Event>) {
        let mut terminals = Terminals::new(directory, detached, (rows, cols)).unwrap();
        let (master, slave) = unix::pty(rows, cols).unwrap();
        raw(&slave);
        terminals.started(master, false).unwrap();
        (terminals, slave, VecDeque::new())
    }
    fn main_screen(terminals: &mut Terminals) -> &mut crate::screen::Screen {
        terminals
            .entries
            .get_mut(&TerminalId::Main)
            .unwrap()
            .io
            .screen()
            .unwrap()
    }

    #[test]
    fn a_main_terminal_launched_detached_reads_at_its_launch_size() {
        let _no_forks = unix::no_forks();
        let directory = unix::temp_directory().unwrap();
        let (mut terminals, slave, mut events) = main(&directory, true, 30, 100);
        print(&slave, &format!("{}next", "x".repeat(100)));
        terminals.tick(false, &mut events);
        let screen = main_screen(&mut terminals);
        assert_eq!(screen.size(), (30, 100));
        assert_eq!(screen.visible(), ["x".repeat(100), "next".into()]);
        drop(terminals);
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn a_resize_before_the_main_pty_exists_sizes_its_screen() {
        let _no_forks = unix::no_forks();
        let directory = unix::temp_directory().unwrap();
        let mut terminals = Terminals::new(&directory, true, (30, 100)).unwrap();
        let mut events = VecDeque::new();
        terminals.resize(TerminalId::Main, 40, 120, &mut events);
        assert!(events.is_empty());
        let (master, slave) = unix::pty(30, 100).unwrap();
        terminals.started(master, false).unwrap();
        assert_eq!(main_screen(&mut terminals).size(), (40, 120));
        let mut size: libc::winsize = unsafe { std::mem::zeroed() };
        unix::cvt(unsafe { libc::ioctl(slave.as_raw_fd(), libc::TIOCGWINSZ, &mut size) }).unwrap();
        assert_eq!((size.ws_row, size.ws_col), (40, 120));
        drop(terminals);
        fs::remove_dir_all(directory).unwrap();
    }

    /// Attach a frontend to the main terminal through the runtime, at 24 by
    /// 80, and read its screen.
    fn attach_main(
        terminals: &mut Terminals,
        events: &mut VecDeque<Event>,
    ) -> (UnixStream, crate::screen::Screen) {
        let (server, mut client) = UnixStream::pair().unwrap();
        terminals.attach(TerminalId::Main, server, 24, 80, Replay::Vt, events);
        terminals.tick(false, events);
        let screen = frontend(&mut client, 24, 80);
        (client, screen)
    }
    /// The payload has exited: the goblin has ended, and its terminals are
    /// ticked until the runtime may end.
    fn end(terminals: &mut Terminals, slave: std::os::fd::OwnedFd, events: &mut VecDeque<Event>) {
        drop(slave);
        for _ in 0..4 {
            terminals.tick(true, events);
        }
        assert!(terminals.drained(), "the main entry is removed");
        assert!(!terminals.entries.contains_key(&TerminalId::Main));
    }

    #[test]
    fn the_main_terminal_is_removed_after_a_clean_detach_when_its_payload_exits() {
        let _no_forks = unix::no_forks();
        let directory = unix::temp_directory().unwrap();
        let (mut terminals, slave, mut events) = main(&directory, false, 24, 80);
        print(&slave, "working\r\n");
        terminals.tick(false, &mut events);
        let (mut client, screen) = attach_main(&mut terminals, &mut events);
        assert_eq!(screen.visible(), ["working"]);
        terminals.detach(TerminalId::Main, &mut events);
        assert_eq!(client.read(&mut [0; 1]).unwrap(), 0);
        drop(client);
        terminals.tick(false, &mut events);
        let status = terminals.status(TerminalId::Main);
        assert!(status.detached && !status.connected && !status.interrupted);
        print(&slave, "done\r\n");
        terminals.tick(false, &mut events);
        assert!(!terminals.drained());
        end(&mut terminals, slave, &mut events);
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn the_main_terminal_is_removed_after_its_frontend_vanishes_when_its_payload_exits() {
        let _no_forks = unix::no_forks();
        let directory = unix::temp_directory().unwrap();
        let (mut terminals, slave, mut events) = main(&directory, false, 24, 80);
        let (client, _) = attach_main(&mut terminals, &mut events);
        client.shutdown(std::net::Shutdown::Both).unwrap();
        terminals.tick(false, &mut events);
        let status = terminals.status(TerminalId::Main);
        assert!(status.interrupted && !status.connected && !status.detached);
        assert!(!terminals.drained());
        end(&mut terminals, slave, &mut events);
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn the_main_terminal_answers_queries_only_while_nobody_is_attached() {
        let _no_forks = unix::no_forks();
        let directory = unix::temp_directory().unwrap();
        let (mut terminals, slave, mut events) = main(&directory, true, 24, 80);
        print(&slave, "\x1b[c");
        terminals.tick(false, &mut events);
        assert_eq!(input_of(&slave).unwrap(), b"\x1b[?62;22c");
        let (mut client, _) = attach_main(&mut terminals, &mut events);
        print(&slave, "\x1b[c");
        terminals.tick(false, &mut events);
        let mut query = [0; 3];
        client.read_exact(&mut query).unwrap();
        assert_eq!(&query, b"\x1b[c");
        assert_eq!(input_of(&slave), None, "the frontend answers");
        drop(terminals);
        fs::remove_dir_all(directory).unwrap();
    }

    /// Attach a frontend with the snapshot replay; returns its stream and
    /// the snapshot after the handshake frame and the length.
    fn attach_snapshot(
        terminals: &mut Terminals,
        id: TerminalId,
        (rows, cols): (u16, u16),
        events: &mut VecDeque<Event>,
    ) -> (UnixStream, Vec<u8>) {
        let (server, mut client) = UnixStream::pair().unwrap();
        terminals.attach(id, server, rows, cols, Replay::Snapshot, events);
        terminals.tick(false, events);
        client
            .set_read_timeout(Some(Duration::from_millis(200)))
            .unwrap();
        assert_eq!(
            goblins_jsonrpc::read(&mut client).unwrap()["result"]["attached"],
            true
        );
        let mut bytes = live(terminals, &mut client, events);
        let length = u32::from_be_bytes(bytes[..4].try_into().unwrap()) as usize;
        assert_eq!(bytes.len(), 4 + length, "nothing follows the snapshot yet");
        (client, bytes.split_off(4))
    }
    /// What reaches the frontend while the terminals are ticked, until
    /// nothing more arrives.
    fn live(
        terminals: &mut Terminals,
        client: &mut UnixStream,
        events: &mut VecDeque<Event>,
    ) -> Vec<u8> {
        client
            .set_read_timeout(Some(Duration::from_millis(100)))
            .unwrap();
        let mut bytes = vec![];
        let mut buf = [0; 65536];
        loop {
            terminals.tick(false, events);
            match client.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => bytes.extend_from_slice(&buf[..n]),
                Err(e)
                    if matches!(
                        e.kind(),
                        std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                    ) =>
                {
                    break;
                }
                Err(e) => panic!("read the frontend's stream: {e}"),
            }
        }
        bytes
    }
    fn screen_of(terminals: &mut Terminals, id: TerminalId) -> &mut crate::screen::Screen {
        terminals.entries.get_mut(&id).unwrap().io.screen().unwrap()
    }

    #[test]
    fn a_snapshot_attachment_restores_a_pane_then_follows_its_output() {
        let _no_forks = unix::no_forks();
        let (mut terminals, slave, mut events) = pane(5);
        let id = TerminalId::Process(5);
        let output: String = (0..40).map(|i| format!("line {i}\r\n")).collect();
        print(&slave, &output);
        // Modes, and a sequence still open when the snapshot is taken.
        print(
            &slave,
            "\x1b[?2004h\x1b[?1h\x1b]2;building\x1b\\prompt> \x1b[3",
        );
        terminals.tick(false, &mut events);
        let before = screen_of(&mut terminals, id).state();
        // The snapshot keeps the size the pane had; the frontend resizes
        // after restoring it, as the pane does.
        let (mut client, snapshot) = attach_snapshot(&mut terminals, id, (30, 100), &mut events);
        assert!(snapshot.starts_with(b"GHOSTSNP"));
        let mut restored = crate::screen::Screen::restore(&snapshot);
        assert_eq!(restored.state(), before);
        restored.resize(30, 100);
        assert_eq!(restored.state(), screen_of(&mut terminals, id).state());
        // The open sequence ends in the live output: the restored screen
        // parses it as the pane did.
        print(&slave, "1mbold\r\nlive\r\n");
        let bytes = live(&mut terminals, &mut client, &mut events);
        assert!(bytes.ends_with(b"live\r\n"));
        restored.feed(&bytes);
        let after = screen_of(&mut terminals, id).state();
        assert_eq!(after.0[after.0.len() - 2..], ["prompt> bold", "live"]);
        assert_eq!(restored.state(), after);
    }

    #[test]
    fn a_snapshot_attachment_restores_the_main_terminal_on_its_alternate_screen() {
        let _no_forks = unix::no_forks();
        let directory = unix::temp_directory().unwrap();
        let (mut terminals, slave, mut events) = main(&directory, true, 24, 80);
        print(
            &slave,
            "shell history\r\n\x1b[?1049h\x1b[?1h\x1b[5;10Hfull screen",
        );
        terminals.tick(false, &mut events);
        let before = screen_of(&mut terminals, TerminalId::Main).state();
        assert!(before.3, "on the alternate screen");
        let (mut client, snapshot) =
            attach_snapshot(&mut terminals, TerminalId::Main, (24, 80), &mut events);
        let mut restored = crate::screen::Screen::restore(&snapshot);
        assert_eq!(restored.state(), before);
        print(&slave, "\x1b[?1049lback\r\n");
        restored.feed(&live(&mut terminals, &mut client, &mut events));
        let after = screen_of(&mut terminals, TerminalId::Main).state();
        assert_eq!(after.0, ["shell history", "back"]);
        assert_eq!(restored.state(), after);
        drop(terminals);
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn a_snapshot_attachment_is_refused_during_a_sequence_too_long_to_encode() {
        let _no_forks = unix::no_forks();
        let (mut terminals, slave, mut events) = pane(5);
        let id = TerminalId::Process(5);
        print(&slave, "before\r\n");
        // A clipboard escape longer than the continuation limit, unfinished.
        let mut long = b"\x1b]52;c;".to_vec();
        long.extend(std::iter::repeat_n(b'A', 70_000));
        let mut writer = fs::File::from(slave.try_clone().unwrap());
        let mut written = 0;
        while written < long.len() {
            match writer.write(&long[written..]) {
                Ok(n) => written += n,
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => (),
                Err(e) => panic!("write the sequence: {e}"),
            }
            terminals.tick(false, &mut events);
        }
        for _ in 0..8 {
            terminals.tick(false, &mut events);
        }
        let (server, mut client) = UnixStream::pair().unwrap();
        terminals.attach(id, server, 30, 100, Replay::Snapshot, &mut events);
        client.set_read_timeout(Some(Duration::from_millis(200))).unwrap();
        let refusal = goblins_jsonrpc::read(&mut client).unwrap();
        assert_eq!(refusal["error"]["code"], -32009);
        assert!(
            refusal["error"]["message"].as_str().unwrap().contains("long escape sequence"),
            "{refusal}"
        );
        // Nothing was accepted: the size and the slot are as they were.
        assert_eq!(screen_of(&mut terminals, id).size(), (24, 80));
        assert!(!terminals.status(id).connected);
        // Once the program ends the sequence, the next attachment gets it.
        print(&slave, "\x07after\r\n");
        terminals.tick(false, &mut events);
        let (_client, snapshot) = attach_snapshot(&mut terminals, id, (24, 80), &mut events);
        let mut restored = crate::screen::Screen::restore(&snapshot);
        assert_eq!(restored.state(), screen_of(&mut terminals, id).state());
        assert_eq!(restored.visible(), ["before", "after"]);
    }

    #[test]
    fn a_snapshot_attachment_to_a_terminal_without_a_screen_has_length_zero() {
        let _no_forks = unix::no_forks();
        let mut terminals = Terminals::default();
        let (master, slave) = unix::pty(24, 80).unwrap();
        raw(&slave);
        terminals.process(9, master, 24, 80).unwrap();
        print(&slave, "early");
        let mut events = VecDeque::new();
        terminals.tick(false, &mut events);
        let (server, mut client) = UnixStream::pair().unwrap();
        terminals.attach(
            TerminalId::Process(9),
            server,
            24,
            80,
            Replay::Snapshot,
            &mut events,
        );
        terminals.tick(false, &mut events);
        client
            .set_read_timeout(Some(Duration::from_millis(200)))
            .unwrap();
        assert_eq!(
            goblins_jsonrpc::read(&mut client).unwrap()["result"]["attached"],
            true
        );
        print(&slave, " late");
        assert_eq!(
            live(&mut terminals, &mut client, &mut events),
            b"\0\0\0\0early late"
        );
    }
}
