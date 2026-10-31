//! Private same-build init wire data, shared for canonical exec validation.
//! This is not a public JSON-RPC endpoint and carries no host authority.
use serde::{Deserialize, Serialize};

pub const MAX_MESSAGE: usize = 64 * 1024;
/// A spawn carries one PTY slave or stdin, stdout and stderr.
pub use crate::HOST_MAX_FDS as MAX_FDS;

/// Supervisor → init. Exec IDs are chosen by the daemon and never reused.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "method", content = "params", rename_all = "kebab-case")]
pub enum Command {
    /// Run `argv` (the sandbox's `$SHELL` when absent) in its own session and
    /// process group. Descriptors: the PTY slave with `tty`, else stdin,
    /// stdout and stderr.
    Spawn {
        id: u64,
        argv: Option<Vec<String>>,
        cwd: Option<String>,
        env: Vec<(String, String)>,
        tty: bool,
    },
    /// Forward a catchable signal to the exec's process group.
    Signal { id: u64, signal: i32 },
    /// SIGHUP and SIGTERM to the exec's process group, or with `everything`
    /// to every process its keeper holds, then SIGKILL after a grace period.
    Cancel {
        id: u64,
        #[serde(default)]
        everything: bool,
    },
    /// Cancel every exec. The agent is not affected.
    KillAll {},
}

/// Init → supervisor, reported to the daemon as runtime events.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "method", content = "params", rename_all = "kebab-case")]
pub enum Event {
    Spawned {
        id: u64,
        pid: i32,
    },
    /// The command could not start. `code` is the exit code a shell would
    /// report: 127 for a missing command, 126 otherwise.
    Failed {
        id: u64,
        code: i32,
        message: String,
    },
    Exited {
        id: u64,
        code: Option<i32>,
        signal: Option<i32>,
    },
}

/// Signals a host client may forward to an exec.
pub fn forwardable(signal: i32) -> bool {
    [libc::SIGHUP, libc::SIGINT, libc::SIGTERM].contains(&signal)
}
