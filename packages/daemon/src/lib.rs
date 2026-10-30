//! Host-only lifecycle and policy. No code here opens or reads /dev/tty.
pub mod controller;
pub mod history;
pub mod raid;
pub type Result<T> = std::result::Result<T, Box<dyn std::error::Error + Send + Sync>>;

pub mod host;
pub mod mailbox;
pub mod messaging;
pub mod operation;

mod scopes;
