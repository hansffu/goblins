//! Bounded, readiness-driven forwarding for the sandbox-local WebSockets.
use crate::Result;
use std::{
    collections::VecDeque,
    io,
    os::{fd::AsRawFd, unix::net::UnixStream},
    time::{Duration, Instant},
};
use tungstenite::{
    Message, WebSocket,
    protocol::{CloseFrame, frame::coding::CloseCode},
};

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Direction {
    Native,
    Server,
}

pub struct Action {
    pub forward: Option<Message>,
    pub reply: Option<Message>,
}
impl Action {
    pub fn forward(message: Message) -> Self {
        Self {
            forward: Some(message),
            reply: None,
        }
    }
    pub fn reply(message: Message) -> Self {
        Self {
            forward: None,
            reply: Some(message),
        }
    }
}

struct Peer {
    ws: WebSocket<UnixStream>,
    queue: VecDeque<Message>,
    flushing: bool,
}
fn blocked(error: &tungstenite::Error) -> bool {
    matches!(error, tungstenite::Error::Io(e) if e.kind() == io::ErrorKind::WouldBlock)
}
impl Peer {
    fn new(ws: WebSocket<UnixStream>) -> Result<Self> {
        ws.get_ref().set_read_timeout(None)?;
        ws.get_ref().set_write_timeout(None)?;
        ws.get_ref().set_nonblocking(true)?;
        Ok(Self {
            ws,
            queue: VecDeque::new(),
            flushing: false,
        })
    }
    fn pending(&self) -> bool {
        self.flushing || !self.queue.is_empty()
    }
    fn flush(&mut self) -> Result<bool> {
        let mut progress = false;
        if self.flushing {
            match self.ws.flush() {
                Ok(()) | Err(tungstenite::Error::ConnectionClosed) => {
                    self.flushing = false;
                    progress = true;
                }
                Err(e) if blocked(&e) => return Ok(false),
                Err(e) => return Err(e.into()),
            }
        }
        if let Some(message) = self.queue.pop_front() {
            match self.ws.write(message) {
                Ok(()) => (),
                // Tungstenite retains the partially written frame. Never
                // submit it again: flush the retained bytes on POLLOUT.
                Err(e) if blocked(&e) => (),
                Err(tungstenite::Error::WriteBufferFull(message)) => {
                    self.queue.push_front(*message);
                    self.flushing = true;
                    return Ok(progress);
                }
                Err(e) => return Err(e.into()),
            }
            self.flushing = true;
            progress = true;
        }
        Ok(progress)
    }
    fn read(&mut self) -> Result<Option<Message>> {
        match self.ws.read() {
            Ok(message) => {
                // This also flushes Tungstenite's automatic pong/close reply.
                self.flushing = true;
                Ok(Some(message))
            }
            Err(e) if blocked(&e) => Ok(None),
            Err(e) => Err(e.into()),
        }
    }
    fn events(&self, readable: bool) -> i16 {
        (if readable { libc::POLLIN } else { 0 }) | (if self.pending() { libc::POLLOUT } else { 0 })
    }
}

fn poll(native: &Peer, server: &Peer, closing: bool, timeout: i32) -> Result<()> {
    let mut fds = [
        libc::pollfd {
            fd: native.ws.get_ref().as_raw_fd(),
            events: native.events(!closing && !server.pending() && native.queue.is_empty()),
            revents: 0,
        },
        libc::pollfd {
            fd: server.ws.get_ref().as_raw_fd(),
            events: server.events(!closing && !native.pending() && server.queue.is_empty()),
            revents: 0,
        },
    ];
    for fd in &mut fds {
        if fd.events == 0 {
            fd.fd = -1;
        }
    }
    loop {
        let result = unsafe { libc::poll(fds.as_mut_ptr(), fds.len() as libc::nfds_t, timeout) };
        if result >= 0 {
            return Ok(());
        }
        let error = io::Error::last_os_error();
        if error.kind() != io::ErrorKind::Interrupted {
            return Err(error.into());
        }
    }
}

pub fn relay(
    native: WebSocket<UnixStream>,
    server: WebSocket<UnixStream>,
    mut inspect: impl FnMut(Direction, Message) -> Result<Action>,
) -> Result<()> {
    let mut native = Peer::new(native)?;
    let mut server = Peer::new(server)?;
    let result = (|| -> Result<()> {
        let mut closing = false;
        loop {
            let mut progress = native.flush()? | server.flush()?;
            if closing && !native.pending() && !server.pending() {
                return Ok(());
            }
            for direction in [Direction::Native, Direction::Server] {
                let (source, destination) = if direction == Direction::Native {
                    (&mut native, &mut server)
                } else {
                    (&mut server, &mut native)
                };
                if closing || destination.pending() || !source.queue.is_empty() {
                    continue;
                }
                if let Some(message) = source.read()? {
                    progress = true;
                    closing = matches!(message, Message::Close(_));
                    if matches!(
                        message,
                        Message::Text(_) | Message::Binary(_) | Message::Close(_)
                    ) {
                        let action = inspect(direction, message)?;
                        if let Some(message) = action.forward {
                            destination.queue.push_back(message);
                        }
                        if let Some(message) = action.reply {
                            source.queue.push_back(message);
                        }
                    }
                }
            }
            if !progress {
                poll(&native, &server, closing, -1)?;
            }
        }
    })();
    if let Err(error) = &result {
        // Standard WebSocket close reasons reach Codex's disconnect UI. Best
        // effort only: a broken/unresponsive terminal must not hold cleanup.
        let mut reason = format!("Goblins proxy: {error}");
        while reason.len() > 123 {
            reason.pop();
        }
        let _ = native.ws.close(Some(CloseFrame {
            code: CloseCode::Error,
            reason: reason.into(),
        }));
        native.flushing = true;
        native.queue.clear();
        let deadline = Instant::now() + Duration::from_millis(200);
        while native.pending() && Instant::now() < deadline {
            if native.flush().is_err() {
                break;
            }
            let _ = poll(&native, &server, true, 10);
        }
    }
    result
}
