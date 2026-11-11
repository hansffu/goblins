//! One-shot maintenance. Real stdio is explicit authority, not ambient state.
use crate::{
    Result,
    cleanup::{self, Context},
    process::Cancellation,
};
use goblins_protocol::contract::supervisor_phases::MaintenanceFirst;
use goblins_runtime::{
    channel::{Channel, Frame},
    supervisor::{Decoder, Encoder, MaintenanceOperation, Message, outbound::Writer},
};
use std::fs::File;
pub(crate) fn start(mut channel: Channel, first: Frame) -> Result<()> {
    let mut decoder = Decoder::default();
    let mut writer = Writer::default();
    let result = (|| {
        if !first
            .value
            .get("method")
            .and_then(|v| v.as_str())
            .is_some_and(MaintenanceFirst::allows)
        {
            drop(first);
            decoder.close(&mut channel);
            return Err("Maintenance must be first".into());
        }
        let received = decoder.receive(first)?.ok_or("incomplete Maintenance")?;
        let Message::Maintenance(request) = received.message else {
            return Err("Maintenance must be first".into());
        };
        let cancel =
            Cancellation::until_readable(channel.fd().ok_or("maintenance control closed")?)?;
        cancel.check()?; // before cache creation, locking or any deletion
        let mut output = File::from(std::os::fd::OwnedFd::from(request.stdout));
        let error = File::from(std::os::fd::OwnedFd::from(request.stderr));
        let mut context = Context {
            output: &mut output,
            error: &error,
            cancel: &cancel,
        };
        let result = match request.operation {
            MaintenanceOperation::Gc {
                runtime,
                state,
                dry_run,
            } => cleanup::gc(runtime.as_deref(), &state, dry_run, &mut context),
            MaintenanceOperation::ClearScope {
                runtime,
                name,
                dry_run,
            } => cleanup::clear_scope(runtime.as_deref(), name.as_deref(), dry_run, &mut context),
        }
        .and_then(|()| std::io::Write::flush(&mut output).map_err(Into::into));
        drop(output);
        drop(error);
        cancel.check()?;
        let mut encoder = Encoder::default();
        writer
            .begin(encoder.prepare(Message::MaintenanceFinished {
                result: result.map_err(|e| e.to_string()),
            })?)
            .map_err(|_| "maintenance writer busy")?;
        while !writer.idle(&channel) {
            writer.pump(&mut channel)?;
            if channel.closed() {
                return Err("maintenance control closed".into());
            }
            if !writer.idle(&channel) {
                crate::entry::wait(&channel);
            }
        }
        crate::preflight::terminal_input(&channel, "maintenance")?;
        Ok(())
    })();
    decoder.close(&mut channel);
    writer.close(&mut channel);
    result
}
#[cfg(test)]
mod c16c_tests {
    use super::*;
    use serde_json::json;
    use std::{io::Read, os::fd::AsRawFd, os::unix::net::UnixStream};

    #[test]
    fn c16c_maintenance_first_guard_and_decoder_failure_close_raw_fds_and_channel() {
        let _no_forks = goblins_unix::no_forks();
        for method in [
            None,
            Some(json!(null)),
            Some(json!(1)),
            Some(json!("Command.Start")),
            Some(json!("Preflight")),
            Some(json!("maintenance")),
            Some(json!("Maintenance")),
        ] {
            let (left, mut peer) = UnixStream::pair().unwrap();
            let channel = Channel::new(left).unwrap();
            let (reader, writer) = goblins_unix::pipe().unwrap();
            let mut reader = std::fs::File::from(reader);
            let correct = method.as_ref() == Some(&json!("Maintenance"));
            let mut value = json!({"jsonrpc":"2.0","params":{}});
            if let Some(method) = method {
                value["method"] = method;
            }
            let error = start(
                channel,
                Frame {
                    value,
                    fds: vec![writer],
                },
            )
            .unwrap_err();
            assert_eq!(
                error.to_string(),
                if correct {
                    "missing control field supervisor_api"
                } else {
                    "Maintenance must be first"
                }
            );
            assert!(goblins_unix::readable(reader.as_raw_fd(), 100).unwrap());
            assert_eq!(reader.read(&mut [0]).unwrap(), 0);
            assert!(goblins_unix::readable(peer.as_raw_fd(), 100).unwrap());
            assert_eq!(peer.read(&mut [0]).unwrap(), 0);
        }
    }
}
