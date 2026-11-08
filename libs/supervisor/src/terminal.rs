//! Runtime-owned PTY with one terminal consumer at a time.
//! A framed acceptance precedes raw bytes; detach half-closes that stream.
use crate::{Result, screen::Screen};
use goblins_unix as unix;
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
    /// The socket frontends attach through; none for an attach client's
    /// terminal, which the runtime hands its attachment.
    listener: Option<(PathBuf, UnixListener)>,
    /// Why the socket refuses every attachment.
    refusal: Option<String>,
    /// Leave connections unanswered until it is known whether the payload
    /// has this terminal.
    held: bool,
    peer: Option<UnixStream>,
    master: Option<File>,
    connected: bool,
    dimensions: Option<(u16, u16)>,
    pub detached: bool,
    pub eof: bool,
    pub interrupted: bool,
    pub complete: bool,
    hello: Vec<u8>,
    to_peer: Vec<u8>,
    to_pty: Vec<u8>,
    /// For a terminal the supervisor emulates: its screen, which is fed all
    /// output and keeps it while nobody is attached.
    screen: Option<Screen>,
    /// The screen was fed since `take_fed`.
    fed: bool,
}
fn read(source: &mut impl Read, dest: &mut Vec<u8>) -> io::Result<bool> {
    if dest.len() >= CAP {
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
    pub fn status(&self) -> goblins_runtime::TerminalStatus {
        goblins_runtime::TerminalStatus {
            eof: self.eof,
            complete: self.complete,
            interrupted: self.interrupted,
            attached: self.attached(),
            detached: self.detached,
            connected: self.connected(),
        }
    }
    pub fn new(path: &Path) -> Result<Self> {
        let listener = UnixListener::bind(path)?;
        Self::from_listener(path, listener)
    }
    pub(crate) fn from_listener(path: &Path, listener: UnixListener) -> Result<Self> {
        if listener.local_addr()?.as_pathname() != Some(path) {
            return Err("legacy terminal listener has the wrong address".into());
        }
        let mut accepting: libc::c_int = 0;
        let mut length = std::mem::size_of_val(&accepting) as libc::socklen_t;
        unix::cvt(unsafe {
            libc::getsockopt(
                listener.as_raw_fd(),
                libc::SOL_SOCKET,
                libc::SO_ACCEPTCONN,
                (&mut accepting as *mut libc::c_int).cast(),
                &mut length,
            )
        })?;
        if accepting != 1 {
            return Err("legacy terminal descriptor is not a listener".into());
        }
        listener.set_nonblocking(true)?;
        fs::set_permissions(path, fs::Permissions::from_mode(0o600))?;
        Ok(Self {
            listener: Some((path.into(), listener)),
            refusal: None,
            held: false,
            peer: None,
            master: None,
            connected: false,
            dimensions: None,
            detached: false,
            eof: false,
            interrupted: false,
            complete: false,
            hello: vec![],
            to_peer: vec![],
            to_pty: vec![],
            screen: None,
            fed: false,
        })
    }
    /// A terminal for one attach client's PTY, sized for its frontend.
    pub fn client(master: OwnedFd, rows: u16, cols: u16) -> Result<Self> {
        let mut terminal = Self {
            listener: None,
            refusal: None,
            held: false,
            peer: None,
            master: None,
            connected: false,
            dimensions: None,
            detached: false,
            eof: false,
            interrupted: false,
            complete: false,
            hello: vec![],
            to_peer: vec![],
            to_pty: vec![],
            screen: None,
            fed: false,
        };
        terminal.resize(rows, cols)?;
        terminal.master(master)?;
        Ok(terminal)
    }
    /// A terminal the supervisor emulates, which nobody needs to be attached
    /// to: it keeps reading, and a frontend that attaches gets the screen.
    pub fn emulated(master: OwnedFd, rows: u16, cols: u16) -> Result<Self> {
        let mut terminal = Self::client(master, rows, cols)?;
        terminal.screen = Some(Screen::new(rows, cols));
        Ok(terminal)
    }
    /// The screen of an emulated terminal.
    pub fn screen(&mut self) -> Option<&mut Screen> {
        self.screen.as_mut()
    }
    /// Count output as new to the screen's watchers again.
    pub fn refeed(&mut self) {
        self.fed = true;
    }
    /// Whether output reached the screen since this was last asked.
    pub fn take_fed(&mut self) -> bool {
        std::mem::take(&mut self.fed)
    }
    /// Queue what brings the attached frontend to the screen, ahead of live
    /// output.
    pub fn redraw(&mut self) {
        if let Some(screen) = &mut self.screen {
            self.to_peer = screen.redraw();
        }
    }
    /// What waits to be written to the program.
    #[cfg(test)]
    pub fn queued_input(&self) -> usize {
        self.to_pty.len()
    }
    /// Write `bytes` to the program as if typed.
    pub fn input(&mut self, bytes: &[u8]) -> Result<()> {
        if self.eof || self.master.is_none() {
            return Err("the terminal's program has ended".into());
        }
        if self.detached && self.peer.is_some() {
            return Err("the terminal is being detached".into());
        }
        if self.to_pty.len() + bytes.len() > 4 * CAP {
            return Err("the terminal's input is full".into());
        }
        self.to_pty.extend_from_slice(bytes);
        Ok(())
    }
    /// Signal the PTY's foreground process group that the window changed,
    /// also when its size did not, so full-screen programs repaint.
    pub fn winch(&self) {
        let Some(master) = &self.master else {
            return;
        };
        let group = unsafe { libc::tcgetpgrp(master.as_raw_fd()) };
        if group > 1 && group != unsafe { libc::getpgrp() } {
            unsafe { libc::kill(-group, libc::SIGWINCH) };
        }
    }
    /// End the frontend's stream now, whatever it has not read.
    pub fn close_stream(&mut self) {
        if let Some(peer) = self.peer.take() {
            let _ = peer.shutdown(Shutdown::Both);
        }
        self.hello.clear();
        self.to_peer.clear();
        self.to_pty.clear();
    }
    /// Answer no connection until released, refused, or the payload starts
    /// on this terminal or fails.
    pub fn hold(&mut self) {
        self.held = true;
    }
    pub fn release(&mut self) {
        self.held = false;
    }
    /// Refuse every later attachment through the socket with `message`.
    pub fn refuse(&mut self, message: &str) {
        self.refusal = Some(message.into());
        self.held = false;
    }
    /// A frontend holds the attachment, also after a detach or completion
    /// until it closes its end.
    pub fn connected(&self) -> bool {
        self.peer.is_some()
    }
    pub fn master(&mut self, fd: OwnedFd) -> Result<()> {
        self.held = false;
        unix::nonblocking(fd.as_raw_fd())?;
        self.master = Some(File::from(fd));
        if let Some((rows, cols)) = self.dimensions {
            self.resize(rows, cols)?;
        }
        Ok(())
    }
    pub fn resize(&mut self, rows: u16, cols: u16) -> Result<()> {
        self.dimensions = Some((rows, cols));
        if let Some(screen) = &mut self.screen {
            screen.resize(rows, cols);
        }
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
        self.held = false;
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
    pub fn attach(&mut self, peer: UnixStream, id: serde_json::Value) -> Result<()> {
        self.refresh_peer();
        if self.peer.is_some() || self.complete {
            return Err(if self.complete {
                "terminal already completed"
            } else {
                "terminal is already attached"
            }
            .into());
        }
        peer.set_nonblocking(true)?;
        self.hello = goblins_jsonrpc::encode(&goblins_jsonrpc::result(
            id,
            serde_json::json!({"attached":true}),
        ))?;
        // Keep framing separate from raw output: an abandoned handshake must
        // never become terminal text when a later client attaches.
        self.peer = Some(peer);
        self.connected = true;
        self.detached = false;
        self.interrupted = false;
        Ok(())
    }
    fn refresh_peer(&mut self) {
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
                self.hello.clear();
                if self.screen.is_some() {
                    // The terminal goes on: the frontend left, or finished
                    // reading what a detach told it.
                    self.detached = false;
                } else if !self.complete && !self.detached {
                    self.interrupted = true;
                }
                self.to_pty.clear();
            }
            if self.detached {
                self.to_pty.clear();
            }
        }
    }
    pub fn tick(&mut self) -> Result<()> {
        self.refresh_peer();
        for _ in 0..4 {
            let Some((_, listener)) = self.listener.as_ref().filter(|_| !self.held) else {
                break;
            };
            match listener.accept() {
                Ok((mut peer, _)) => {
                    use goblins_jsonrpc as rpc;
                    use serde_json::json;
                    let writer = peer.try_clone()?;
                    let attached = match &self.refusal {
                        Some(refusal) => Err(refusal.as_str().into()),
                        None => self.attach(writer, json!(0)),
                    };
                    if let Err(error) = attached {
                        peer.set_nonblocking(true)?;
                        let _ = peer.write_all(&rpc::encode(&rpc::error(
                            json!(0),
                            -32009,
                            &error.to_string(),
                        ))?);
                    }
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
            let mut newest = Vec::new();
            let consumer = self.peer.is_some() && !self.detached;
            let tail = if self.screen.is_some() {
                !consumer
            } else {
                self.detached || (self.connected && self.peer.is_none())
            };
            let result = if tail || self.to_peer.len() < CAP {
                // Preserve backpressure by limiting the read to remaining room.
                let mut buf = [0; 8192];
                let room = if tail {
                    buf.len()
                } else {
                    buf.len().min(CAP - self.to_peer.len())
                };
                match master.read(&mut buf[..room]) {
                    Ok(n) => {
                        newest.extend_from_slice(&buf[..n]);
                        Ok(n == 0)
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
            } else {
                Ok(false)
            };
            if !newest.is_empty() {
                if let Some(screen) = &mut self.screen {
                    screen.feed(&newest);
                    self.fed = true;
                    // With nobody attached nobody would answer a query of
                    // the program (a shell waits for its device
                    // attributes); an attached frontend answers its own.
                    let replies = screen.take_replies();
                    if !consumer && self.to_pty.len() + replies.len() <= CAP {
                        self.to_pty.extend_from_slice(&replies);
                    }
                }
                // An emulated terminal keeps its output on the screen, and
                // queues it only for a frontend that is reading.
                if self.screen.is_none() || consumer {
                    let overflow = (self.to_peer.len() + newest.len()).saturating_sub(CAP);
                    self.to_peer.drain(..overflow);
                    self.to_peer.extend(newest);
                }
            }
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
            let delivery = write(peer, &mut self.hello).and_then(|()| {
                if self.hello.is_empty() {
                    write(peer, &mut self.to_peer)
                } else {
                    Ok(())
                }
            });
            if delivery.is_err() {
                self.interrupted = true;
                self.peer.take();
                self.hello.clear();
                self.to_pty.clear();
            } else if self.eof && self.hello.is_empty() && self.to_peer.is_empty() && !self.complete
            {
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
        if let Some((path, _)) = &self.listener {
            let _ = fs::remove_file(path);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use goblins_jsonrpc as rpc;
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
            let mut peer = UnixStream::connect(self.directory.join("terminal.sock")).unwrap();
            peer.set_read_timeout(Some(Duration::from_secs(1))).unwrap();
            self.terminal.tick().unwrap();
            assert_eq!(rpc::read(&mut peer).unwrap()["result"]["attached"], true);
            peer
        }
    }
    /// Close a consumer's end for good: shut the connection down, so the
    /// terminal sees the end at once, even while a process another test
    /// forks still holds a copy of the descriptor.
    fn close(peer: UnixStream) {
        peer.shutdown(Shutdown::Both).unwrap();
    }
    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.directory);
        }
    }

    #[test]
    fn c4_attach_refreshes_closed_peer_before_refusing() {
        let _no_forks = unix::no_forks();
        let mut fixture = Fixture::new();
        let (first, first_runtime) = UnixStream::pair().unwrap();
        fixture
            .terminal
            .attach(first_runtime, serde_json::json!(0))
            .unwrap();
        close(first);
        // Attach is a dispatcher action and can precede the next I/O tick.
        let (mut second, second_runtime) = UnixStream::pair().unwrap();
        second
            .set_read_timeout(Some(Duration::from_secs(1)))
            .unwrap();
        fixture
            .terminal
            .attach(second_runtime, serde_json::json!(0))
            .unwrap();
        fixture.terminal.tick().unwrap();
        assert_eq!(rpc::read(&mut second).unwrap()["result"]["attached"], true);
    }
    #[test]
    fn inherited_listener_is_adopted_and_invalid_descriptors_are_refused() {
        let _no_forks = unix::no_forks();
        let directory = unix::temp_directory().unwrap();
        let path = directory.join("terminal.sock");
        let wrong = directory.join("wrong.sock");
        let listener = UnixListener::bind(&path).unwrap();
        let other = UnixListener::bind(&wrong).unwrap();
        assert!(Terminal::from_listener(&path, other).is_err());
        assert!(path.exists());
        let file = OwnedFd::from(File::open("/dev/null").unwrap());
        assert!(Terminal::from_listener(&path, UnixListener::from(file)).is_err());
        let mut terminal = Terminal::from_listener(&path, listener).unwrap();
        let mut peer = UnixStream::connect(&path).unwrap();
        peer.set_read_timeout(Some(Duration::from_secs(1))).unwrap();
        terminal.tick().unwrap();
        assert_eq!(rpc::read(&mut peer).unwrap()["result"]["attached"], true);
        drop(terminal);
        assert!(!path.exists());
        fs::remove_dir_all(directory).unwrap();
    }
    #[test]
    fn abandoned_handshake_never_enters_reattached_output() {
        for size in [6, CAP] {
            let mut f = Fixture::new();
            let output = vec![b'x'; size];
            f.terminal.to_peer.extend_from_slice(&output);
            let (server, client) = UnixStream::pair().unwrap();
            f.terminal.attach(server, serde_json::json!(123)).unwrap();
            // Disconnect before the first handshake byte can be sent.
            close(client);
            f.terminal.tick().unwrap();
            let mut next = f.connect();
            let mut received = vec![0; size];
            next.read_exact(&mut received).unwrap();
            assert_eq!(received, output);
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
        let mut other = UnixStream::connect(f.directory.join("terminal.sock")).unwrap();
        f.terminal.tick().unwrap();
        assert_eq!(rpc::read(&mut other).unwrap()["error"]["code"], -32009);
        // Input sent by the departing consumer must not reach the payload.
        first.write_all(b"stale input").unwrap();
        f.terminal.tick().unwrap();
        assert!(f.terminal.to_pty.is_empty());
        close(first);
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
        close(f.connect());
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
    fn detached_launch_drains_output_before_first_attachment() {
        let mut f = Fixture::new();
        f.terminal.detached = true;
        for _ in 0..32 {
            f.payload.write_all(&[b'x'; 8192]).unwrap();
            f.terminal.tick().unwrap();
            assert!(f.terminal.to_peer.len() <= CAP);
        }
        f.payload.write_all(b"tail!").unwrap();
        f.terminal.tick().unwrap();
        assert!(f.terminal.to_peer.ends_with(b"tail!"));
        let mut peer = f.connect();
        let mut bytes = vec![0; CAP];
        peer.read_exact(&mut bytes).unwrap();
        assert!(bytes.ends_with(b"tail!"));
    }

    #[test]
    fn closed_consumer_releases_attachment_with_full_input_queue() {
        let mut f = Fixture::new();
        let first = f.connect();
        f.terminal.to_pty.resize(CAP, b'x');
        close(first);
        let _second = f.connect();
        assert!(f.terminal.to_pty.is_empty());
        assert!(f.terminal.attached());
    }

    #[test]
    fn second_consumer_is_rejected_without_interrupting_first() {
        let mut f = Fixture::new();
        let mut first = f.connect();
        let mut second = UnixStream::connect(f.directory.join("terminal.sock")).unwrap();
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
