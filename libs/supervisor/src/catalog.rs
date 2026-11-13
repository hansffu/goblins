//! Pinned host Nix lookup and advisory pre-approval cost estimates. A preview
//! evaluates metadata and runs --dry-run; it never realizes or mounts a package.
use crate::Result;
use crate::process;
use crate::process::Cancellation;
use goblins_protocol::package_name;
use serde::Deserialize;
use std::{
    path::{Path, PathBuf},
    process::Command,
};

use goblins_runtime::catalog::Preview;
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
/// Where the catalogue's packages are evaluated from (ADR 0020).
#[derive(Debug, PartialEq, Eq)]
pub enum Source<'a> {
    /// The pinned nixpkgs source tree, imported directly in restricted
    /// evaluation; Nix does not hash the whole tree as it does for a flake.
    Store(&'a Path),
    /// Any flake reference, through its `legacyPackages`.
    Flake(&'a str),
}
/// The manifest's explicit `nixpkgs` entry selects the store form, and only
/// when it names an entry directly under /nix/store. The flake reference is
/// never inspected: a store-backed flake need not be an importable nixpkgs.
pub fn source<'a>(flake: &'a str, nixpkgs: Option<&'a Path>) -> Source<'a> {
    use std::path::Component;
    match nixpkgs {
        Some(path)
            if matches!(
                path.components().collect::<Vec<_>>().as_slice(),
                [Component::RootDir, Component::Normal(nix), Component::Normal(store), Component::Normal(_)]
                    if *nix == "nix" && *store == "store"
            ) =>
        {
            Source::Store(path)
        }
        _ => Source::Flake(flake),
    }
}
/// The pinned nixpkgs, without the host's config, overlays or environment.
const IMPORT: &str = "import <nixpkgs> { system = \"x86_64-linux\"; config = {}; overlays = []; }";
/// `name` (an already validated attribute path) as Nix source. Components
/// that are Nix keywords are quoted; the CLI's attribute paths need no quoting.
fn attribute_path(name: &str) -> String {
    const KEYWORDS: &[&str] = &[
        "assert", "else", "if", "in", "inherit", "let", "or", "rec", "then", "with",
    ];
    name.split('.')
        .map(|part| {
            if KEYWORDS.contains(&part) {
                format!("\"{part}\"")
            } else {
                part.to_string()
            }
        })
        .collect::<Vec<_>>()
        .join(".")
}
/// Append the arguments naming package `name` (an already validated
/// attribute path), with `output` for a build.
pub fn installable(source: &Source<'_>, name: &str, output: Option<&str>, command: &mut Command) {
    match source {
        Source::Flake(flake) => {
            let attribute = attribute(flake, name);
            command.arg(match output {
                Some(output) => format!("{attribute}^{output}"),
                None => attribute,
            });
        }
        Source::Store(path) => {
            // --impure is needed for <nixpkgs>; restrict-eval hides every path
            // outside -I and makes builtins.getEnv return "".
            command
                .args(["--impure", "--option", "restrict-eval", "true", "-I"])
                .arg(format!("nixpkgs={}", path.display()));
            match output {
                Some(output) => command.args(["--expr", IMPORT, &format!("{name}^{output}")]),
                None => command.args(["--expr", &format!("({IMPORT}).{}", attribute_path(name))]),
            };
        }
    }
}
/// The flake form's wording for a missing package, which callers report,
/// in place of the plain evaluation's "attribute '<name>' missing".
fn missing_attribute(name: &str, error: &str) -> Option<String> {
    (error.contains("attribute '") && error.contains("' missing")).then(|| {
        format!(
            "cannot resolve package '{name}' from pinned nixpkgs: \
             the pinned nixpkgs does not provide attribute '{name}'"
        )
    })
}
pub fn select(
    source: &Source<'_>,
    name: &str,
    directory: &Path,
    cancel: &Cancellation,
    allow_build: bool,
) -> Result<Selection> {
    if !package_name(name) {
        return Err("invalid package attribute".into());
    }
    let selection = "p: if builtins.isAttrs p && (p.type or null) == \"derivation\" then let q = p.bin or p; in { output = q.outputName; path = q.outPath; } else throw \"attribute is not a package derivation\"";
    let mut command = nix();
    command.args([
        "eval",
        "--no-write-lock-file",
        "--option",
        "allow-import-from-derivation",
        if allow_build { "true" } else { "false" },
        "--json",
    ]);
    installable(source, name, None, &mut command);
    command.args(["--apply", selection]);
    let raw = process::command(&mut command, directory, cancel).map_err(|e| {
        let e = e.to_string();
        match source {
            Source::Store(_) => missing_attribute(name, &e),
            Source::Flake(_) => None,
        }
        .unwrap_or_else(|| format!("cannot resolve package '{name}' from pinned nixpkgs: {e}"))
    })?;
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
pub fn preview(
    source: &Source<'_>,
    name: &str,
    directory: &Path,
    cancel: &Cancellation,
) -> Result<Preview> {
    let selection = select(source, name, directory, cancel, false)?;
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
    let mut command = nix();
    command.env("LC_ALL", "C").env("NO_COLOR", "1").args([
        "build",
        "--dry-run",
        "--no-link",
        "--no-write-lock-file",
        "--log-format",
        "raw",
        "--option",
        "allow-import-from-derivation",
        "false",
    ]);
    installable(source, name, Some(&selection.output), &mut command);
    process::command(&mut command, directory, cancel)?;
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
    fn arguments(source: &Source<'_>, name: &str, output: Option<&str>) -> Vec<String> {
        let mut command = Command::new("nix");
        installable(source, name, output, &mut command);
        command
            .get_args()
            .map(|a| a.to_str().unwrap().to_string())
            .collect()
    }
    #[test]
    fn only_an_explicit_store_entry_selects_the_direct_import() {
        let flake = "path:/nix/store/abc-source";
        // The location of the flake alone never selects the store form.
        assert_eq!(source(flake, None), Source::Flake(flake));
        let store = Path::new("/nix/store/abc-source");
        assert_eq!(source(flake, Some(store)), Source::Store(store));
        for rejected in [
            "/home/user/nixpkgs",
            "/nix/store/../etc",
            "nix/store/abc",
            "/nix/store",
            "/nix/store/abc-source/pkgs",
        ] {
            assert_eq!(
                source(flake, Some(Path::new(rejected))),
                Source::Flake(flake),
                "{rejected}"
            );
        }
    }
    #[test]
    fn installable_arguments_for_each_source() {
        let flake = "path:/nix/store/abc-source";
        assert_eq!(
            arguments(&Source::Flake(flake), "hello", None),
            [attribute(flake, "hello")]
        );
        assert_eq!(
            arguments(&Source::Flake(flake), "openssl", Some("bin")),
            [format!("{}^bin", attribute(flake, "openssl"))]
        );
        let store = Source::Store(Path::new("/nix/store/abc-source"));
        let eval = arguments(&store, "hello", None);
        assert!(
            eval.windows(2).any(|w| w == ["restrict-eval", "true"]),
            "{eval:?}"
        );
        assert!(
            eval.windows(2)
                .any(|w| w == ["-I", "nixpkgs=/nix/store/abc-source"]),
            "{eval:?}"
        );
        let expression = &eval[eval.iter().position(|a| a == "--expr").unwrap() + 1];
        assert!(expression.ends_with(".hello"), "{expression}");
        assert!(
            expression.contains("config = {}; overlays = [];"),
            "{expression}"
        );
        let build = arguments(&store, "openssl", Some("bin"));
        assert!(
            build.windows(2).any(|w| w == ["restrict-eval", "true"]),
            "{build:?}"
        );
        assert_eq!(build.last().unwrap(), "openssl^bin");
        assert_eq!(build[build.len() - 2], IMPORT);
    }
    #[test]
    fn keyword_components_are_quoted_in_the_expression() {
        assert_eq!(attribute_path("hello"), "hello");
        assert_eq!(
            attribute_path("python3Packages.requests"),
            "python3Packages.requests"
        );
        assert_eq!(attribute_path("perlPackages.if"), "perlPackages.\"if\"");
        assert_eq!(attribute_path("let.in.no-such"), "\"let\".\"in\".no-such");
        let store = Source::Store(Path::new("/nix/store/abc-source"));
        let eval = arguments(&store, "perlPackages.if", None);
        let expression = &eval[eval.iter().position(|a| a == "--expr").unwrap() + 1];
        assert!(
            expression.ends_with(").perlPackages.\"if\""),
            "{expression}"
        );
        // The build's attribute path is not Nix source and stays as given.
        assert_eq!(
            arguments(&store, "perlPackages.if", Some("out"))
                .last()
                .unwrap(),
            "perlPackages.if^out"
        );
    }
    #[test]
    fn names_are_validated_before_any_command_is_built() {
        let store = Source::Store(Path::new("/nix/store/abc-source"));
        let cancel = Cancellation::default();
        for name in ["he\"llo", "hello world", "a; b", ""] {
            let error = select(&store, name, Path::new("/nonexistent"), &cancel, false)
                .err()
                .unwrap()
                .to_string();
            assert_eq!(error, "invalid package attribute", "{name}");
        }
    }
    #[test]
    fn a_missing_attribute_keeps_the_flake_wording() {
        let error = "error:\n       … while evaluating the attribute 'no-such-goblins-package'\n\n       error: attribute 'no-such-goblins-package' missing\n       at «string»:1:1:";
        assert_eq!(
            missing_attribute("no-such-goblins-package", error).unwrap(),
            "cannot resolve package 'no-such-goblins-package' from pinned nixpkgs: \
             the pinned nixpkgs does not provide attribute 'no-such-goblins-package'"
        );
        assert_eq!(missing_attribute("x", "error: unable to download"), None);
    }
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
}
