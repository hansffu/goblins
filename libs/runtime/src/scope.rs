//! Private scope process protocol. Descriptors use frame-local indices.
use crate::{channel::Descriptor, config::ScopeDefinition};
use serde::{Deserialize, Serialize};
use std::{collections::BTreeMap, path::PathBuf};

pub const API: u32 = 1;
pub const CONTROL_FD: i32 = 3;
/// Match the existing daemon CLI cleanup wait; scope Drop was unbounded.
pub const RELEASE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(15);
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Join {
    pub definition: ScopeDefinition,
    pub bwrap: PathBuf,
    pub helper: PathBuf,
    pub pasta: Option<PathBuf>,
    pub allow_nix: bool,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DockerConfig {
    pub bwrap: PathBuf,
    pub helper: PathBuf,
    pub daemon: PathBuf,
}
/// Each part has at most one capability, so transactions can exceed a frame's
/// descriptor budget without serializing host fd numbers into argument text.
#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "kind", deny_unknown_fields)]
pub enum DockerPart {
    Arguments {
        values: Vec<String>,
    },
    Bind {
        source: Descriptor,
        destination: PathBuf,
        readonly: bool,
    },
    Store {},
    Grant {
        source: Descriptor,
        destination: PathBuf,
        readonly: bool,
    },
    Lease {
        directory: PathBuf,
        lock: Descriptor,
    },
}
#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "method", content = "params", deny_unknown_fields)]
pub enum Message {
    Setup {
        api: u32,
        name: String,
        directory: PathBuf,
    },
    AddMember {
        fd: Descriptor,
    },
    Stop {},
    Stopping {},
    Idle {
        added: u64,
    },
    Join(Join),
    Joined {
        outer: Descriptor,
        user: Descriptor,
        net: Descriptor,
        pid: Option<Descriptor>,
        directory: PathBuf,
    },
    /// Storage capabilities are requested in bounded batches after Join.
    OpenStorage {
        names: Vec<String>,
    },
    Storage {
        directories: BTreeMap<String, Descriptor>,
    },
    RegisterMember {
        mountns: Descriptor,
    },
    Registered {},
    AttachDocker {
        config: Option<DockerConfig>,
        parts: Vec<DockerPart>,
        more: bool,
    },
    ExtendDocker {
        parts: Vec<DockerPart>,
        more: bool,
    },
    DockerPending {},
    DockerAttached {
        socket: PathBuf,
    },
    DetachDocker {},
    DockerDetached {},
    Leave {},
    Left {},
    Error {
        message: String,
    },
}
pub fn encode(message: &Message) -> serde_json::Value {
    let mut value = serde_json::to_value(message).expect("scope message is serializable");
    value
        .as_object_mut()
        .unwrap()
        .insert("jsonrpc".into(), "2.0".into());
    value
}
pub fn decode(mut value: serde_json::Value) -> std::io::Result<Message> {
    if value
        .as_object_mut()
        .and_then(|v| v.remove("jsonrpc"))
        .as_ref()
        != Some(&serde_json::json!("2.0"))
    {
        return Err(std::io::Error::other("invalid scope protocol envelope"));
    }
    serde_json::from_value(value).map_err(std::io::Error::other)
}

/// Collect only scope runtime state whose owning process has released its lock.
/// Storage under the user's cache is managed independently.
pub fn remove_stale(directory: &std::path::Path, dry_run: bool) -> crate::Result<bool> {
    use std::os::unix::fs::MetadataExt;
    use std::os::unix::fs::OpenOptionsExt;
    let parent = std::fs::File::options()
        .read(true)
        .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW)
        .open(directory.parent().ok_or("scope runtime has no parent")?)?;
    let name = directory.file_name().ok_or("scope runtime has no name")?;
    let selected = match goblins_unix::cleanup::open_at(&parent, name) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(true),
        Err(error) => return Err(error.into()),
    };
    let meta = selected.metadata()?;
    if !meta.is_dir() || meta.uid() != unsafe { libc::getuid() } || meta.mode() & 0o7777 != 0o700 {
        return Err(format!("unsafe scope runtime directory {}", directory.display()).into());
    }
    let Some(_lock) = crate::lease::flock_at(&selected, std::ffi::OsStr::new("lock"), true)? else {
        return Ok(false);
    };
    if !dry_run {
        goblins_unix::cleanup::remove_at(&parent, name)?;
    }
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{fs, os::unix::fs::PermissionsExt};
    #[test]
    fn runtime_collection_keeps_a_live_lock_and_respects_dry_run() {
        let _no_forks = goblins_unix::no_forks();
        let root =
            std::env::temp_dir().join(format!("goblins-runtime-state-{}", std::process::id()));
        fs::create_dir(&root).unwrap();
        fs::set_permissions(&root, fs::Permissions::from_mode(0o700)).unwrap();
        let lease = crate::lease::flock(&root.join("lock"), false)
            .unwrap()
            .unwrap();
        let keeper = crate::lease::flock(&root.join("lock"), false)
            .unwrap()
            .unwrap();
        fs::write(root.join("marker"), "scope runtime").unwrap();
        assert!(!remove_stale(&root, false).unwrap());
        assert!(root.join("marker").exists());
        drop(lease);
        assert!(!remove_stale(&root, false).unwrap());
        drop(keeper);
        assert!(remove_stale(&root, true).unwrap());
        assert!(root.join("marker").exists());
        assert!(remove_stale(&root, false).unwrap());
        assert!(!root.exists());
    }
}
