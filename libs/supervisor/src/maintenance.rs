//! One-shot maintenance. Real stdio is explicit authority, not ambient state.
use crate::{
    Result,
    cleanup::{self, Context},
    process::Cancellation,
};
use goblins_runtime::{
    channel::{Channel, Frame},
    supervisor::{Decoder, Encoder, MaintenanceOperation, Message, outbound::Writer},
};
use std::fs::File;
pub(crate) fn start(mut channel: Channel, first: Frame) -> Result<()> {
    let mut decoder = Decoder::default();
    let mut writer = Writer::default();
    let result = (|| {
        if first.value.get("method").and_then(|v| v.as_str()) != Some("Maintenance") {
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
