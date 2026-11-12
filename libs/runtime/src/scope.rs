//! Private scope process protocol. Descriptors use frame-local indices.
pub use goblins_protocol::contract::scope_metadata::{DockerConfig, DockerPart, Join, Message};

pub const API: u32 = 1;
pub const CONTROL_FD: i32 = 3;
/// Match the existing daemon CLI cleanup wait; scope Drop was unbounded.
pub const RELEASE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(15);
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

#[cfg(test)]
mod migration_happy_path_tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn remaining_migration_scope_normal_frames_roundtrip() {
        let definition = json!({"persistent":true,"pid":true,"storage":["cache"],"docker":true});
        let config = json!({"bwrap":"/bin/bwrap","helper":"/bin/helper","daemon":"/bin/docker"});
        let parts = json!([
            {"kind":"Arguments","values":["--debug"]},
            {"kind":"Bind","source":{"$fd":0},"destination":"/workspace","readonly":false},
            {"kind":"Store"},
            {"kind":"Grant","source":{"$fd":1},"destination":"/grant","readonly":true},
            {"kind":"Lease","directory":"/resource","lock":{"$fd":2}}
        ]);
        let cases = [
            ("Setup",json!({"api":1,"name":"shared","directory":"/runtime"})),
            ("AddMember",json!({"fd":{"$fd":0}})),
            ("Stop",json!({})), ("Stopping",json!({})),
            ("Idle",json!({"added":3})),
            ("Join",json!({"definition":definition,"bwrap":"/bin/bwrap","helper":"/bin/helper","pasta":"/bin/pasta","allow_nix":true})),
            ("Join",json!({"definition":{"persistent":false,"pid":false,"storage":[],"docker":false},"bwrap":"/bin/bwrap","helper":"/bin/helper","pasta":null,"allow_nix":false})),
            ("Joined",json!({"outer":{"$fd":0},"user":{"$fd":1},"net":{"$fd":2},"pid":{"$fd":3},"directory":"/runtime"})),
            ("Joined",json!({"outer":{"$fd":0},"user":{"$fd":1},"net":{"$fd":2},"pid":null,"directory":"/runtime"})),
            ("OpenStorage",json!({"names":["cache","data"]})),
            ("Storage",json!({"directories":{"cache":{"$fd":0},"data":{"$fd":1}}})),
            ("RegisterMember",json!({"mountns":{"$fd":0}})), ("Registered",json!({})),
            ("AttachDocker",json!({"config":config,"parts":parts,"more":true})),
            ("AttachDocker",json!({"config":null,"parts":[],"more":false})),
            ("ExtendDocker",json!({"parts":[{"kind":"Store"}],"more":false})),
            ("DockerPending",json!({})), ("DockerAttached",json!({"socket":"/docker.sock"})),
            ("DetachDocker",json!({})), ("DockerDetached",json!({})),
            ("Leave",json!({})), ("Left",json!({})),
            ("Error",json!({"message":"scope stopped"})),
        ];
        for (method,params) in cases {
            let expected=json!({"jsonrpc":"2.0","method":method,"params":params});
            let message=decode(expected.clone()).unwrap();
            assert_eq!(encode(&message),expected,"{method}");
        }
    }
}
