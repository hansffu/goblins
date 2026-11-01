//! One-shot maintenance with explicit real-output capabilities.
use super::{
    Executable,
    oneshot::{self, Expected},
};
use crate::Result;
use goblins_runtime::supervisor::{self, Maintenance, MaintenanceOperation, Message};
use std::os::fd::{BorrowedFd, OwnedFd};
pub(crate) fn run(operation: MaintenanceOperation, cancel: impl Fn() -> bool) -> Result<()> {
    let duplicate = |fd| -> Result<OwnedFd> {
        use std::os::fd::AsFd;
        Ok(unsafe { BorrowedFd::borrow_raw(fd) }
            .as_fd()
            .try_clone_to_owned()?)
    };
    let executable = Executable::default();
    let message = Message::Maintenance(Maintenance {
        supervisor_api: supervisor::API,
        operation,
        stdout: duplicate(libc::STDOUT_FILENO)?.into(),
        stderr: duplicate(libc::STDERR_FILENO)?.into(),
    });
    match oneshot::request(&executable, message, Expected::Maintenance, cancel)? {
        Message::MaintenanceFinished { result } => result.map_err(Into::into),
        _ => unreachable!("validated maintenance result"),
    }
}

#[cfg(test)]
pub(crate) mod tests;
