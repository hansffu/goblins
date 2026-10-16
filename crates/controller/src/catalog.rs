//! Pinned host Nix lookup and advisory pre-approval cost estimates. A preview
//! evaluates metadata and runs --dry-run; it never realizes or mounts a package.
use crate::{
    Result,
    process::{self, Cancellation},
};
use goblins_protocol::package_name;
use serde::{Deserialize, Serialize};
use std::{
    path::{Path, PathBuf},
    process::Command,
};

#[derive(Clone, Debug, Serialize)]
pub struct Preview {
    #[serde(skip)]
    pub output_path: Option<PathBuf>,
    pub in_store: bool,
    /// Nix's compressed transfer estimate for the missing closure. This is not
    /// the unpacked NAR size, and remains advisory until the actual build.
    pub download: Option<String>,
    pub build_required: bool,
}
#[derive(Deserialize)]
pub struct Selection {
    pub output: String,
    pub path: PathBuf,
}
pub fn nix() -> Command {
    let mut c = Command::new("nix");
    c.args(["--extra-experimental-features", "nix-command flakes"]);
    c
}
pub fn attribute(flake: &str, name: &str) -> String {
    format!("{flake}#legacyPackages.x86_64-linux.{name}")
}
pub fn select(
    flake: &str,
    name: &str,
    directory: &Path,
    cancel: &Cancellation,
    allow_build: bool,
) -> Result<Selection> {
    if !package_name(name) {
        return Err("invalid package attribute".into());
    }
    let selection = "p: if builtins.isAttrs p && (p.type or null) == \"derivation\" then let q = p.bin or p; in { output = q.outputName; path = q.outPath; } else throw \"attribute is not a package derivation\"";
    let raw = process::command(
        nix().args([
            "eval",
            "--no-write-lock-file",
            "--option",
            "allow-import-from-derivation",
            if allow_build { "true" } else { "false" },
            "--json",
            &attribute(flake, name),
            "--apply",
            selection,
        ]),
        directory,
        cancel,
    )
    .map_err(|e| format!("cannot resolve package '{name}' from pinned nixpkgs: {e}"))?;
    let selected: Selection = serde_json::from_str(&raw)?;
    if selected.output.is_empty()
        || !selected
            .output
            .bytes()
            .enumerate()
            .all(|(i, b)| b.is_ascii_alphabetic() || b == b'_' || i > 0 && b.is_ascii_digit())
    {
        return Err("package has an unsupported output name".into());
    }
    Ok(selected)
}
/// Whether a `select` error means the name no longer names a package in
/// the catalog, as opposed to a failure to evaluate, fetch or build it.
pub fn no_longer_a_package(error: &str) -> bool {
    [
        "invalid package attribute",
        "package has an unsupported output name",
        // Our selection's own throw.
        "attribute is not a package derivation",
        // Nix, for an attribute path the flake lacks.
        "does not provide attribute",
        // nixpkgs aliases.nix, for a package that was removed.
        "has been removed",
    ]
    .iter()
    .any(|pattern| error.contains(pattern))
}
pub fn preview(
    flake: &str,
    name: &str,
    directory: &Path,
    cancel: &Cancellation,
) -> Result<Preview> {
    let selection = select(flake, name, directory, cancel, false)?;
    if selection.path.exists() {
        crate::session::store_path(&selection.path)?;
        return Ok(Preview {
            output_path: Some(selection.path),
            in_store: true,
            download: Some("0 B".into()),
            build_required: false,
        });
    }
    // Keep the pinned CLI's summary in the C locale. Its JSON lists output
    // paths but does not include transfer sizes; the raw summary does.
    process::command(
        nix().env("LC_ALL", "C").env("NO_COLOR", "1").args([
            "build",
            "--dry-run",
            "--no-link",
            "--no-write-lock-file",
            "--log-format",
            "raw",
            "--option",
            "allow-import-from-derivation",
            "false",
            &format!("{}^{}", attribute(flake, name), selection.output),
        ]),
        directory,
        cancel,
    )?;
    let mut preview = parse_summary(&std::fs::read_to_string(directory.join("command.err"))?);
    preview.output_path = Some(selection.path);
    Ok(preview)
}
fn parse_summary(text: &str) -> Preview {
    let mut download = None;
    let mut build_required = false;
    for line in text.lines() {
        build_required |= line.contains("will be built:");
        if let Some((_, rest)) = line.split_once("will be fetched (")
            && let Some((size, _)) = rest.split_once(" download,")
        {
            // Fail to Unknown if a later Nix version changes its summary.
            let parts: Vec<_> = size.split_whitespace().collect();
            if parts.len() == 2
                && parts[0]
                    .parse::<f64>()
                    .is_ok_and(|v| v.is_finite() && v >= 0.0)
                && ["B", "KiB", "MiB", "GiB", "TiB"].contains(&parts[1])
            {
                download = Some(size.to_string());
            }
        }
    }
    Preview {
        output_path: None,
        in_store: false,
        download,
        build_required,
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn transfer_size_is_not_unpacked_size() {
        let p = parse_summary(
            "this path will be fetched (1.5 MiB download, 5.9 MiB unpacked):\n /nix/store/example",
        );
        assert_eq!(p.download.as_deref(), Some("1.5 MiB"));
        assert!(!p.build_required);
        let p = parse_summary(
            "these 2 derivations will be built:\nthese 3 paths will be fetched (20.1 KiB download, 3 MiB unpacked):",
        );
        assert!(p.build_required);
        assert_eq!(p.download.as_deref(), Some("20.1 KiB"));
        assert!(parse_summary("unexpected output").download.is_none());
    }
    #[test]
    fn only_a_missing_package_counts_as_gone() {
        let resolve = |e: &str| format!("cannot resolve package 'x' from pinned nixpkgs: {e}");
        for gone in [
            "invalid package attribute".to_string(),
            resolve(
                "error: flake 'path:/nix/store/x-source' does not provide attribute 'packages.x86_64-linux.x', 'legacyPackages.x86_64-linux.x' or 'x'",
            ),
            resolve(
                "error: … while calling the 'throw' builtin\n error: attribute is not a package derivation",
            ),
            resolve("error: 'x' has been removed from nixpkgs as it was unmaintained"),
        ] {
            assert!(no_longer_a_package(&gone), "{gone}");
        }
        for failure in [
            resolve(
                "error: unable to download 'https://cache.nixos.org/x.narinfo': Couldn't resolve host name (6)",
            ),
            resolve("error: interrupted by the user"),
            resolve("error: attribute 'meta' missing"),
            "could not build package 'x': builder failed".to_string(),
        ] {
            assert!(!no_longer_a_package(&failure), "{failure}");
        }
    }
}
