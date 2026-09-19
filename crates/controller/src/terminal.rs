//! Daemon-owned PTY with one terminal consumer at a time.
//! A framed acceptance precedes raw bytes; detach half-closes that stream.
use crate::{Result, unix};
use std::{
    fs::{self, File},
    io::{self, Read, Write},
    net::Shutdown,
    os::{
        fd::{AsRawFd, OwnedFd},
        unix::{
            fs::PermissionsExt,
            net::{UnixListener, UnixStream},
        },
    },
    path::{Path, PathBuf},
};
const CAP: usize = 65536;
pub struct Terminal {
    path: PathBuf,
    listener: UnixListener,
    peer: Option<UnixStream>,
    master: Option<File>,
    connected: bool,
    dimensions: Option<(u16, u16)>,
    pub detached: bool,
    pub eof: bool,
    pub interrupted: bool,
    pub complete: bool,
    to_peer: Vec<u8>,
    to_pty: Vec<u8>,
}
fn read(source: &mut impl Read, dest: &mut Vec<u8>) -> io::Result<bool> {
    if dest.len() == CAP {
        return Ok(false);
    }
    let mut bytes = [0; 8192];
    let room = bytes.len().min(CAP - dest.len());
    match source.read(&mut bytes[..room]) {
        Ok(0) => Ok(true),
        Ok(n) => {
            dest.extend_from_slice(&bytes[..n]);
            Ok(false)
        }
        Err(e)
            if matches!(
                e.kind(),
                io::ErrorKind::WouldBlock | io::ErrorKind::Interrupted
            ) =>
        {
            Ok(false)
        }
        Err(e) => Err(e),
    }
}
fn write(dest: &mut impl Write, bytes: &mut Vec<u8>) -> io::Result<()> {
    if bytes.is_empty() {
        return Ok(());
    }
    match dest.write(bytes) {
        Ok(0) => Err(io::Error::new(
            io::ErrorKind::WriteZero,
            "terminal write failed",
        )),
        Ok(n) => {
            bytes.drain(..n);
            Ok(())
        }
        Err(e)
            if matches!(
                e.kind(),
                io::ErrorKind::WouldBlock | io::ErrorKind::Interrupted
            ) =>
        {
            Ok(())
        }
        Err(e) => Err(e),
    }
}
impl Terminal {
    pub fn new(path: &Path) -> Result<Self> {
        let listener = UnixListener::bind(path)?;
        listener.set_nonblocking(true)?;
        fs::set_permissions(path, fs::Permissions::from_mode(0o600))?;
        Ok(Self {
            path: path.into(),
            listener,
            peer: None,
            master: None,
            connected: false,
            dimensions: None,
            detached: false,
            eof: false,
            interrupted: false,
            complete: false,
            to_peer: vec![],
            to_pty: vec![],
        })
    }
    pub fn master(&mut self, fd: OwnedFd) -> Result<()> {
        unix::nonblocking(fd.as_raw_fd())?;
        self.master = Some(File::from(fd));
        if let Some((rows, cols)) = self.dimensions {
            self.resize(rows, cols)?;
        }
        Ok(())
    }
    pub fn resize(&mut self, rows: u16, cols: u16) -> Result<()> {
        self.dimensions = Some((rows, cols));
        let Some(master) = self.master.as_ref() else {
            return Ok(());
        };
        let size = libc::winsize {
            ws_row: rows,
            ws_col: cols,
            ws_xpixel: 0,
            ws_ypixel: 0,
        };
        unix::cvt(unsafe { libc::ioctl(master.as_raw_fd(), libc::TIOCSWINSZ, &size) })?;
        Ok(())
    }
    pub fn failed_start(&mut self) {
        self.eof = true;
        self.interrupted = true;
    }
    pub fn attached(&self) -> bool {
        self.peer.is_some() && !self.detached
    }
    pub fn can_evict(&self) -> bool {
        self.complete || self.interrupted || (self.detached && self.peer.is_none())
    }
    pub fn detach(&mut self) -> Result<()> {
        let peer = self.peer.as_ref().ok_or("terminal is not attached")?;
        if self.detached || self.complete {
            return Err("terminal is not attached".into());
        }
        // Retain the read half until the client has observed EOF and queried
        // its outcome. Reject other consumers during this short handoff.
        peer.shutdown(Shutdown::Write)?;
        self.detached = true;
        self.to_pty.clear();
        Ok(())
    }
    pub fn tick(&mut self) -> Result<()> {
        if let Some(peer) = &mut self.peer {
            // Even after PTY EOF, continue draining output. Peer input EOF is a
            // consumer disconnect, not payload exit or session cancellation.
            if self.detached {
                self.to_pty.clear();
            }
            // Detect a closed consumer even when queued input is full and the
            // payload is not reading. Otherwise backpressure could hold the
            // attachment slot forever after the client exits.
            let mut state = libc::pollfd {
                fd: peer.as_raw_fd(),
                events: libc::POLLRDHUP,
                revents: 0,
            };
            unsafe {
                libc::poll(&mut state, 1, 0);
            }
            if state.revents & (libc::POLLRDHUP | libc::POLLHUP | libc::POLLERR) != 0
                || read(peer, &mut self.to_pty).unwrap_or(true)
            {
                self.peer.take();
                if !self.complete && !self.detached {
                    self.interrupted = true;
                }
                self.to_pty.clear();
            }
            if self.detached {
                self.to_pty.clear();
            }
        }
        for _ in 0..4 {
            match self.listener.accept() {
                Ok((mut peer, _)) => {
                    use goblins_protocol::rpc;
                    use serde_json::json;
                    peer.set_nonblocking(true)?;
                    if self.peer.is_some() || self.complete {
                        let message = if self.complete {
                            "terminal already completed"
                        } else {
                            "terminal is already attached"
                        };
                        let _ =
                            peer.write_all(&rpc::encode(&rpc::error(json!(0), -32009, message))?);
                        continue;
                    }
                    if peer
                        .write_all(&rpc::encode(&rpc::result(
                            json!(0),
                            json!({"attached":true}),
                        ))?)
                        .is_err()
                    {
                        // A client disappearing during acceptance must not
                        // change the payload's delivery state.
                        continue;
                    }
                    self.peer = Some(peer);
                    self.connected = true;
                    self.detached = false;
                    self.interrupted = false;
                }
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => break,
                Err(e) => return Err(e.into()),
            }
        }
        if let Some(master) = &mut self.master
            && !self.eof
        {
            // Before the first consumer, and while attached, preserve lossless
            // backpressure. Detached payloads keep running: retain only a
            // bounded tail, making room for the next read when necessary.
            let result = if self.connected && (self.peer.is_none() || self.detached) {
                let mut newest = Vec::new();
                let result = read(master, &mut newest);
                let overflow = (self.to_peer.len() + newest.len()).saturating_sub(CAP);
                self.to_peer.drain(..overflow);
                self.to_peer.extend(newest);
                result
            } else {
                read(master, &mut self.to_peer)
            };
            match result {
                Ok(eof) => self.eof = eof,
                Err(e) if e.raw_os_error() == Some(libc::EIO) => self.eof = true,
                Err(_) => {
                    self.eof = true;
                    self.interrupted = true;
                }
            }
            if !self.eof && write(master, &mut self.to_pty).is_err() {
                self.to_pty.clear();
            }
        }
        if let Some(peer) = &mut self.peer
            && !self.detached
        {
            if write(peer, &mut self.to_peer).is_err() {
                self.interrupted = true;
                self.peer.take();
                self.to_pty.clear();
            } else if self.eof && self.to_peer.is_empty() && !self.complete {
                peer.shutdown(Shutdown::Write)?;
                self.complete = true;
            }
        }
        if self.eof {
            self.master.take();
        }
        Ok(())
    }
}
impl Drop for Terminal {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.path);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use goblins_protocol::rpc;
    use std::time::Duration;

    struct Fixture {
        terminal: Terminal,
        payload: UnixStream,
        directory: PathBuf,
    }
    impl Fixture {
        fn new() -> Self {
            let directory = unix::temp_directory().unwrap();
            let mut terminal = Terminal::new(&directory.join("terminal.sock")).unwrap();
            let (master, payload) = UnixStream::pair().unwrap();
            terminal.master(master.into()).unwrap();
            Self {
                terminal,
                payload,
                directory,
            }
        }
        fn connect(&mut self) -> UnixStream {
            let mut peer = UnixStream::connect(&self.terminal.path).unwrap();
            peer.set_read_timeout(Some(Duration::from_secs(1))).unwrap();
            self.terminal.tick().unwrap();
            assert_eq!(rpc::read(&mut peer).unwrap()["result"]["attached"], true);
            peer
        }
    }
    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.directory);
        }
    }

    #[test]
    fn detach_preserves_payload_and_outcome_until_client_releases_stream() {
        let mut f = Fixture::new();
        let mut first = f.connect();
        f.payload.write_all(b"before").unwrap();
        f.terminal.tick().unwrap();
        let mut bytes = [0; 6];
        first.read_exact(&mut bytes).unwrap();
        assert_eq!(&bytes, b"before");
        f.terminal.detach().unwrap();
        assert_eq!(first.read(&mut bytes).unwrap(), 0);
        assert!(f.terminal.detached);
        assert!(!f.terminal.interrupted);
        assert!(!f.terminal.complete);

        // A competing client cannot replace the detached client's outcome.
        let mut other = UnixStream::connect(&f.terminal.path).unwrap();
        f.terminal.tick().unwrap();
        assert_eq!(rpc::read(&mut other).unwrap()["error"]["code"], -32009);
        // Input sent by the departing consumer must not reach the payload.
        first.write_all(b"stale input").unwrap();
        f.terminal.tick().unwrap();
        assert!(f.terminal.to_pty.is_empty());
        drop(first);
        f.payload.write_all(b"after!").unwrap();
        let mut second = f.connect();
        second.read_exact(&mut bytes).unwrap();
        assert_eq!(&bytes, b"after!");
        assert!(!f.terminal.detached);
        assert!(f.terminal.attached());
        second.write_all(b"fresh!").unwrap();
        f.terminal.tick().unwrap();
        f.payload.read_exact(&mut bytes).unwrap();
        assert_eq!(&bytes, b"fresh!");
        f.payload.shutdown(Shutdown::Write).unwrap();
        f.terminal.tick().unwrap();
        assert_eq!(second.read(&mut bytes).unwrap(), 0);
        assert!(f.terminal.complete);
    }

    #[test]
    fn detached_output_is_bounded_and_connection_loss_allows_reattachment() {
        let mut f = Fixture::new();
        drop(f.connect());
        f.terminal.tick().unwrap();
        assert!(f.terminal.interrupted);
        assert!(!f.terminal.attached());
        for _ in 0..32 {
            f.payload.write_all(&[b'x'; 8192]).unwrap();
            f.terminal.tick().unwrap();
            assert!(f.terminal.to_peer.len() <= CAP);
        }
        assert_eq!(f.terminal.to_peer.len(), CAP);
        f.terminal.tick().unwrap();
        assert_eq!(f.terminal.to_peer.len(), CAP);
        f.payload.write_all(b"final!").unwrap();
        f.terminal.tick().unwrap();
        let count = f.terminal.to_peer.len();
        let mut second = f.connect();
        let mut bytes = vec![0; count];
        second.read_exact(&mut bytes).unwrap();
        assert!(bytes.ends_with(b"final!"));
        assert!(!f.terminal.interrupted);
    }

    #[test]
    fn closed_consumer_releases_attachment_with_full_input_queue() {
        let mut f = Fixture::new();
        let first = f.connect();
        f.terminal.to_pty.resize(CAP, b'x');
        drop(first);
        let _second = f.connect();
        assert!(f.terminal.to_pty.is_empty());
        assert!(f.terminal.attached());
    }

    #[test]
    fn second_consumer_is_rejected_without_interrupting_first() {
        let mut f = Fixture::new();
        let mut first = f.connect();
        let mut second = UnixStream::connect(&f.terminal.path).unwrap();
        f.terminal.tick().unwrap();
        assert_eq!(rpc::read(&mut second).unwrap()["error"]["code"], -32009);
        f.payload.write_all(b"ok").unwrap();
        f.terminal.tick().unwrap();
        let mut bytes = [0; 2];
        first.read_exact(&mut bytes).unwrap();
        assert_eq!(&bytes, b"ok");
        assert!(!f.terminal.interrupted);
    }
}
