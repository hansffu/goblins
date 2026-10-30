//! Temporary manifest preflight entry points; phase 6 replaces them.
use crate::Result;
use goblins_runtime::config::{Configuration, Manifest, ManifestLease};
use std::path::Path;
pub fn read(path: &Path) -> Result<Manifest> {
    crate::config::read(path)
}
pub fn select_configuration(
    manifest: Manifest,
    name: &str,
    scope: Option<&str>,
) -> Result<Configuration> {
    crate::config::select(manifest, name, scope)
}
pub fn allows_nix(config: &Configuration) -> Result<bool> {
    crate::config::allows_nix(config)
}
pub fn lease(path: &Path, directory: &Path, sequence: u64) -> Result<ManifestLease> {
    crate::config::lease_manifest(
        path,
        directory,
        sequence,
        &crate::process::Cancellation::default(),
    )
}
