//! Host-only lifecycle and policy. No code here opens or reads /dev/tty.
pub mod binds;
pub mod catalog;
pub mod config;
pub mod controller;
mod process;
mod seccomp;
pub mod session;
mod snapshot;
pub mod unix;
pub type Result<T> = std::result::Result<T, Box<dyn std::error::Error + Send + Sync>>;

mod worker;

pub mod host;
pub mod mailbox;
mod messaging;
mod terminal;
