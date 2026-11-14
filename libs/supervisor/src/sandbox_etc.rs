//! Immutable /etc files supplied by the trusted Nix launch configuration.
//! These are mounted inside bwrap only; no host destination is created.
use crate::Result;
use crate::session::store_path;
use std::{collections::BTreeMap, path::PathBuf};

/// `dev_shell` holds the files mounted only in a dev shell sandbox; they may
/// not share a destination with `files`.
pub fn mounts(
    files: &BTreeMap<String, PathBuf>,
    dev_shell: &BTreeMap<String, PathBuf>,
) -> Result<Vec<(PathBuf, PathBuf)>> {
    if files.len() + dev_shell.len() > 128 {
        return Err("at most 128 injectedFiles files are supported".into());
    }
    let mut mounts: Vec<(PathBuf, PathBuf)> = vec![];
    for (name, source) in files.iter().chain(dev_shell) {
        if name.len() > 512
            || !name.split('/').all(|part| {
                !part.is_empty()
                    && part != "."
                    && part != ".."
                    && part
                        .bytes()
                        .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-' || b == b'.')
            })
        {
            return Err("injectedFiles names must be relative /etc file paths".into());
        }
        // Require a complete immutable store object, not an arbitrary host file
        // or a path that could escape through a symlink within a derivation.
        store_path(source)?;
        let meta = std::fs::symlink_metadata(source)?;
        if !meta.is_file() {
            return Err("injectedFiles sources must be regular Nix store files".into());
        }
        let destination = PathBuf::from("/etc").join(name);
        if mounts
            .iter()
            .any(|(_, other)| destination.starts_with(other) || other.starts_with(&destination))
        {
            return Err("injectedFiles file destinations overlap".into());
        }
        mounts.push((source.clone(), destination));
    }
    Ok(mounts)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_escaping_paths_and_non_store_sources() {
        for name in [
            "",
            "/tmp/x",
            "../x",
            "codex/../../x",
            "codex//x",
            "codex/./x",
        ] {
            let files = BTreeMap::from([(name.into(), "/tmp/source".into())]);
            assert!(mounts(&files, &BTreeMap::new()).is_err());
            assert!(mounts(&BTreeMap::new(), &files).is_err());
        }
        assert!(
            mounts(
                &BTreeMap::from([("codex/config.toml".into(), "/tmp/source".into())]),
                &BTreeMap::new()
            )
            .is_err()
        );
    }

    #[test]
    fn dev_shell_files_are_mounted_only_with_a_dev_shell() {
        let skill = "claude-code/.claude/skills/goblins-devshell/SKILL.md";
        let mut launch: crate::config::Launch = serde_json::from_value(serde_json::json!({
            "shell": "/unused", "posix_shell": "/unused", "helper": "/unused",
            "bwrap": "/unused", "flake": "unused", "client_package": "/unused",
            "dev_shell_sandbox_etc": {skill: "/nix/store/0123456789abcdfghijklmnpqrsvwxyz-SKILL.md"},
        }))
        .unwrap();
        assert!(launch_dev_shell_files(&launch).is_empty());
        launch.dev_shell = Some(
            serde_json::from_value(serde_json::json!({
                "reference": "/work", "path": "/work", "attr": "default", "generation": 1,
                "source": "/nix/store/x", "flake": "/nix/store/x", "lock": {},
                "profile": "/nix/store/y", "variables": [], "script": "",
                "summary": {"packages": {}, "env": {}, "derivation": "/nix/store/z"},
            }))
            .unwrap(),
        );
        assert_eq!(
            launch_dev_shell_files(&launch).keys().collect::<Vec<_>>(),
            [skill]
        );
    }
}

pub(crate) fn launch_mounts(
    launch: &crate::config::Launch,
) -> crate::Result<Vec<(PathBuf, PathBuf)>> {
    mounts(&launch.sandbox_etc, launch_dev_shell_files(launch))
}

/// The dev shell files a launch mounts: all of them in a sandbox with a dev
/// shell, whether launched with one or a member inheriting it, else none.
fn launch_dev_shell_files(launch: &crate::config::Launch) -> &BTreeMap<String, PathBuf> {
    static NONE: BTreeMap<String, PathBuf> = BTreeMap::new();
    match launch.dev_shell {
        Some(_) => &launch.dev_shell_sandbox_etc,
        None => &NONE,
    }
}
