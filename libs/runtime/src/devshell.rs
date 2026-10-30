use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{
    collections::{BTreeMap, BTreeSet},
    path::{Path, PathBuf},
};

/// What a generation contains, for refresh diffs: the dev shell's direct
/// inputs by package name, and its derivation environment.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Summary {
    pub packages: BTreeMap<String, BTreeSet<String>>,
    pub env: BTreeMap<String, String>,
    pub derivation: PathBuf,
}

/// One host-approved dev shell generation, shared by a sandbox's children.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct DevShell {
    /// Host flake reference as given at launch, for display.
    pub reference: String,
    /// Host flake directory the workspace lock is read from.
    pub path: PathBuf,
    /// `devShells.<system>.<attr>` attribute name.
    pub attr: String,
    /// Starts at 1 for the launch snapshot.
    pub generation: u32,
    /// Store copy of the flake source that was evaluated (GC-rooted).
    pub source: PathBuf,
    /// The flake directory inside `source`.
    pub flake: PathBuf,
    /// The trusted `flake.lock`, parsed so formatting never matters.
    pub lock: Value,
    /// `nix print-dev-env` environment in the store; its closure is mounted.
    pub profile: PathBuf,
    /// Exported variables, excluding those ignored by `nix develop` and
    /// sandbox plumbing.
    pub variables: Vec<(String, String)>,
    /// Bash code reproducing the full environment, functions and shellHook.
    pub script: String,
    pub summary: Summary,
}
impl DevShell {
    /// Equal identities are the same trusted generation.
    pub fn identity(&self) -> String {
        identity(&self.flake, &self.attr, &self.lock)
    }
}
pub fn identity(flake: &Path, attr: &str, lock: &Value) -> String {
    format!("{}#{attr}\n{lock}", flake.display())
}

/// Inputs to update (`Some(empty)` means all) or new inputs to lock.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Changes {
    pub update: Option<Vec<String>>,
    pub lock: bool,
}

/// Split `PATH[#ATTR]` into an absolute flake directory and an attribute.
pub fn parse_reference(reference: &str) -> crate::Result<(PathBuf, Option<&str>)> {
    let (path, attr) = match reference.split_once('#') {
        Some((path, attr)) => (path, Some(attr)),
        None => (reference, None),
    };
    let path = Path::new(path);
    if !path.is_absolute() || path.as_os_str().to_str().is_none_or(|p| p.contains('?')) {
        return Err("dev shell must be an absolute local flake path".into());
    }
    if let Some(attr) = attr
        && (attr.is_empty()
            || attr.len() > 256
            || !attr
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"_-+'".contains(&b)))
    {
        return Err("invalid dev shell attribute".into());
    }
    Ok((path.to_path_buf(), attr))
}
