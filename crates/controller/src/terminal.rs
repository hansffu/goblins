//! Daemon-owned PTY and one raw terminal stream. Exit does not discard bytes.
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
    pub fn tick(&mut self) -> Result<()> {
        for _ in 0..4 {
            match self.listener.accept() {
                Ok((peer, _)) if !self.connected => {
                    peer.set_nonblocking(true)?;
                    self.peer = Some(peer);
                    self.connected = true;
                }
                Ok(_) => (),
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => break,
                Err(e) => return Err(e.into()),
            }
        }
        if let Some(peer) = &mut self.peer {
            // Even after PTY EOF, continue draining output. Peer input EOF is a
            // consumer disconnect, not payload exit or session cancellation.
            if read(peer, &mut self.to_pty).unwrap_or(true) {
                self.peer.take();
                if !self.complete {
                    self.interrupted = true;
                }
                self.to_pty.clear();
            }
        }
        if self.interrupted {
            self.to_peer.clear();
        }
        if let Some(master) = &mut self.master
            && !self.eof
        {
            match read(master, &mut self.to_peer) {
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
        if self.interrupted {
            self.to_peer.clear();
        }
        if let Some(peer) = &mut self.peer {
            if write(peer, &mut self.to_peer).is_err() {
                self.interrupted = true;
                self.peer.take();
                self.to_peer.clear();
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
