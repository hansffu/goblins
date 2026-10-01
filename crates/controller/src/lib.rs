//! Host-only lifecycle and policy. No code here opens or reads /dev/tty.
pub mod binds;
mod branch;
pub mod catalog;
pub mod config;
pub mod controller;
pub mod devshell;
mod docker;
pub mod evaluator;
mod network;
mod process;
pub mod raid;
mod scope;
mod scope_keeper;
mod scope_storage;
mod seccomp;
pub mod session;
mod snapshot;
pub mod unix;
pub type Result<T> = std::result::Result<T, Box<dyn std::error::Error + Send + Sync>>;

mod worker;

pub mod host;
pub mod mailbox;
pub mod messaging;
pub mod operation;
mod sandbox_etc;
mod terminal;

pub mod integration;
