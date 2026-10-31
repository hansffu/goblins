//! Linux runtime engine and inherited-socket executable dispatch. Local and
//! the maintenance facade remains during the phase 6 cutover.
pub type Result<T> = std::result::Result<T, Box<dyn std::error::Error + Send + Sync>>;
mod binds;
mod catalog;
mod cleanup;
mod config;
mod devshell;
mod docker;
mod engine;
mod entry;
mod evaluator;
mod local;
mod logging;
pub mod maintenance;
mod network;
mod preflight;
mod process;
mod relay;
mod results;
mod sandbox_etc;
mod scope_client;
mod scope_docker;
mod scope_keeper;
mod scope_process;
mod supervisor_process;
pub use entry::main as control_main;
pub use local::Local;
mod scope_storage;
mod seccomp;
mod server_log;
mod session;
mod snapshot;
mod terminal;
mod terminals;
mod worker;
// Legacy descriptor fixtures travel with the ignored Session driver.
#[cfg(test)]
#[allow(dead_code)]
mod unix;
