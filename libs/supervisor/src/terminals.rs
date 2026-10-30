//! Terminal I/O stays with the runtime after the sandbox stops, until its
//! retained output and frontend handoff no longer need it.
use crate::{server_log::ServerLog, terminal::Terminal};
use goblins_runtime::{Event, TerminalId, TerminalStatus};
use std::{
    collections::{BTreeMap, VecDeque},
    io::Write,
    os::{fd::OwnedFd, unix::net::UnixStream},
    path::{Path, PathBuf},
};

pub(crate) const UPGRADE: &str =
    "this goblin starts a client at each attach; upgrade Goblins to attach";
struct Entry {
    io: Terminal,
    reported: TerminalStatus,
    exposed: bool,
    exited: bool,
}
#[derive(Default)]
pub(crate) struct Terminals {
    entries: BTreeMap<TerminalId, Entry>,
    log: Option<ServerLog>,
    log_path: Option<PathBuf>,
}
impl Terminals {
    pub fn new(directory: &Path, detached: bool) -> crate::Result<Self> {
        let mut io = Terminal::new(&directory.join("terminal.sock"))?;
        io.detached = detached;
        io.hold();
        let mut this = Self {
            log_path: Some(directory.join("server.log")),
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
            },
        );
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
            }
        }
    }
    pub fn attach(
        &mut self,
        id: TerminalId,
        mut stream: UnixStream,
        rows: u16,
        cols: u16,
        events: &mut VecDeque<Event>,
    ) {
        let mut diagnostic = None;
        let result: crate::Result<()> = match self.entries.get_mut(&id) {
            Some(entry) => {
                entry.exposed = true;
                stream.try_clone().map_err(Into::into).and_then(|peer| {
                    entry.io.attach(peer, serde_json::json!(0))?;
                    // Only an accepted consumer may change the size. An
                    // ioctl error does not turn an accepted stream into a
                    // second, contradictory refusal frame.
                    diagnostic = entry.io.resize(rows, cols).err().map(|e| e.to_string());
                    Ok(())
                })
            }
            None => Err("terminal already completed".into()),
        };
        if let Err(error) = result {
            // The daemon has already reserved the slot; report the actual
            // state even when the terminal disappeared before Attach arrived.
            let _ = stream.set_nonblocking(true);
            if let Ok(bytes) = goblins_jsonrpc::encode(&goblins_jsonrpc::error(
                serde_json::json!(0),
                -32009,
                &error.to_string(),
            )) {
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
    pub fn tick(&mut self, ended: bool, events: &mut VecDeque<Event>) {
        if let Some(log) = &mut self.log {
            log.tick();
        }
        self.entries.retain(|id, entry| {
            let error = entry.io.tick().err().map(|e| e.to_string());
            if error.is_some() {
                entry.io.interrupted = true;
            }
            let mut status = entry.io.status();
            let remove = (ended && (!entry.exposed || entry.io.can_evict()))
                || (matches!(id, TerminalId::Process(_))
                    && entry.exited
                    && !status.connected
                    && entry.io.can_evict());
            if remove {
                status.connected = false;
                status.attached = false;
            }
            if entry.exposed && (status != entry.reported || error.is_some()) {
                events.push_back(Event::Terminal {
                    terminal: *id,
                    status,
                    error,
                });
                entry.reported = status;
            }
            !remove
        });
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
        let mut terminals = Terminals::new(&directory, false).unwrap();
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
        let mut terminals = Terminals::new(&directory, false).unwrap();
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
        assert_eq!(output, b"retained\0payload");
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
        terminals.attach(id, server, 30, 100, &mut events);
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
        terminals.attach(id, second, 20, 80, &mut events);
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
        terminals.attach(id, next, 20, 80, &mut events);
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
        terminals.attach(id, missing, 20, 80, &mut events);
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
        let mut terminals = Terminals::new(&directory, false).unwrap();
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
}
