//! Startup-only one-shot client. Never called from the daemon event pump.
use super::{Child, EXIT_GRACE, Executable};
use crate::Result;
use goblins_runtime::{
    Event,
    channel::Channel,
    config::ManifestLease,
    supervisor::{self, Decoder, Encoder, Message, Received, outbound::Writer},
};
use std::{path::Path, time::Instant};

pub(crate) fn lease(
    executable: &Executable,
    path: &Path,
    directory: &Path,
    sequence: u64,
) -> Result<ManifestLease> {
    let (child, socket) = Child::start(executable.path()?)?;
    let mut attempt = Attempt {
        child: Some(child),
        channel: Channel::new(socket)?,
        decoder: Decoder::default(),
        encoder: Encoder::default(),
        writer: Writer::default(),
        result: None,
        extra: None,
        deadline: None,
    };
    attempt
        .writer
        .begin(attempt.encoder.prepare(Message::Preflight {
            supervisor_api: supervisor::API,
            path: path.into(),
            directory: directory.into(),
            sequence,
        })?)
        .map_err(|_| "preflight writer busy")?;
    attempt.exchange(path, sequence)?;
    // Successful reap and strict EOF precede publication or the operation error.
    match attempt.result.take().unwrap() {
        Message::Event(Event::Manifest { lease, .. }) => Ok(lease),
        Message::PreflightFailed { error } => Err(error.into()),
        _ => unreachable!("validated one-shot result"),
    }
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
            Some("Event.Manifest" | "PreflightFailed")
        ) {
            // Both legal results are single-frame. Reject a wrong direction
            // before the general decoder could retain a partial Handoff and
            // leave us waiting indefinitely for a complete logical result.
            drop(frame);
            self.decoder.close(&mut self.channel);
            return Err("invalid preflight result method".into());
        }
        let received = self
            .decoder
            .receive(frame)?
            .ok_or("incomplete preflight result")?;
        Ok(Some(received))
    }
    fn exchange(&mut self, path: &Path, sequence: u64) -> Result<()> {
        loop {
            if self.deadline.is_some_and(|d| Instant::now() >= d) {
                return Err("preflight did not finish within 2 seconds after its result".into());
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
                    return Err("extra preflight result".into());
                }
                self.deadline = Some(Instant::now() + EXIT_GRACE);
                self.result = Some(received.message);
                match self.result.as_ref().unwrap() {
                    Message::Event(Event::Manifest {
                        sequence: actual,
                        lease,
                    }) if *actual == sequence && lease.path == path => {
                        let transfer = received
                            .adopted
                            .ok_or("preflight manifest lacks adoption")?;
                        self.writer
                            .begin(self.encoder.prepare(Message::Adopted { transfer })?)
                            .map_err(|_| "preflight acknowledgement writer busy")?;
                    }
                    Message::PreflightFailed { .. } if received.adopted.is_none() => (),
                    _ => return Err("invalid preflight result".into()),
                }
            }
            let reaped = self.child.as_mut().unwrap().reap()?;
            if self.channel.closed() {
                if self.result.is_none() {
                    return Err("preflight closed without a result".into());
                }
                if !self.writer.drained(&self.channel) {
                    return Err("preflight closed before acknowledgement flushed".into());
                }
                if self.deadline.is_some_and(|d| Instant::now() >= d) {
                    return Err("preflight did not finish within 2 seconds after its result".into());
                }
                if reaped {
                    if !self.child.as_ref().unwrap().successful() {
                        return Err("preflight supervisor exited unsuccessfully".into());
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

#[cfg(test)]
pub(crate) mod tests;
