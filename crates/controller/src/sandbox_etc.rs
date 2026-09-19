//! Immutable /etc files supplied by the trusted Nix launch configuration.
//! These are mounted inside bwrap only; no host destination is created.
use crate::{Result, session::store_path};
use std::{collections::BTreeMap, path::PathBuf};

pub fn mounts(files: &BTreeMap<String, PathBuf>) -> Result<Vec<(PathBuf, PathBuf)>> {
    if files.len() > 128 {
        return Err("at most 128 sandboxEtc files are supported".into());
    }
    let mut mounts: Vec<(PathBuf, PathBuf)> = vec![];
    for (name, source) in files {
        if name.len() > 512
            || !name.split('/').all(|part| {
                !part.is_empty()
                    && part != "."
                    && part != ".."
                    && part.bytes().enumerate().all(|(i, b)| {
                        b.is_ascii_alphanumeric() || b == b'_' || b == b'-' || (i > 0 && b == b'.')
                    })
            })
        {
            return Err("sandboxEtc names must be relative /etc file paths".into());
        }
        // Require a complete immutable store object, not an arbitrary host file
        // or a path that could escape through a symlink within a derivation.
        store_path(source)?;
        let meta = std::fs::symlink_metadata(source)?;
        if !meta.is_file() {
            return Err("sandboxEtc sources must be regular Nix store files".into());
        }
        let destination = PathBuf::from("/etc").join(name);
        if mounts
            .iter()
            .any(|(_, other)| destination.starts_with(other) || other.starts_with(&destination))
        {
            return Err("sandboxEtc file destinations overlap".into());
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
            assert!(mounts(&BTreeMap::from([(name.into(), "/tmp/source".into())])).is_err());
        }
        assert!(
            mounts(&BTreeMap::from([(
                "codex/config.toml".into(),
                "/tmp/source".into()
            )]))
            .is_err()
        );
    }
}
