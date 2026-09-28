//! Host-trusted flake dev shells (ADR 0007). The host snapshots the flake
//! source into the store and evaluates that immutable copy, so later workspace
//! edits never change what a sandbox received at launch.
use crate::{
    Result,
    evaluator::{Evaluator, source_tree},
    process::Cancellation,
    session::store_path,
};
use serde_json::Value;
use std::{
    ffi::OsString,
    fs::{self, OpenOptions},
    io::Read,
    os::unix::fs::OpenOptionsExt,
    path::{Path, PathBuf},
};

const MAX_LOCK_BYTES: u64 = 4 * 1024 * 1024;
const MAX_ENVIRONMENT_BYTES: u64 = 64 * 1024 * 1024;
/// `nix develop` never exports these into the dev shell.
const NIX_IGNORED: &[&str] = &[
    "BASHOPTS",
    "HOME",
    "NIX_BUILD_TOP",
    "NIX_ENFORCE_PURITY",
    "NIX_LOG_FD",
    "NIX_REMOTE",
    "PPID",
    "SHELL",
    "SHELLOPTS",
    "SSL_CERT_DIR",
    "SSL_CERT_FILE",
    "NIX_SSL_CERT_FILE",
    "TEMP",
    "TEMPDIR",
    "TERM",
    "TMP",
    "TMPDIR",
    "TZ",
    "UID",
];
/// Sandbox plumbing a dev shell may not replace.
const RESERVED: &[&str] = &[
    "BASH_ENV",
    "DOCKER_HOST",
    "ENV",
    "LC_ALL",
    "LOGNAME",
    "OLDPWD",
    "PWD",
    "PYTHONNOUSERSITE",
    "SHLVL",
    "USER",
    "_",
];
/// Search paths combined with the sandbox's own value, dev shell first.
pub const PREPENDED: &[&str] = &["PATH", "PKG_CONFIG_PATH", "XDG_DATA_DIRS"];

/// One host-approved dev shell generation, shared by a sandbox's children.
#[derive(Clone, Debug)]
pub struct DevShell {
    /// Host flake reference as given at launch, for display.
    pub reference: String,
    /// Store copy of the flake source that was evaluated.
    pub source: PathBuf,
    /// The trusted `flake.lock`, parsed so formatting never matters.
    pub lock: Value,
    /// `nix print-dev-env` environment in the store; its closure is mounted.
    pub profile: PathBuf,
    /// Exported variables, excluding those ignored by `nix develop` and
    /// sandbox plumbing.
    pub variables: Vec<(String, String)>,
    /// Bash code reproducing the full environment, functions and shellHook.
    pub script: String,
}

/// Split `PATH[#ATTR]` into an absolute flake directory and an attribute.
pub fn parse_reference(reference: &str) -> Result<(PathBuf, Option<&str>)> {
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
                .all(|b| b.is_ascii_alphanumeric() || b"_-.+'".contains(&b)))
    {
        return Err("invalid dev shell attribute".into());
    }
    Ok((path.to_path_buf(), attr))
}

/// Launch and refresh never write or update a lock.
fn nix(subcommand: &[&str]) -> Vec<OsString> {
    subcommand
        .iter()
        .chain(&["--no-update-lock-file", "--no-write-lock-file"])
        .map(OsString::from)
        .collect()
}

fn read_bounded(path: &Path, limit: u64, what: &str) -> Result<Vec<u8>> {
    let file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NONBLOCK | libc::O_NOFOLLOW)
        .open(path)
        .map_err(|e| format!("cannot open {what}: {e}"))?;
    if !file.metadata()?.is_file() {
        return Err(format!("{what} must be a regular file").into());
    }
    let mut bytes = Vec::new();
    file.take(limit + 1).read_to_end(&mut bytes)?;
    if bytes.len() as u64 > limit {
        return Err(format!("{what} exceeds {} MiB", limit / (1024 * 1024)).into());
    }
    Ok(bytes)
}

