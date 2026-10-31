//! Shared synchronous one-shot control; never called from the daemon event pump.
use super::{Child, EXIT_GRACE, Executable};
use crate::Result;
use goblins_runtime::{
    Event,
    channel::Channel,
    supervisor::{Decoder, Encoder, Message, Received, outbound::Writer},
};
use std::{path::PathBuf, time::Instant};
pub(super) enum Expected {
    Manifest { path: PathBuf, sequence: u64 },
    Maintenance,
}
impl Expected {
    fn allows(&self, method: &str) -> bool {
        match self {
            Self::Manifest { .. } => matches!(method, "Event.Manifest" | "PreflightFailed"),
            Self::Maintenance => method == "MaintenanceFinished",
        }
    }
    fn error(&self, before: &str, after: &str) -> Box<dyn std::error::Error + Send + Sync> {
        let name = match self {
            Self::Manifest { .. } => "preflight",
            Self::Maintenance => "maintenance",
        };
        if before.is_empty() {
            format!("{name} {after}").into()
        } else {
            format!("{before} {name} {after}").into()
        }
    }
}
pub(super) fn request(
    executable: &Executable,
    message: Message,
    expected: Expected,
    cancel: impl Fn() -> bool,
) -> Result<Message> {
    if cancel() {
        return Err("session cancelled".into());
    }
    let (child, socket) = match expected {
        Expected::Maintenance => Child::start_maintenance(executable.path()?)?,
        Expected::Manifest { .. } => Child::start(executable.path()?)?,
    };
    let mut attempt = Attempt {
        child: Some(child),
        channel: Channel::new(socket)?,
        decoder: Decoder::default(),
        encoder: Encoder::default(),
        writer: Writer::default(),
        result: None,
        extra: None,
        deadline: None,
        expected,
    };
    attempt
        .writer
        .begin(attempt.encoder.prepare(message)?)
        .map_err(|_| attempt.expected.error("", "writer busy"))?;
    attempt.exchange(cancel)?;
    Ok(attempt.result.take().unwrap())
}
struct Attempt {
    child: Option<Child>,
    channel: Channel,
    decoder: Decoder,
    encoder: Encoder,
    writer: Writer,
    // Full typed messages survive malformed transcripts/EOF until confirmed reap.
    result: Option<Message>,
    extra: Option<Message>,
    deadline: Option<Instant>,
    expected: Expected,
}
impl Attempt {
    fn receive_result(&mut self) -> Result<Option<Received>> {
        let frame = match self.channel.receive() {
            Ok(Some(frame)) => frame,
            Ok(None) => {
                if self.channel.closed() {
                    self.decoder.finish()?;
                }
                return Ok(None);
            }
            // Preserve the common codec's EOF handling after Channel has
            // disposed raw descriptors and closed the reset transport.
            Err(error) if error.kind() == std::io::ErrorKind::ConnectionReset => {
                self.decoder.finish()?;
                return Ok(None);
            }
            Err(error) => return Err(error.into()),
        };
        if !matches!(
            frame.value.get("method").and_then(|v| v.as_str()),
            Some(method) if self.expected.allows(method)
        ) {
            // Both legal results are single-frame. Reject a wrong direction
            // before the general decoder could retain a partial Handoff and
            // leave us waiting indefinitely for a complete logical result.
            drop(frame);
            self.decoder.close(&mut self.channel);
            return Err(self.expected.error("invalid", "result method"));
        }
        let received = self
            .decoder
            .receive(frame)?
            .ok_or_else(|| self.expected.error("incomplete", "result"))?;
        Ok(Some(received))
    }
    fn exchange(&mut self, cancel: impl Fn() -> bool) -> Result<()> {
        loop {
            if cancel() {
                return Err("session cancelled".into());
            }
            if self.deadline.is_some_and(|d| Instant::now() >= d) {
                return Err(self
                    .expected
                    .error("", "did not finish within 2 seconds after its result"));
            }
            // EOF can precede the child's final destructors and reap. Do not
            // flush a closed transport while waiting; the drained check below
            // still rejects any acknowledgement that did not actually flush.
            if !self.channel.closed() {
                self.writer.pump(&mut self.channel)?;
            }
            if let Some(received) = self.receive_result()? {
                if self.result.is_some() {
                    self.extra = Some(received.message);
                    return Err(self.expected.error("extra", "result"));
                }
                self.deadline = Some(Instant::now() + EXIT_GRACE);
                self.result = Some(received.message);
                match (&self.expected, self.result.as_ref().unwrap()) {
                    (
                        Expected::Manifest { path, sequence },
                        Message::Event(Event::Manifest {
                            sequence: actual,
                            lease,
                        }),
                    ) if actual == sequence && &lease.path == path => {
                        let transfer = received
                            .adopted
                            .ok_or_else(|| self.expected.error("", "manifest lacks adoption"))?;
                        self.writer
                            .begin(self.encoder.prepare(Message::Adopted { transfer })?)
                            .map_err(|_| self.expected.error("", "acknowledgement writer busy"))?;
                    }
                    (Expected::Manifest { .. }, Message::PreflightFailed { .. })
                    | (Expected::Maintenance, Message::MaintenanceFinished { .. })
                        if received.adopted.is_none() =>
                    {
                        ()
                    }
                    _ => return Err(self.expected.error("invalid", "result")),
                }
            }
            let reaped = self.child.as_mut().unwrap().reap()?;
            if self.channel.closed() {
                if self.result.is_none() {
                    return Err(self.expected.error("", "closed without a result"));
                }
                if !self.writer.drained(&self.channel) {
                    let what = match self.expected {
                        Expected::Manifest { .. } => "acknowledgement",
                        Expected::Maintenance => "request",
                    };
                    return Err(self
                        .expected
                        .error("", &format!("closed before {what} flushed")));
                }
                if self.deadline.is_some_and(|d| Instant::now() >= d) {
                    return Err(self
                        .expected
                        .error("", "did not finish within 2 seconds after its result"));
                }
                if reaped {
                    if !self.child.as_ref().unwrap().successful() {
                        return Err(self.expected.error("", "supervisor exited unsuccessfully"));
                    }
                    self.child.take(); // already reaped
                    return Ok(());
                }
            }
            wait(&self.channel);
        }
    }
}
impl Drop for Attempt {
    fn drop(&mut self) {
        // Dispose partial/raw capabilities before closing cancellation control.
        self.decoder.close(&mut self.channel);
        if let Some(mut child) = self.child.take() {
            // A decoded result's fixed deadline is never reset or extended.
            let deadline = self.deadline.unwrap_or_else(|| Instant::now() + EXIT_GRACE);
            while Instant::now() < deadline {
                match child.reap() {
                    Ok(true) => break,
                    Err(_) => break,
                    Ok(false) => std::thread::sleep(std::time::Duration::from_millis(10)),
                }
            }
            drop(child); // kill if necessary, then confirmed reap
        }
        self.writer.close(&mut self.channel);
        // result/extra owners are dropped only after the child above.
    }
}
fn wait(channel: &Channel) {
    if let Some(fd) = channel.fd() {
        let mut poll = libc::pollfd {
            fd,
            events: libc::POLLIN
                | if channel.wants_write() {
                    libc::POLLOUT
                } else {
                    0
                },
            revents: 0,
        };
        unsafe {
            libc::poll(&mut poll, 1, 10);
        }
    } else {
        // EOF can precede process destructors; the pidfd is sampled next turn.
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
}
