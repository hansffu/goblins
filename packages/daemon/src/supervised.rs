//! Daemon-wide executable selection. Child lifecycle moves here at cutover.
use std::path::{Path, PathBuf};

/// Captured once per controller, shared by every inherited-socket mode.
pub(crate) struct Executable(Option<PathBuf>);
impl Default for Executable {
    fn default() -> Self {
        Self::from_path(std::env::var_os("GOBLINS_SUPERVISOR_RUNTIME").map(PathBuf::from))
    }
}
impl Executable {
    pub(crate) fn from_path(path: Option<PathBuf>) -> Self {
        Self(path)
    }
    pub(crate) fn path(&self) -> crate::Result<&Path> {
        let path = self
            .0
            .as_deref()
            .filter(|p| !p.as_os_str().is_empty())
            .ok_or("supervisor executable not configured (GOBLINS_SUPERVISOR_RUNTIME)")?;
        if !path.is_absolute() {
            return Err(
                "supervisor executable must be an absolute path (GOBLINS_SUPERVISOR_RUNTIME)"
                    .into(),
            );
        }
        Ok(path)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn executable_requires_an_explicit_absolute_pin_without_path_search() {
        for path in [None, Some(PathBuf::new())] {
            assert!(
                Executable::from_path(path)
                    .path()
                    .unwrap_err()
                    .to_string()
                    .contains("supervisor executable not configured")
            );
        }
        assert!(
            Executable::from_path(Some("goblins-supervisor-bwrap".into()))
                .path()
                .is_err()
        );
        let pin = Executable::from_path(Some("/immutable/supervisor".into()));
        assert_eq!(pin.path().unwrap(), Path::new("/immutable/supervisor"));
    }
}