/// Snapshot and evaluate a dev shell for a host launch in the evaluator
/// sandbox. `directory` is the session's private host directory; it receives
/// the evaluator state and the environment GC root.
pub fn prepare(
    reference: &str,
    evaluator: &Evaluator,
    directory: &Path,
    cancel: &Cancellation,
) -> Result<DevShell> {
    let (path, attr) = parse_reference(reference)?;
    if !fs::metadata(&path)?.is_dir() {
        return Err(format!("dev shell flake {} is not a directory", path.display()).into());
    }
    // Only the flake's own source tree is visible to evaluation.
    let visible = [source_tree(&path)];
    let mut args = nix(&["flake", "metadata", "--json"]);
    args.push(path.clone().into());
    let metadata: Value =
        serde_json::from_str(&evaluator.nix(&args, &visible, directory, cancel)?)?;
    let root = store_path(Path::new(
        metadata["path"]
            .as_str()
            .ok_or("flake metadata has no source path")?,
    ))?;
    let source = match metadata["locked"]["dir"].as_str() {
        Some(dir) if !dir.is_empty() => {
            if Path::new(dir)
                .components()
                .any(|c| !matches!(c, std::path::Component::Normal(_)))
            {
                return Err("invalid flake subdirectory".into());
            }
            root.join(dir)
        }
        _ => root.clone(),
    };
    let lock: Value = serde_json::from_slice(
        &read_bounded(&source.join("flake.lock"), MAX_LOCK_BYTES, "flake.lock").map_err(|e| {
            format!(
                "{e}; the flake source must contain a complete flake.lock \
                 (for a Git flake, it must be tracked)"
            )
        })?,
    )?;
    let installable = format!(
        "path:{}{}",
        source.display(),
        attr.map(|a| format!("#{a}")).unwrap_or_default()
    );
    let link = evaluator.roots().join("devshell-profile");
    let mut args = nix(&["print-dev-env", "--json"]);
    args.extend(["--profile".into(), link.clone().into(), installable.into()]);
    evaluator.nix(&args, &[], directory, cancel).map_err(|e| {
        format!(
            "could not evaluate dev shell {reference}: {}",
            cause(&e.to_string())
        )
    })?;
    let profile = store_path(&fs::canonicalize(&link)?)?;
    let environment: Value = serde_json::from_slice(&read_bounded(
        &profile,
        MAX_ENVIRONMENT_BYTES,
        "dev shell environment",
    )?)?;
    let (variables, script) = environment_of(&environment, reference)?;
    Ok(DevShell {
        reference: reference.into(),
        source: root,
        lock,
        profile,
        variables,
        script,
    })
}

/// Nix prints trace frames before the actual error; lead with the latter so
/// bounded diagnostics keep it.
fn cause(message: &str) -> String {
    match message
        .rfind("\nerror:")
        .or_else(|| message.rfind("       error:"))
    {
        Some(at) => message[at..].trim().to_string(),
        None => message.to_string(),
    }
}

fn variable_name(name: &str) -> bool {
    let mut bytes = name.bytes();
    bytes
        .next()
        .is_some_and(|b| b.is_ascii_alphabetic() || b == b'_')
        && bytes.all(|b| b.is_ascii_alphanumeric() || b == b'_')
}

fn quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', r"'\''"))
}

