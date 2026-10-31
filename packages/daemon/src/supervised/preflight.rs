//! Startup-only preflight over the common one-shot client.
use super::{
    Executable,
    oneshot::{self, Expected},
};
use crate::Result;
use goblins_runtime::{
    config::ManifestLease,
    supervisor::{self, Message},
};
use std::path::Path;
pub(crate) fn lease(
    executable: &Executable,
    path: &Path,
    directory: &Path,
    sequence: u64,
) -> Result<ManifestLease> {
    let message = oneshot::request(
        executable,
        Message::Preflight {
            supervisor_api: supervisor::API,
            path: path.into(),
            directory: directory.into(),
            sequence,
        },
        Expected::Manifest {
            path: path.into(),
            sequence,
        },
        || false,
    )?;
    match message {
        Message::Event(goblins_runtime::Event::Manifest { lease, .. }) => Ok(lease),
        Message::PreflightFailed { error } => Err(error.into()),
        _ => unreachable!("validated one-shot result"),
    }
}
#[cfg(test)]
use goblins_runtime::{
    Event,
    channel::Channel,
    supervisor::{Decoder, Encoder, outbound::Writer},
};
#[cfg(test)]
use std::time::Instant;
#[cfg(test)]
pub(crate) mod tests;
