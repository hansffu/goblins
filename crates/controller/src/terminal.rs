//! Daemon-owned PTY with one terminal consumer at a time.
//! A framed acceptance precedes raw bytes; detach half-closes that stream.
use crate::{Result, unix};
use goblins_protocol::terminal::palette::Palette;
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
    time::{Duration, Instant},
};
const CAP: usize = 65536;
const INPUT_ESCAPE_TIMEOUT: Duration = Duration::from_millis(100);

/// Observe control reports without changing the byte stream sent to the PTY.
/// Unix stream reads may split a report anywhere, including after Escape.
#[derive(Default)]
struct InputEvents {
    pending: Vec<u8>,
    since: Option<Instant>,
    palette: Palette,
    paste: bool,
    string: Option<bool>,
    string_escape: bool,
}
impl InputEvents {
    // None means only terminal reports; Some records human input/interruption.
    fn observe(&mut self, bytes: &[u8], now: Instant) -> Option<bool> {
        let mut human = None;
        if self
            .since
            .is_some_and(|t| now.duration_since(t) >= INPUT_ESCAPE_TIMEOUT)
        {
            human = Some(self.pending == b"\x1b" || self.pending.contains(&3));
            if !self.paste && self.pending.starts_with(b"\x1b]") {
                self.string = Some(true);
                self.string_escape = self.pending.last() == Some(&0x1b);
            }
            self.pending.clear();
            self.since = None;
        }
        for &byte in bytes {
            if let Some(osc) = self.string {
                human = Some(human.unwrap_or(false) || byte == 3);
                if (osc && byte == 7) || (self.string_escape && byte == b'\\') {
                    self.string = None;
                }
                self.string_escape = byte == 0x1b;
                continue;
            }
            if self.pending.is_empty() {
                if byte == 0x1b {
                    self.pending.push(byte);
                    self.since = Some(now);
                } else {
                    human = Some(human.unwrap_or(false) || byte == 3);
                }
                continue;
            }
            self.pending.push(byte);
            let p = &self.pending;
            let osc = p.starts_with(b"\x1b]");
            let osc_end = osc && (p.ends_with(b"\x07") || p.ends_with(b"\x1b\\"));
            let color = osc_end && !self.paste && self.palette.observe(p);
            let report = color
                || (!self.paste
                    && (p == b"\x1b[I"
                        || p == b"\x1b[O"
                        || (p.starts_with(b"\x1b[")
                            && p.len() > 3
                            && matches!(p.last(), Some(b'R' | b'c'))
                            && p[2..p.len() - 1]
                                .iter()
                                .all(|b| b.is_ascii_digit() || b";?>".contains(b)))));
            if p == b"\x1b[200~" {
                self.paste = true;
            }
            if p == b"\x1b[201~" {
                self.paste = false;
            }
            let prefix = (osc && !self.paste && !osc_end && p.len() < 128)
                || p == b"\x1b["
                || (p.starts_with(b"\x1b[")
                    && p.len() < 64
                    && p[2..]
                        .iter()
                        .all(|b| b.is_ascii_digit() || b";?>".contains(b)));
            if report || !prefix {
                if !report {
                    human = Some(human.unwrap_or(false) || self.pending.contains(&3));
                    if !self.paste && osc && !osc_end {
                        self.string = Some(true);
                    } else if !self.paste && p.len() == 2 && p[0] == 0x1b && b"P_^X".contains(&byte)
                    {
                        self.string = Some(false);
                    }
                    self.string_escape = byte == 0x1b;
                }
                self.pending.clear();
                self.since = None;
            }
        }
        human
    }
}
/// Hold only a possible short color query. Other control strings pass through
/// untouched, including strings which contain bytes resembling a color query.
#[derive(Default)]
struct ColorQueries {
    pending: Vec<u8>,
    string: Option<bool>, // OSC accepts BEL as well as ST; DCS/APC/PM do not.
    escape: bool,
    since: Option<Instant>,
}
impl ColorQueries {
    fn flush(&mut self, output: &mut Vec<u8>) {
        output.append(&mut self.pending);
        self.since = None;
    }
    fn process(
        &mut self,
        bytes: &[u8],
        palette: Option<Palette>,
        output: &mut Vec<u8>,
        replies: &mut Vec<u8>,
    ) {
        const QUERIES: &[(&[u8], &[u8])] = &[
            (b"\x1b]10;?\x07", &[10]),
            (b"\x1b]10;?\x1b\\", &[10]),
            (b"\x1b]11;?\x07", &[11]),
            (b"\x1b]11;?\x1b\\", &[11]),
            (b"\x1b]10;?;?\x07", &[10, 11]),
            (b"\x1b]10;?;?\x1b\\", &[10, 11]),
        ];
        for &byte in bytes {
            if let Some(osc) = self.string {
                output.push(byte);
                if (osc && byte == 7) || (self.escape && byte == b'\\') {
                    self.string = None;
                }
                self.escape = byte == 0x1b;
                continue;
            }
            if self.pending.is_empty() {
                if byte == 0x1b {
                    self.pending.push(byte);
                    self.since = Some(Instant::now());
                } else {
                    output.push(byte);
                }
                continue;
            }
            self.pending.push(byte);
            if let Some((_, codes)) = QUERIES.iter().find(|(query, _)| *query == self.pending) {
                let answer: Option<Vec<Vec<u8>>> =
                    palette.and_then(|p| codes.iter().map(|c| p.reply(*c)).collect());
                if let Some(answer) = answer {
                    for reply in answer {
                        replies.extend(reply);
                    }
                    self.pending.clear();
                    self.since = None;
                } else {
                    self.flush(output);
                }
            } else if !QUERIES
                .iter()
                .any(|(query, _)| query.starts_with(&self.pending))
            {
                self.string = if self.pending.starts_with(b"\x1b]") && byte != 7 {
                    Some(true)
                } else if self.pending.len() == 2 && b"P_^X".contains(&byte) {
                    Some(false)
                } else {
                    None
                };
                self.escape = byte == 0x1b;
                self.flush(output);
                // A second Escape can start another sequence.
                if self.string.is_none() && byte == 0x1b {
                    output.pop();
                    self.pending.push(byte);
                    self.since = Some(Instant::now());
                }
            }
        }
    }
}
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
    hello: Vec<u8>,
    to_peer: Vec<u8>,
    to_pty: Vec<u8>,
    screen: vt100::Parser,
    color_queries: ColorQueries,
    color_replies: Vec<u8>,
    revision: u64,
    input_revision: u64,
    input_interrupted: bool,
    input_events: InputEvents,
    last_output: std::time::Instant,
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
            hello: vec![],
            to_peer: vec![],
            to_pty: vec![],
            screen: vt100::Parser::new(24, 80, 0),
            color_queries: ColorQueries::default(),
            color_replies: Vec::new(),
            revision: 0,
            input_revision: 0,
            input_interrupted: false,
            input_events: InputEvents::default(),
            last_output: std::time::Instant::now(),
        })
    }
    pub fn palette(&self) -> Palette {
        self.input_events.palette
    }
    pub fn set_palette(&mut self, palette: Palette) {
        self.input_events.palette = palette;
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
        // The screen model needs room to wrap: vt100 panics wrapping on a
        // single row or column. The PTY still gets the real size.
        self.screen
            .screen_mut()
            .set_size(rows.clamp(2, 200), cols.clamp(2, 500));
        self.revision += 1;
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
    pub fn attach(&mut self, peer: UnixStream, id: serde_json::Value) -> Result<()> {
        if self.peer.is_some() || self.complete {
            return Err(if self.complete {
                "terminal already completed"
            } else {
                "terminal is already attached"
            }
            .into());
        }
        peer.set_nonblocking(true)?;
        self.hello = goblins_protocol::rpc::encode(&goblins_protocol::rpc::result(
            id,
            serde_json::json!({"attached":true}),
        ))?;
        // A new consumer has its own terminal input modes. The payload may
        // have enabled mouse reporting only once, long before this attach;
        // those bytes may already be consumed or evicted from the output tail.
        let screen = self.screen.screen();
        if screen.application_keypad()
            || screen.application_cursor()
            || screen.bracketed_paste()
            || screen.mouse_protocol_mode() != vt100::MouseProtocolMode::None
            || screen.mouse_protocol_encoding() != vt100::MouseProtocolEncoding::Default
        {
            self.hello.extend(screen.input_mode_formatted());
        }
        // Keep this consumer's acceptance and mode restore separate from the
        // retained payload output: an abandoned handshake must never become
        // terminal text when a later client attaches.
        self.peer = Some(peer);
        self.connected = true;
        self.detached = false;
        self.interrupted = false;
        Ok(())
    }
    pub fn integration_view(&self) -> crate::integration::View {
        let screen = self.screen.screen();
        let (row, col) = screen.cursor_position();
        let text = screen.contents();
        // Claude indents its composer in some layouts. Match the two prompt
        // cells immediately before the cursor, while requiring any left
        // padding to be blank and all composer contents to be empty/dimmed.
        let prompt = col.checked_sub(2).is_some_and(|start| {
            screen
                .cell(row, start)
                .is_some_and(|cell| matches!(cell.contents(), "›" | "❯"))
                && screen
                    .cell(row, start + 1)
                    .is_none_or(|cell| cell.contents().trim().is_empty())
                && (0..start).all(|column| {
                    screen
                        .cell(row, column)
                        .is_none_or(|cell| cell.contents().trim().is_empty())
                })
        });
        let empty_or_placeholder = (col..screen.size().1).all(|column| {
            screen
                .cell(row, column)
                .is_none_or(|c| c.contents().trim().is_empty() || c.dim())
        });
        let ready = empty_or_placeholder
            && prompt
            && !screen.hide_cursor()
            && !text.contains("esc to interrupt")
            && self.last_output.elapsed() >= std::time::Duration::from_millis(300)
            && self.to_pty.is_empty()
            && self.color_replies.is_empty()
            && self.input_events.pending.is_empty()
            && !self.eof;
        crate::integration::View {
            revision: self.revision,
            ready,
            input: self.input_revision,
            interrupted: self.input_interrupted,
            input_pending: !self.input_events.pending.is_empty(),
        }
    }
    pub fn notify(&mut self, revision: u64) -> Result<()> {
        if revision != self.revision
            || !self.integration_view().ready
            || self
                .peer
                .as_ref()
                .is_some_and(|p| unix::readable(p.as_raw_fd(), 0).unwrap_or(true))
        {
            return Err("terminal changed; notification deferred".into());
        }
        let bytes = format!("\x1b[200~{}\x1b[201~\r", crate::integration::PROMPT);
        self.master
            .as_mut()
            .ok_or("terminal unavailable")?
            .write_all(bytes.as_bytes())?;
        self.revision += 1;
        Ok(())
    }
    pub fn tick(&mut self) -> Result<()> {
        if let Some(interrupted) = self.input_events.observe(&[], Instant::now()) {
            self.input_revision += 1;
            self.input_interrupted = interrupted;
            self.revision += 1;
        }
        if let Some(peer) = &mut self.peer {
            let input_before = self.to_pty.len();
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
                if !self.complete && !self.detached {
                    self.interrupted = true;
                }
                self.to_pty.clear();
            }
            if self.to_pty.len() > input_before {
                let bytes = &self.to_pty[input_before..];
                // Focus and query reports don't give the human ownership of
                // an otherwise empty composer. Still invalidate in-flight views.
                self.revision += 1;
                if let Some(interrupted) = self.input_events.observe(bytes, Instant::now()) {
                    self.input_revision += 1;
                    self.input_interrupted = interrupted;
                }
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
                    let writer = peer.try_clone()?;
                    if let Err(error) = self.attach(writer, json!(0)) {
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
        let palette = if self.attached() {
            None
        } else {
            Some(self.palette())
        };
        if self
            .color_queries
            .since
            .is_some_and(|t| t.elapsed() >= INPUT_ESCAPE_TIMEOUT)
        {
            if self.color_queries.pending.starts_with(b"\x1b]") {
                self.color_queries.string = Some(true);
                self.color_queries.escape = self.color_queries.pending.last() == Some(&0x1b);
            }
            self.color_queries.flush(&mut self.to_peer);
        }
        if let Some(master) = &mut self.master
            && !self.eof
        {
            // Before the first consumer, and while attached, preserve lossless
            // backpressure. Detached payloads keep running: retain only a
            // bounded tail, making room for the next read when necessary.
            let mut newest = Vec::new();
            let tail = self.detached || (self.connected && self.peer.is_none());
            let result = if self.color_replies.len() <= CAP - 8192
                && (tail || self.to_peer.len() + self.color_queries.pending.len() < CAP)
            {
                // Preserve backpressure by limiting the read to remaining room.
                let mut buf = [0; 8192];
                let room = if tail {
                    buf.len()
                } else {
                    buf.len()
                        .min(CAP - self.to_peer.len() - self.color_queries.pending.len())
                }
                .min((CAP - self.color_replies.len()) / 8);
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
                self.screen.process(&newest);
                self.revision += 1;
                self.last_output = std::time::Instant::now();
                let mut output = Vec::new();
                self.color_queries
                    .process(&newest, palette, &mut output, &mut self.color_replies);
                let overflow =
                    (self.to_peer.len() + output.len() + self.color_queries.pending.len())
                        .saturating_sub(CAP);
                self.to_peer.drain(..overflow);
                self.to_peer.extend(output);
            }
            match result {
                Ok(eof) => self.eof = eof,
                Err(e) if e.raw_os_error() == Some(libc::EIO) => self.eof = true,
                Err(_) => {
                    self.eof = true;
                    self.interrupted = true;
                }
            }
            if self.eof {
                self.color_queries.flush(&mut self.to_peer);
            }
            if !self.eof && write(master, &mut self.color_replies).is_err() {
                self.color_replies.clear();
            }
            if !self.eof
                && self.color_replies.is_empty()
                && write(master, &mut self.to_pty).is_err()
            {
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
    fn detached_color_queries_use_only_known_colors_without_claiming_human_input() {
        for state in ["pre-attach", "detached", "disconnected"] {
            let mut f = Fixture::new();
            if state != "pre-attach" {
                let peer = f.connect();
                if state == "detached" {
                    f.terminal.detach().unwrap();
                }
                drop(peer);
                f.terminal.tick().unwrap();
            }
            let palette = Palette {
                foreground: Some([0xdddd; 3]),
                background: Some([0x2020, 0x2424, 0x2a2a]),
            };
            f.terminal.set_palette(palette);
            for part in [b"\x1b]1".as_slice(), b"0;?;?\x1b", b"\\"] {
                f.payload.write_all(part).unwrap();
                f.terminal.tick().unwrap();
            }
            f.payload
                .set_read_timeout(Some(Duration::from_secs(1)))
                .unwrap();
            let mut expected = palette.reply(10).unwrap();
            expected.extend(palette.reply(11).unwrap());
            let mut actual = vec![0; expected.len()];
            f.payload.read_exact(&mut actual).unwrap();
            assert_eq!(actual, expected, "{state}");
            assert_eq!(f.terminal.input_revision, 0);
            assert!(!f.terminal.input_interrupted);
            assert!(f.terminal.input_events.pending.is_empty());
        }
        let mut f = Fixture::new();
        f.terminal.set_palette(Palette {
            background: Some([0; 3]),
            ..Palette::default()
        });
        f.payload
            .write_all(b"\x1b]10;?\x07\x1b]11;?\x07\x1b]12;?\x07")
            .unwrap();
        f.terminal.tick().unwrap();
        f.payload.set_nonblocking(true).unwrap();
        let mut reply = [0; 128];
        let n = f.payload.read(&mut reply).unwrap();
        assert_eq!(&reply[..n], b"\x1b]11;rgb:0000/0000/0000\x1b\\");
        assert!(
            matches!(f.payload.read(&mut reply), Err(e) if e.kind() == io::ErrorKind::WouldBlock)
        );
    }
    #[test]
    fn attached_colors_pass_through_and_refresh_the_detached_palette() {
        let mut f = Fixture::new();
        let mut peer = f.connect();
        f.payload.write_all(b"\x1b]11;?\x07").unwrap();
        f.terminal.tick().unwrap();
        let mut query = [0; 7];
        peer.read_exact(&mut query).unwrap();
        assert_eq!(&query, b"\x1b]11;?\x07");
        f.payload.set_nonblocking(true).unwrap();
        let mut bytes = [0; 128];
        assert!(
            matches!(f.payload.read(&mut bytes), Err(e) if e.kind() == io::ErrorKind::WouldBlock)
        );
        let report = b"\x1b]11;rgb:12/34/56\x1b\\";
        for byte in report {
            peer.write_all(&[*byte]).unwrap();
            f.terminal.tick().unwrap();
        }
        let n = f.payload.read(&mut bytes).unwrap();
        assert_eq!(&bytes[..n], report);
        assert_eq!(
            f.terminal.palette().background,
            Some([0x1212, 0x3434, 0x5656])
        );
        assert_eq!(f.terminal.input_revision, 0);
        f.terminal.detach().unwrap();
        drop(peer);
        f.terminal.tick().unwrap();
        f.payload.write_all(b"\x1b]11;?\x07").unwrap();
        f.terminal.tick().unwrap();
        let n = f.payload.read(&mut bytes).unwrap();
        assert_eq!(&bytes[..n], b"\x1b]11;rgb:1212/3434/5656\x1b\\");
    }
    #[test]
    fn color_report_fragments_validate_without_swallowing_pasted_or_malformed_input() {
        let report = b"\x1b]11;rgb:f/12/abc\x07";
        let now = Instant::now();
        for split in 0..=report.len() {
            let mut input = InputEvents::default();
            assert_eq!(input.observe(&report[..split], now), None);
            assert_eq!(input.observe(&report[split..], now), None);
            assert_eq!(input.palette.background, Some([65535, 0x1212, 0xabca]));
            assert!(input.pending.is_empty());
        }
        let mut input = InputEvents::default();
        assert_eq!(
            input.observe(b"\x1b[200~\x1b]11;rgb:0/0/0\x07\x1b[201~", now),
            Some(false)
        );
        assert!(input.palette.is_empty());
        assert_eq!(input.observe(b"\x1b]11;rgb:invalid\x07", now), Some(false));
        assert!(input.palette.is_empty());
        assert_eq!(input.observe(b"\x1b]11;\x03\x07", now), Some(true));
        input.observe(b"\x1b]11;rgb:", now);
        assert_eq!(input.observe(&[], now + INPUT_ESCAPE_TIMEOUT), Some(false));
        assert!(input.pending.is_empty());
        input.observe(&[b'\x1b', b']'], now);
        input.observe(&[b'x'; 1024], now);
        assert!(input.pending.len() < 128);
    }

    #[test]
    fn answered_queries_are_not_replayed_and_other_output_stays_byte_exact() {
        let palette = Palette {
            foreground: Some([0xffff; 3]),
            background: Some([0x2020, 0x2424, 0x2a2a]),
        };
        for query in [
            b"\x1b]10;?\x07".as_slice(),
            b"\x1b]11;?\x1b\\",
            b"\x1b]10;?;?\x07",
            b"\x1b]10;?;?\x1b\\",
        ] {
            for split in 0..=query.len() {
                let mut scanner = ColorQueries::default();
                let mut output = Vec::new();
                let mut replies = Vec::new();
                scanner.process(b"before", Some(palette), &mut output, &mut replies);
                scanner.process(&query[..split], Some(palette), &mut output, &mut replies);
                scanner.process(&query[split..], Some(palette), &mut output, &mut replies);
                scanner.process(b"after", Some(palette), &mut output, &mut replies);
                assert_eq!(output, b"beforeafter");
                let expected = if query.starts_with(b"\x1b]11") {
                    palette.reply(11).unwrap()
                } else if query.contains(&b';') && query.iter().filter(|b| **b == b'?').count() == 2
                {
                    [palette.reply(10).unwrap(), palette.reply(11).unwrap()].concat()
                } else {
                    palette.reply(10).unwrap()
                };
                assert_eq!(replies, expected);
                for known in [None, Some(Palette::default())] {
                    let mut s = ColorQueries::default();
                    let mut o = Vec::new();
                    let mut r = Vec::new();
                    s.process(query, known, &mut o, &mut r);
                    assert_eq!(o, query);
                    assert!(r.is_empty());
                }
            }
        }
        for bytes in [
            b"\x1b]10;rgb:1/2/3\x07".as_slice(),
            b"\x1b]12;?\x07",
            b"\x1b]11;??\x07",
            b"\x1bPignored\x1b]11;?\x07\x1b\\",
            b"\x1b]52;ignored\x1b]11;?\x07",
        ] {
            let mut scanner = ColorQueries::default();
            let mut output = Vec::new();
            let mut replies = Vec::new();
            for byte in bytes {
                scanner.process(&[*byte], Some(palette), &mut output, &mut replies);
            }
            scanner.flush(&mut output);
            assert_eq!(output, bytes);
            assert!(replies.is_empty());
        }
        let mut f = Fixture::new();
        f.terminal.set_palette(palette);
        f.payload.write_all(b"before\x1b]11;?\x07after").unwrap();
        f.terminal.tick().unwrap();
        let mut peer = f.connect();
        let mut output = [0; 11];
        peer.read_exact(&mut output).unwrap();
        assert_eq!(&output, b"beforeafter");
        // EOF must release a held incomplete prefix without losing bytes.
        f.payload.write_all(b"\x1b]11;").unwrap();
        f.terminal.tick().unwrap();
        f.payload.shutdown(Shutdown::Write).unwrap();
        f.terminal.tick().unwrap();
        let mut output = Vec::new();
        peer.read_to_end(&mut output).unwrap();
        assert_eq!(output, b"\x1b]11;");
    }
    #[test]
    fn malformed_osc_does_not_hide_later_queries_or_the_end_of_paste() {
        let p = Palette {
            background: Some([0; 3]),
            ..Palette::default()
        };
        for malformed in [b"\x1b]1\x07".as_slice(), b"\x1b]10;\x07", b"\x1b]10;?;\x07"] {
            let mut scanner = ColorQueries::default();
            let mut output = Vec::new();
            let mut replies = Vec::new();
            scanner.process(malformed, Some(p), &mut output, &mut replies);
            scanner.process(b"\x1b]11;?\x07", Some(p), &mut output, &mut replies);
            assert_eq!(output, malformed);
            assert_eq!(replies, p.reply(11).unwrap());
        }
        let now = Instant::now();
        let mut input = InputEvents::default();
        input.observe(b"\x1b[200~\x1b]unterminated", now);
        input.observe(&[], now + INPUT_ESCAPE_TIMEOUT);
        input.observe(b"\x1b[201~", now + INPUT_ESCAPE_TIMEOUT);
        assert!(!input.paste);
        assert!(input.string.is_none());
        assert_eq!(
            input.observe(b"\x1b]11;rgb:0/0/0\x07", now + INPUT_ESCAPE_TIMEOUT),
            None
        );
        assert_eq!(input.palette, p);
    }
    #[test]
    fn a_tiny_window_never_breaks_the_screen_model() {
        // A client in a one-line window (or a terminal reporting 0x0, sent
        // as 1x1) resizes the payload to that; wrapping output must not
        // panic the daemon in the screen parser.
        let mut f = Fixture::new();
        for (rows, cols) in [(1, 1), (1, 80), (24, 1)] {
            // The fixture's master is no PTY, so only the model resizes.
            let _ = f.terminal.resize(rows, cols);
            f.payload
                .write_all(
                    "wrap this line past the edge 中文 again\r\n"
                        .repeat(3)
                        .as_bytes(),
                )
                .unwrap();
            for _ in 0..4 {
                f.terminal.tick().unwrap();
            }
        }
    }
    #[test]
    fn terminal_reports_are_independent_of_stream_read_boundaries() {
        let now = Instant::now();
        let reports = b"\x1b[I\x1b[O\x1b[12;3R\x1b[?1;2c";
        for split in 0..=reports.len() {
            let mut input = InputEvents::default();
            assert_eq!(input.observe(&reports[..split], now), None);
            assert_eq!(input.observe(&reports[split..], now), None);
            assert!(input.pending.is_empty());
        }
        let mut input = InputEvents::default();
        for byte in reports {
            assert_eq!(input.observe(&[*byte], now), None);
        }
        assert!(input.pending.is_empty());
        for bytes in [
            b"\x1b[Idraft\x1b[O".as_slice(),
            b"\x1b[A", // Arrow keys are human input, not terminal reports.
            b"\x1b[200~paste\x1b[201~",
            b"\x1b[1I", // Only unparameterized focus events are reports.
        ] {
            assert_eq!(input.observe(bytes, now), Some(false));
        }
        assert_eq!(input.observe(b"\x03\x1b[O", now), Some(true));
    }

    #[test]
    fn incomplete_terminal_reports_expire_without_losing_escape_interrupts() {
        let now = Instant::now();
        let mut input = InputEvents::default();
        assert_eq!(input.observe(b"\x1b", now), None);
        assert_eq!(input.observe(&[], now + INPUT_ESCAPE_TIMEOUT), Some(true));
        assert!(input.pending.is_empty());
        assert_eq!(input.observe(b"\x1b[", now), None);
        assert_eq!(input.observe(&[], now + INPUT_ESCAPE_TIMEOUT), Some(false));
        assert!(input.pending.is_empty());
        let oversized = format!("\x1b[{}", "1".repeat(100));
        assert_eq!(input.observe(oversized.as_bytes(), now), Some(false));
        assert!(input.pending.is_empty());
    }

    #[test]
    fn focus_changes_preserve_idle_input_revision_and_raw_pty_bytes() {
        let mut f = Fixture::new();
        let mut peer = f.connect();
        f.payload
            .write_all("\x1b[2J\x1b[H› \x1b[?25h".as_bytes())
            .unwrap();
        f.terminal.tick().unwrap();
        f.terminal.last_output -= Duration::from_secs(1);
        let idle = f.terminal.integration_view();
        assert!(idle.ready);
        for chunk in [b"\x1b".as_slice(), b"[", b"I\x1b[", b"O"] {
            peer.write_all(chunk).unwrap();
            f.terminal.tick().unwrap();
            assert_eq!(f.terminal.integration_view().input, idle.input);
            if !f.terminal.input_events.pending.is_empty() {
                assert!(!f.terminal.integration_view().ready);
            }
        }
        let mut received = [0; 6];
        f.payload.read_exact(&mut received).unwrap();
        assert_eq!(&received, b"\x1b[I\x1b[O");
        let view = f.terminal.integration_view();
        assert!(view.ready);
        f.terminal.notify(view.revision).unwrap();
        peer.write_all(b"\x1b[Idraft\x1b[O").unwrap();
        f.terminal.tick().unwrap();
        assert!(f.terminal.integration_view().input > idle.input);
    }

    #[test]
    fn wake_requires_current_empty_composer_and_yields_to_human_input() {
        let mut f = Fixture::new();
        f.terminal.detached = true;
        f.payload
            .write_all("\x1b[2J\x1b[H› \x1b[2mAsk a question\x1b[0m\x1b[1;3H\x1b[?25h".as_bytes())
            .unwrap();
        f.terminal.tick().unwrap();
        f.terminal.last_output -= Duration::from_secs(1);
        let view = f.terminal.integration_view();
        assert!(view.ready);
        assert!(f.terminal.notify(view.revision + 1).is_err());
        f.terminal.notify(view.revision).unwrap();
        let mut bytes = [0; 256];
        let n = f.payload.read(&mut bytes).unwrap();
        assert_eq!(
            &bytes[..n],
            format!("\x1b[200~{}\x1b[201~\r", crate::integration::PROMPT).as_bytes()
        );
        let mut peer = f.connect();
        peer.write_all(b"draft").unwrap();
        assert!(f.terminal.notify(f.terminal.revision).is_err());
        f.terminal.tick().unwrap();
        assert_eq!(f.terminal.integration_view().input, 1);
        f.payload
            .write_all("\x1b[2J\x1b[H› existing draft\x1b[1;3H".as_bytes())
            .unwrap();
        f.terminal.tick().unwrap();
        f.terminal.last_output -= Duration::from_secs(1);
        assert!(!f.terminal.integration_view().ready);
    }

    #[test]
    fn wake_recognizes_an_indented_claude_composer() {
        let mut f = Fixture::new();
        f.terminal.detached = true;
        f.payload
            .write_all("\x1b[2J\x1b[H  ❯ \x1b[2mTry a command\x1b[0m\x1b[1;5H\x1b[?25h".as_bytes())
            .unwrap();
        f.terminal.tick().unwrap();
        f.terminal.last_output -= Duration::from_secs(1);
        let view = f.terminal.integration_view();
        assert!(view.ready);
        f.terminal.notify(view.revision).unwrap();
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
            drop(client);
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
    fn attachment_restores_active_input_modes_without_payload_redraw() {
        let modes = b"\x1b=\x1b[?1h\x1b[?2004h\x1b[?1000h\x1b[?1002h\x1b[?1003h\x1b[?1006h";
        let marker = b"new consumer ready";
        let wheel = b"\x1b[<64;12;8M";
        for attachment in ["detach", "disconnect", "detached launch"] {
            for overflow in [false, true] {
                let mut f = Fixture::new();
                if attachment == "detached launch" {
                    f.terminal.detached = true;
                    f.payload.write_all(modes).unwrap();
                    f.terminal.tick().unwrap();
                } else {
                    let mut first = f.connect();
                    f.payload.write_all(modes).unwrap();
                    f.terminal.tick().unwrap();
                    let mut received = vec![0; modes.len()];
                    first.read_exact(&mut received).unwrap();
                    assert_eq!(received, modes);
                    if attachment == "detach" {
                        f.terminal.detach().unwrap();
                        assert_eq!(first.read(&mut [0]).unwrap(), 0);
                    }
                    drop(first);
                    f.terminal.tick().unwrap();
                }
                if overflow {
                    for _ in 0..16 {
                        f.payload.write_all(&[b'x'; 8192]).unwrap();
                        f.terminal.tick().unwrap();
                        assert!(f.terminal.to_peer.len() <= CAP);
                    }
                }
                f.payload.write_all(marker).unwrap();
                let mut second = f.connect();
                let mut received = Vec::new();
                while !received.ends_with(marker) {
                    f.terminal.tick().unwrap();
                    let mut bytes = [0; 8192];
                    let count = second.read(&mut bytes).unwrap();
                    assert_ne!(count, 0);
                    received.extend_from_slice(&bytes[..count]);
                }
                // A fresh terminal has none of the previous consumer's modes.
                let mut consumer = vt100::Parser::new(24, 80, 0);
                consumer.process(&received);
                let screen = consumer.screen();
                assert_eq!(
                    screen.mouse_protocol_mode(),
                    vt100::MouseProtocolMode::AnyMotion,
                    "{attachment}, overflow={overflow}"
                );
                assert_eq!(
                    screen.mouse_protocol_encoding(),
                    vt100::MouseProtocolEncoding::Sgr
                );
                assert!(screen.application_keypad());
                assert!(screen.application_cursor());
                assert!(screen.bracketed_paste());
                // Restoration is output only; wheel input still reaches the payload unchanged.
                second.write_all(wheel).unwrap();
                f.terminal.tick().unwrap();
                let mut received = vec![0; wheel.len()];
                f.payload.read_exact(&mut received).unwrap();
                assert_eq!(received, wheel);
            }
        }
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