/// Convert `nix print-dev-env --json` output into exported variables and an
/// equivalent Bash script. Variables Nix itself ignores, sandbox plumbing and
/// `GOBLINS_*` names are left out of both.
pub fn environment_of(
    environment: &Value,
    reference: &str,
) -> Result<(Vec<(String, String)>, String)> {
    let variables = environment["variables"]
        .as_object()
        .ok_or("dev shell environment has no variables")?;
    let functions = environment["bashFunctions"]
        .as_object()
        .ok_or("dev shell environment has no functions")?;
    let mut exported = Vec::new();
    let mut script = format!(
        "# Generated by Goblins from the dev shell {}.\n",
        reference.replace('\n', " ")
    );
    for (name, variable) in variables {
        if !variable_name(name)
            || NIX_IGNORED.contains(&name.as_str())
            || RESERVED.contains(&name.as_str())
            || name.starts_with("GOBLINS_")
        {
            continue;
        }
        let value = &variable["value"];
        match variable["type"].as_str() {
            Some(kind @ ("exported" | "var")) => {
                let Some(value) = value.as_str().filter(|v| !v.contains('\0')) else {
                    continue;
                };
                if kind == "exported" {
                    exported.push((name.clone(), value.to_string()));
                }
                if PREPENDED.contains(&name.as_str()) {
                    script.push_str(&format!(
                        "export {name}={}\"${{{name}:+:${name}}}\"\n",
                        quote(value)
                    ));
                } else {
                    let prefix = if kind == "exported" { "export " } else { "" };
                    script.push_str(&format!("{prefix}{name}={}\n", quote(value)));
                }
            }
            Some("array") => {
                let items: Option<Vec<_>> = value
                    .as_array()
                    .map(|a| a.iter().map(|v| v.as_str().map(quote)).collect())
                    .unwrap_or(None);
                if let Some(items) = items {
                    script.push_str(&format!("declare -a {name}=({})\n", items.join(" ")));
                }
            }
            Some("associative") => {
                let items: Option<Vec<_>> = value
                    .as_object()
                    .map(|o| {
                        o.iter()
                            .map(|(k, v)| {
                                v.as_str().map(|v| format!("[{}]={}", quote(k), quote(v)))
                            })
                            .collect()
                    })
                    .unwrap_or(None);
                if let Some(items) = items {
                    script.push_str(&format!("declare -A {name}=({})\n", items.join(" ")));
                }
            }
            _ => {}
        }
    }
    for (name, body) in functions {
        if let Some(body) = body.as_str()
            && variable_name(name)
        {
            script.push_str(&format!("{name} ()\n{{\n{body}\n}}\n"));
        }
    }
    script.push_str("eval \"${shellHook:-}\"\n");
    Ok((exported, script))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn references_are_absolute_local_paths_with_simple_attributes() {
        assert_eq!(
            parse_reference("/src/app#rust").unwrap(),
            (PathBuf::from("/src/app"), Some("rust"))
        );
        assert_eq!(
            parse_reference("/src/app").unwrap(),
            (PathBuf::from("/src/app"), None)
        );
        for bad in [
            "app",
            "github:owner/repo",
            "/src/app?dir=x",
            "/src/app#",
            "/src/app#a b",
            "/src/app#$(x)",
        ] {
            assert!(parse_reference(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn cause_keeps_the_final_nix_error() {
        let message = "host command failed (exit status: 1): error:\n       … while x\n\n       error: path '/tmp/x' does not exist\n";
        assert_eq!(cause(message), "error: path '/tmp/x' does not exist");
        assert_eq!(cause("plain"), "plain");
    }

    #[test]
    fn environment_matches_nix_develop_filtering() {
        let environment = json!({
            "variables": {
                "CC": {"type": "exported", "value": "gcc"},
                "PATH": {"type": "exported", "value": "/nix/store/a/bin"},
                "HOME": {"type": "exported", "value": "/homeless-shelter"},
                "TMPDIR": {"type": "exported", "value": "/build"},
                "USER": {"type": "exported", "value": "nixbld"},
                "GOBLINS_X": {"type": "exported", "value": "no"},
                "phases": {"type": "var", "value": "buildPhase"},
                "arr": {"type": "array", "value": ["a", "b'c"]},
                "map": {"type": "associative", "value": {"k": "v"}},
                "odd": {"type": "unknown", "value": null},
                "bad-name": {"type": "exported", "value": "x"},
                "shellHook": {"type": "var", "value": "echo hi"},
            },
            "bashFunctions": {"greet": "echo hello"},
        });
        let (exported, script) = environment_of(&environment, "/src#x").unwrap();
        assert_eq!(
            exported,
            vec![
                ("CC".to_string(), "gcc".to_string()),
                ("PATH".to_string(), "/nix/store/a/bin".to_string()),
            ]
        );
        assert!(script.contains("export CC='gcc'\n"));
        assert!(script.contains("export PATH='/nix/store/a/bin'\"${PATH:+:$PATH}\"\n"));
        assert!(script.contains("phases='buildPhase'\n"));
        assert!(script.contains(r"declare -a arr=('a' 'b'\''c')"));
        assert!(script.contains("declare -A map=(['k']='v')"));
        assert!(script.contains("greet ()\n{\necho hello\n}\n"));
        assert!(script.ends_with("eval \"${shellHook:-}\"\n"));
        for absent in ["HOME", "TMPDIR", "USER", "GOBLINS_X", "odd", "bad-name"] {
            assert!(!script.contains(&format!("{absent}=")), "{absent}");
        }
    }
}
