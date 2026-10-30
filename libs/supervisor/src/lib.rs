//! Linux runtime implementation. Only Local and the temporary preflight and
//! maintenance facades are public; phase 6 replaces the facades before unlinking.
pub type Result<T> = std::result::Result<T, Box<dyn std::error::Error + Send + Sync>>;
mod binds;
mod catalog;
mod cleanup;
mod config;
mod devshell;
mod docker;
mod evaluator;
mod local;
pub mod maintenance;
pub mod manifest;
mod network;
mod process;
mod sandbox_etc;
mod scope;
mod scope_keeper;
mod scope_storage;
mod seccomp;
mod session;
mod snapshot;
mod worker;
// Legacy descriptor fixtures travel with the ignored Session driver.
#[cfg(test)]
#[allow(dead_code)]
mod unix;
use crate::local::HostJob;
use crate::local::Published;
use crate::process::Cancellation as Cancel;
use crate::worker::Worker;
use goblins_runtime::policy::{Policy, Support};
use goblins_runtime::{Event, Handoff};
use std::{
    collections::{BTreeMap, VecDeque},
    fs::File,
    os::{fd::OwnedFd, unix::net::UnixListener},
    path::PathBuf,
    sync::mpsc,
};

/// The runtime inside the daemon: a worker thread for Nix-bound work, with
/// `send` as the dispatcher that never waits for it, and the running
/// sandbox's init channel, request socket, exit watch and host jobs.
pub struct Local {
    worker: Worker,
    /// Tokens of cancellable work sent to the worker, by cancel id.
    cancels: BTreeMap<u64, Cancel>,
    /// The private channel to the sandbox's goblins-init, from `Started`.
    init: Option<OwnedFd>,
    /// The sandbox's `request.sock`, from `Started` until it stops.
    listener: Option<UnixListener>,
    /// The helper's pidfd, until the payload's exit is reported.
    exit_watch: Option<OwnedFd>,
    /// PTYs allocated for processes init has not reported started.
    masters: BTreeMap<u64, OwnedFd>,
    buffer: Vec<u8>,
    /// The goblin's capability, published by its worker at `Started` and by
    /// each dev shell refresh: it resolves host directories through the
    /// sandbox's binds and is what an export hands over.
    published: Published,
    /// Where a member's `Start` goes, to the worker that waits for it.
    start: Option<mpsc::SyncSender<Handoff>>,
    /// What the goblin may do, from `Started`.
    capabilities: Option<(Policy, Support)>,
    /// Where host jobs keep their output and outcome.
    host_jobs: PathBuf,
    /// Pinned directories of host commands awaiting approval, by request.
    prepared: BTreeMap<String, File>,
    jobs: Vec<HostJob>,
    stopping: bool,
    /// Events reported and not yet polled.
    events: VecDeque<Event>,
    /// A round is being polled: its sources were collected.
    polling: bool,
    ended: bool,
}
