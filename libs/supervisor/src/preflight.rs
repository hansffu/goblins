//! One-shot validation/rooting before the daemon begins serving requests.
use crate::Result;
use goblins_runtime::{
    Event,
    channel::{Channel, Frame},
    supervisor::{Decoder, Encoder, Message, outbound::Writer},
};

pub(crate) fn start(mut channel: Channel, first: Frame) -> Result<()> {
    let mut decoder = Decoder::default();
    let mut writer = Writer::default();
    let result = (|| {
        let received = decoder.receive(first)?.ok_or("incomplete Preflight")?;
        let Message::Preflight {
            path,
            directory,
            sequence,
            ..
        } = received.message
        else {
            return Err("Preflight must be first".into());
        };
        let cancel = crate::process::Cancellation::until_readable(
            channel.fd().ok_or("preflight control closed")?,
        )?;
        let lease = crate::config::lease_manifest(&path, &directory, sequence, &cancel);
        cancel.check()?;
        let message = match lease {
            Ok((lease, catalogue)) => Message::Event(Event::Manifest {
                sequence,
                lease,
                catalogue,
            }),
            Err(error) => Message::PreflightFailed {
                error: error.to_string(),
            },
        };
        let mut encoder = Encoder::default();
        let prepared = encoder.prepare(message)?;
        let adoption = prepared.needs_adoption();
        writer
            .begin(prepared)
            .map_err(|_| "preflight writer busy")?;
        while !writer.idle(&channel) {
            writer.pump(&mut channel)?;
            if channel.closed() {
                return Err("preflight control closed".into());
            }
            if !writer.idle(&channel) {
                crate::entry::wait(&channel);
            }
        }
        if adoption {
            loop {
                if let Some(frame) = channel.receive()? {
                    // Adopted is scalar. A wrong-direction multipart Handoff
                    // must never enter the decoder's pending receive state.
                    if frame.value.get("method").and_then(|v| v.as_str()) != Some("Adopted") {
                        drop(frame);
                        decoder.close(&mut channel);
                        return Err("preflight expected Adopted".into());
                    }
                    let received = decoder.receive(frame)?.ok_or("incomplete Adopted")?;
                    let Message::Adopted { transfer } = received.message else {
                        return Err("preflight expected Adopted".into());
                    };
                    writer.adopted(transfer)?;
                    break;
                }
                if channel.closed() {
                    return Err("preflight closed before Adopted".into());
                }
                crate::entry::wait(&channel);
            }
        }
        terminal_input(&channel, "preflight")?;
        Ok(())
    })();
    decoder.close(&mut channel);
    writer.close(&mut channel);
    result
}
/// ACK (or a failure result) ends the child transcript at this nonblocking
/// check. Reject bytes already queued, including a partial extra frame; do not
/// wait for parent EOF or claim to reject bytes arriving after this boundary.
/// Channel consumes exactly one frame without reading ahead across frames.
pub(crate) fn terminal_input(channel: &Channel, mode: &str) -> Result<()> {
    let fd = channel
        .fd()
        .ok_or_else(|| format!("{mode} control closed"))?;
    let mut byte = 0u8;
    let n = unsafe {
        libc::recv(
            fd,
            (&mut byte as *mut u8).cast(),
            1,
            libc::MSG_PEEK | libc::MSG_DONTWAIT,
        )
    };
    if n > 0 {
        return Err(format!("trailing {mode} input").into());
    }
    if n < 0 {
        let error = std::io::Error::last_os_error();
        if error.kind() != std::io::ErrorKind::WouldBlock {
            return Err(error.into());
        }
    }
    Ok(())
}
