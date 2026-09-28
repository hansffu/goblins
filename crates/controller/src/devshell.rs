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
    collections::{BTreeMap, BTreeSet},
    ffi::{OsStr, OsString},
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

/// Nix system for dev shell attributes, as for the pinned package catalog.
const SYSTEM: &str = "x86_64-linux";
const MAX_DIFF_BYTES: usize = 32 * 1024;

/// What a generation contains, for refresh diffs: the dev shell's direct
/// inputs by package name, and its derivation environment.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Summary {
    pub packages: BTreeMap<String, BTreeSet<String>>,
    pub env: BTreeMap<String, String>,
    pub derivation: PathBuf,
}

/// One host-approved dev shell generation, shared by a sandbox's children.
#[derive(Clone, Debug)]
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
fn identity(flake: &Path, attr: &str, lock: &Value) -> String {
    format!("{}#{attr}\n{lock}", flake.display())
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
                .all(|b| b.is_ascii_alphanumeric() || b"_-+'".contains(&b)))
    {
        return Err("invalid dev shell attribute".into());
    }
    Ok((path.to_path_buf(), attr))
}

/// Launch and refresh never write or update the workspace lock.
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

fn read_lock(flake: &Path) -> Result<Value> {
    Ok(serde_json::from_slice(&read_bounded(
        &flake.join("flake.lock"),
        MAX_LOCK_BYTES,
        "flake.lock",
    )?)?)
}

/// Copy a flake into the store through the evaluator and return the store
/// root, the flake directory within it and whether it declares nixConfig.
/// With `complete`, the source's `flake.lock` must lock everything: Nix
/// silently fetches and uses inputs missing from the lock even with
/// `--no-update-lock-file`.
fn snapshot(
    evaluator: &Evaluator,
    flake: &OsStr,
    visible: &[PathBuf],
    complete: bool,
    directory: &Path,
    cancel: &Cancellation,
) -> Result<(PathBuf, PathBuf, bool)> {
    let mut args = nix(&["flake", "metadata", "--json"]);
    args.push(flake.into());
    let metadata: Value = serde_json::from_str(&evaluator.nix(&args, visible, directory, cancel)?)?;
    // nixConfig is never applied; Nix reports that it ignored it.
    let config = fs::read_to_string(directory.join("command.err"))
        .unwrap_or_default()
        .contains("ignoring untrusted flake configuration");
    let root = store_path(Path::new(
        metadata["path"]
            .as_str()
            .ok_or("flake metadata has no source path")?,
    ))?;
    let flake = match metadata["locked"]["dir"].as_str() {
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
    if complete {
        let lock = read_lock(&flake)
            .map_err(|e| format!("{e}; the flake source must contain a complete flake.lock"))?;
        if metadata["locks"] != lock {
            let mut changed = Vec::new();
            changed_paths(&lock, &metadata["locks"], "", &mut changed, 5);
            return Err(format!(
                "flake.lock does not lock everything flake.nix uses (changed: {})",
                changed.join(", ")
            )
            .into());
        }
    }
    Ok((root, flake, config))
}

fn installable(flake: &Path, attr: &str) -> String {
    format!("path:{}#devShells.{SYSTEM}.{attr}", flake.display())
}

/// Package name and version from a store name such as `hello-2.12.3`.
fn package(name: &str) -> (String, String) {
    let bytes = name.as_bytes();
    match (1..bytes.len()).find(|&i| bytes[i - 1] == b'-' && bytes[i].is_ascii_digit()) {
        Some(i) => (name[..i - 1].into(), name[i..].into()),
        None => (name.into(), String::new()),
    }
}

fn store_name(path: &str) -> &str {
    let base = path.rsplit('/').next().unwrap_or(path);
    base.get(33..).unwrap_or(base)
}

/// Direct inputs and environment of the dev shell derivation, without
/// building anything.
fn summarize(
    evaluator: &Evaluator,
    installable: &str,
    directory: &Path,
    cancel: &Cancellation,
) -> Result<Summary> {
    let mut args = nix(&["derivation", "show"]);
    args.push(installable.into());
    let shown: Value = serde_json::from_str(&evaluator.nix(&args, &[], directory, cancel)?)?;
    // Format 4 nests derivations and uses basenames; older formats do not.
    let derivations = shown.get("derivations").unwrap_or(&shown);
    let (path, derivation) = derivations
        .as_object()
        .and_then(|d| d.iter().next())
        .ok_or("dev shell has no derivation")?;
    let inputs: Vec<&str> = match derivation["inputs"]["drvs"].as_object() {
        Some(drvs) => drvs.keys().map(String::as_str).collect(),
        None => derivation["inputDrvs"]
            .as_object()
            .map(|d| d.keys().map(String::as_str).collect())
            .unwrap_or_default(),
    };
    let mut packages: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    for input in inputs {
        let (name, version) = package(store_name(input).trim_end_matches(".drv"));
        packages.entry(name).or_default().insert(version);
    }
    let env = derivation["env"]
        .as_object()
        .map(|e| {
            e.iter()
                .filter_map(|(k, v)| Some((k.clone(), v.as_str()?.to_string())))
                .collect()
        })
        .unwrap_or_default();
    let path = if path.starts_with('/') {
        path.clone()
    } else {
        format!("/nix/store/{path}")
    };
    Ok(Summary {
        packages,
        env,
        derivation: path.into(),
    })
}

/// The realized environment: its store path, exported variables and script.
type Environment = (PathBuf, Vec<(String, String)>, String);

/// Build (or fetch) and read the dev shell environment.
fn realize(
    evaluator: &Evaluator,
    installable: &str,
    link: &Path,
    reference: &str,
    directory: &Path,
    cancel: &Cancellation,
) -> Result<Environment> {
    let mut args = nix(&["print-dev-env", "--json"]);
    args.extend(["--profile".into(), link.into(), installable.into()]);
    evaluator.nix(&args, &[], directory, cancel).map_err(|e| {
        format!(
            "could not evaluate dev shell {reference}: {}",
            cause(&e.to_string())
        )
    })?;
    let profile = store_path(&fs::canonicalize(link)?)?;
    let environment: Value = serde_json::from_slice(&read_bounded(
        &profile,
        MAX_ENVIRONMENT_BYTES,
        "dev shell environment",
    )?)?;
    let (variables, script) = environment_of(&environment, reference)?;
    Ok((profile, variables, script))
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
    let attr = attr.unwrap_or("default").to_string();
    if !fs::metadata(&path)?.is_dir() {
        return Err(format!("dev shell flake {} is not a directory", path.display()).into());
    }
    // Only the flake's own source tree is visible to evaluation.
    let visible = [source_tree(&path)];
    let (root, flake, _) = snapshot(
        evaluator,
        path.as_os_str(),
        &visible,
        true,
        directory,
        cancel,
    )
    .map_err(|e| {
        format!(
            "{e}\n  lock the flake on the host first (for a Git flake, flake.lock must be tracked)"
        )
    })?;
    let lock = read_lock(&flake)?;
    let installable = installable(&flake, &attr);
    let summary = summarize(evaluator, &installable, directory, cancel).map_err(|e| {
        format!(
            "could not evaluate dev shell {reference}: {}",
            cause(&e.to_string())
        )
    })?;
    let link = evaluator.roots().join("devshell-1");
    let (profile, variables, script) =
        realize(evaluator, &installable, &link, reference, directory, cancel)?;
    Ok(DevShell {
        reference: reference.into(),
        path,
        attr,
        generation: 1,
        source: root,
        flake,
        lock,
        profile,
        variables,
        script,
        summary,
    })
}

/// Paths (dotted) where two JSON values differ, at most `limit` of them.
fn changed_paths(old: &Value, new: &Value, prefix: &str, out: &mut Vec<String>, limit: usize) {
    if out.len() >= limit || old == new {
        return;
    }
    match (old, new) {
        (Value::Object(a), Value::Object(b)) => {
            let keys: BTreeSet<_> = a.keys().chain(b.keys()).collect();
            for key in keys {
                let path = if prefix.is_empty() {
                    key.clone()
                } else {
                    format!("{prefix}.{key}")
                };
                changed_paths(
                    a.get(key).unwrap_or(&Value::Null),
                    b.get(key).unwrap_or(&Value::Null),
                    &path,
                    out,
                    limit,
                );
            }
        }
        _ => out.push(if prefix.is_empty() {
            "(file)".into()
        } else {
            prefix.into()
        }),
    }
}

/// Fail unless the workspace lock is the sandbox's trusted lock. Only a
/// host-launched or host-approved lock is trusted.
pub fn check_lock(current: &DevShell) -> Result<()> {
    let workspace = read_lock(&current.path).map_err(|e| {
        format!(
            "{e}\n  flake.lock must be the trusted lock for this sandbox\n  \
             revert: goblins devshell restore-lock"
        )
    })?;
    if workspace == current.lock {
        return Ok(());
    }
    let mut changed = Vec::new();
    changed_paths(&current.lock, &workspace, "", &mut changed, 5);
    Err(format!(
        "flake.lock differs from the trusted lock for this sandbox\n  changed: {}\n  \
         revert:  goblins devshell restore-lock\n  \
         then:    goblins devshell refresh --update INPUT (or --lock for new inputs)",
        changed.join(", ")
    )
    .into())
}

/// Reject lock entries that reach host files or credentialed transports, or
/// that are not locked to content.
pub fn validate_lock(lock: &Value) -> Result<()> {
    let nodes = lock["nodes"].as_object().ok_or("flake.lock has no nodes")?;
    let mut problems = Vec::new();
    for (name, node) in nodes {
        if node.get("locked").is_none() {
            // The root node and follows-only nodes have nothing to fetch.
            continue;
        }
        let locked = &node["locked"];
        let url = locked["url"].as_str().unwrap_or_default();
        let kind = locked["type"].as_str().unwrap_or_default();
        let path = locked["path"].as_str().unwrap_or_default();
        let relative = kind == "path"
            && !path.starts_with('/')
            && Path::new(path).components().all(|c| {
                matches!(
                    c,
                    std::path::Component::Normal(_) | std::path::Component::CurDir
                )
            });
        let allowed = match kind {
            "github" | "gitlab" | "sourcehut" => true,
            "git" | "tarball" | "file" => url.starts_with("https://"),
            "path" => relative || path.starts_with("/nix/store/"),
            _ => false,
        };
        if !allowed {
            problems.push(format!(
                "{name}: {kind} input {}",
                if url.is_empty() { path } else { url }
            ));
        } else if locked["narHash"].as_str().is_none() && !relative {
            problems.push(format!("{name}: not locked to a narHash"));
        }
    }
    if problems.is_empty() {
        Ok(())
    } else {
        Err(format!(
            "flake.lock has inputs a refresh cannot use (only locked https forges, \
             tarballs, store paths and paths inside the flake are allowed):\n  {}",
            problems.join("\n  ")
        )
        .into())
    }
}

/// A refresh candidate: evaluated and summarized, not yet built or trusted.
pub struct Candidate {
    pub generation: u32,
    pub root: PathBuf,
    pub flake: PathBuf,
    pub lock: Value,
    pub summary: Summary,
    pub description: String,
}
impl Candidate {
    pub fn identity(&self, attr: &str) -> String {
        identity(&self.flake, attr, &self.lock)
    }
}

/// Inputs to update (`Some(empty)` means all) or new inputs to lock.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Changes {
    pub update: Option<Vec<String>>,
    pub lock: bool,
}

fn copy_tree(from: &Path, to: &Path, cancel: &Cancellation) -> Result<()> {
    fs::create_dir(to)?;
    for entry in fs::read_dir(from)? {
        cancel.check()?;
        let entry = entry?;
        let target = to.join(entry.file_name());
        let kind = entry.file_type()?;
        if kind.is_dir() {
            copy_tree(&entry.path(), &target, cancel)?;
        } else if kind.is_symlink() {
            std::os::unix::fs::symlink(fs::read_link(entry.path())?, &target)?;
        } else {
            fs::copy(entry.path(), &target)?;
            // Store files are read-only; Nix must be able to rewrite the lock.
            use std::os::unix::fs::PermissionsExt;
            let mode = fs::metadata(&target)?.permissions().mode();
            fs::set_permissions(&target, fs::Permissions::from_mode(0o644 | (mode & 0o111)))?;
        }
    }
    Ok(())
}

/// Evaluate the workspace flake as a refresh candidate in the evaluator
/// sandbox. `visible` is the flake's source tree, which must already be
/// visible to the sandbox.
pub fn candidate(
    current: &DevShell,
    changes: &Changes,
    evaluator: &Evaluator,
    visible: &[PathBuf],
    git: Option<&Path>,
    directory: &Path,
    cancel: &Cancellation,
) -> Result<Candidate> {
    check_lock(current)?;
    // With --lock or --update the candidate lock is computed below and
    // checked for completeness then.
    let relock = changes.update.is_some() || changes.lock;
    let (mut root, mut flake, mut config) = snapshot(
        evaluator,
        current.path.as_os_str(),
        visible,
        !relock,
        directory,
        cancel,
    )
    .map_err(|e| {
        format!(
            "{e}\n  lock new inputs: goblins devshell refresh --lock\n  \
             update inputs:   goblins devshell refresh --update [INPUT...]"
        )
    })?;
    if relock {
        // Compute the candidate lock in a private copy; the workspace lock
        // changes only after approval.
        let work = evaluator.work();
        let _ = fs::remove_dir_all(&work);
        copy_tree(&flake, &work, cancel)?;
        let target = format!("path:{}", work.display());
        let mut args: Vec<OsString> = match &changes.update {
            Some(inputs) => ["flake", "update"]
                .iter()
                .map(OsString::from)
                .chain(inputs.iter().map(OsString::from))
                .chain(["--flake".into(), target.into()])
                .collect(),
            None => ["flake", "lock"]
                .iter()
                .map(OsString::from)
                .chain([target.into()])
                .collect(),
        };
        args.push("--quiet".into());
        evaluator
            .nix(&args, &[], directory, cancel)
            .map_err(|e| format!("could not update flake.lock: {}", cause(&e.to_string())))?;
        let path = work.clone().into_os_string();
        let mut prefixed = OsString::from("path:");
        prefixed.push(&path);
        (root, flake, config) = snapshot(evaluator, &prefixed, &[], true, directory, cancel)?;
        let _ = fs::remove_dir_all(&work);
    }
    let lock = read_lock(&flake)?;
    validate_lock(&lock)?;
    let installable = installable(&flake, &current.attr);
    let summary = summarize(evaluator, &installable, directory, cancel)
        .map_err(|e| format!("could not evaluate dev shell: {}", cause(&e.to_string())))?;
    let mut args: Vec<OsString> = ["build", "--dry-run", "--no-link", "--log-format", "raw"]
        .iter()
        .map(OsString::from)
        .collect();
    args.push(format!("{}^*", summary.derivation.display()).into());
    evaluator.nix(&args, &[], directory, cancel)?;
    let cost = cost(
        &fs::read_to_string(directory.join("command.err")).unwrap_or_default(),
        &summary.derivation,
    );
    let generation = current.generation + 1;
    let description = describe(
        current,
        generation,
        &lock,
        &summary,
        &cost,
        config,
        source_diff(git, &current.flake, &flake),
    );
    Ok(Candidate {
        generation,
        root,
        flake,
        lock,
        summary,
        description,
    })
}

/// Build and read an approved candidate, as the sandbox's next generation.
pub fn apply(
    current: &DevShell,
    candidate: Candidate,
    evaluator: &Evaluator,
    directory: &Path,
    cancel: &Cancellation,
) -> Result<DevShell> {
    let installable = installable(&candidate.flake, &current.attr);
    let link = evaluator
        .roots()
        .join(format!("devshell-{}", candidate.generation));
    let (profile, variables, script) = realize(
        evaluator,
        &installable,
        &link,
        &current.reference,
        directory,
        cancel,
    )?;
    Ok(DevShell {
        reference: current.reference.clone(),
        path: current.path.clone(),
        attr: current.attr.clone(),
        generation: candidate.generation,
        source: candidate.root,
        flake: candidate.flake,
        lock: candidate.lock,
        profile,
        variables,
        script,
        summary: candidate.summary,
    })
}

#[derive(Debug, Default, PartialEq)]
struct Cost {
    /// Store names of derivations the host daemon will build.
    built: Vec<String>,
    /// Store names of paths that will be fetched.
    fetched: Vec<String>,
    download: Option<String>,
}

/// Parse `nix build --dry-run` output; the dev shell derivation itself is
/// never built, so it is not counted.
fn cost(text: &str, shell: &Path) -> Cost {
    let mut cost = Cost::default();
    let mut section = 0;
    for line in text.lines() {
        if line.contains("will be built:") {
            section = 1;
        } else if line.contains("will be fetched") {
            section = 2;
            if let Some((_, rest)) = line.split_once("will be fetched (")
                && let Some((size, _)) = rest.split_once(" download")
            {
                cost.download = Some(size.trim().to_string());
            }
        } else if let Some(path) = line
            .strip_prefix("  ")
            .filter(|l| l.starts_with("/nix/store/"))
        {
            let path = path.trim();
            let name = store_name(path).trim_end_matches(".drv").to_string();
            match section {
                1 if Path::new(path) != shell => cost.built.push(name),
                2 => cost.fetched.push(name),
                _ => {}
            }
        } else {
            section = 0;
        }
    }
    cost
}

fn short(rev: &str) -> &str {
    rev.get(..7).unwrap_or(rev)
}

/// `YYYY-MM-DD` for a Unix time (civil-from-days).
fn date(seconds: i64) -> String {
    let days = seconds.div_euclid(86_400);
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    format!("{year:04}-{month:02}-{day:02}")
}

fn node_source(node: &Value) -> String {
    let l = &node["locked"];
    match l["type"].as_str().unwrap_or_default() {
        kind @ ("github" | "gitlab" | "sourcehut") => format!(
            "{kind}:{}/{}",
            l["owner"].as_str().unwrap_or("?"),
            l["repo"].as_str().unwrap_or("?")
        ),
        "path" => format!("path:{}", l["path"].as_str().unwrap_or("?")),
        kind => format!("{kind}:{}", l["url"].as_str().unwrap_or("?")),
    }
}

fn node_version(node: &Value) -> String {
    let l = &node["locked"];
    let id = l["rev"]
        .as_str()
        .map(short)
        .or_else(|| l["narHash"].as_str().map(|h| h.get(7..14).unwrap_or(h)))
        .unwrap_or("?");
    match l["lastModified"].as_i64().filter(|&t| t > 0) {
        Some(t) => format!("{id} ({})", date(t)),
        None => id.to_string(),
    }
}

fn input_lines(old: &Value, new: &Value) -> Vec<String> {
    let empty = serde_json::Map::new();
    let (a, b) = (
        old["nodes"].as_object().unwrap_or(&empty),
        new["nodes"].as_object().unwrap_or(&empty),
    );
    let names: BTreeSet<_> = a.keys().chain(b.keys()).filter(|k| *k != "root").collect();
    let mut lines = Vec::new();
    for name in names {
        match (a.get(name), b.get(name)) {
            (Some(x), Some(y)) if x["locked"] == y["locked"] => {}
            (Some(x), Some(y)) if node_source(x) != node_source(y) => lines.push(format!(
                "~ {name}  {} → {}  {} → {}",
                node_source(x),
                node_source(y),
                node_version(x),
                node_version(y)
            )),
            (Some(x), Some(y)) => lines.push(format!(
                "~ {name}  {}  {} → {}",
                node_source(y),
                node_version(x),
                node_version(y)
            )),
            (None, Some(y)) if y.get("locked").is_some() => lines.push(format!(
                "+ {name}  {} @ {}",
                node_source(y),
                node_version(y)
            )),
            (Some(x), None) if x.get("locked").is_some() => {
                lines.push(format!("- {name}  {}", node_source(x)))
            }
            _ => {}
        }
    }
    lines
}

fn package_lines(old: &Summary, new: &Summary) -> Vec<String> {
    let names: BTreeSet<_> = old.packages.keys().chain(new.packages.keys()).collect();
    let versions = |v: &BTreeSet<String>| {
        v.iter()
            .map(|s| if s.is_empty() { "?" } else { s.as_str() })
            .collect::<Vec<_>>()
            .join(", ")
    };
    let mut lines = Vec::new();
    for name in names {
        match (old.packages.get(name), new.packages.get(name)) {
            (Some(a), Some(b)) if a == b => {}
            (Some(a), Some(b)) => {
                lines.push(format!("[U] {name} {} → {}", versions(a), versions(b)))
            }
            (None, Some(b)) => lines.push(format!("[A] {name} {}", versions(b))),
            (Some(a), None) => lines.push(format!("[R] {name} {}", versions(a))),
            (None, None) => {}
        }
    }
    lines
}

/// Environment keys covered by the package list or always different.
const NOISY: &[&str] = &[
    "buildInputs",
    "builder",
    "depsBuildBuild",
    "depsBuildBuildPropagated",
    "depsBuildTarget",
    "depsBuildTargetPropagated",
    "depsHostHost",
    "depsHostHostPropagated",
    "depsTargetTarget",
    "depsTargetTargetPropagated",
    "nativeBuildInputs",
    "out",
    "propagatedBuildInputs",
    "propagatedNativeBuildInputs",
    "stdenv",
];

/// Replace store hashes, so a rebuilt dependency is not an environment change.
fn without_hashes(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    let mut rest = value;
    while let Some(at) = rest.find("/nix/store/") {
        out.push_str(&rest[..at + 11]);
        rest = &rest[at + 11..];
        if rest.len() >= 33 && rest.as_bytes()[32] == b'-' {
            out.push('…');
            rest = &rest[32..];
        }
    }
    out.push_str(rest);
    out
}

fn short_value(value: &str) -> bool {
    value.chars().count() <= 80 && !value.contains('\n')
}

/// Package names and versions of store paths entering the machine.
fn incoming_lines(tag: &str, names: &[String], limit: usize) -> Vec<String> {
    let mut packages: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    for name in names {
        let (name, version) = package(name);
        packages.entry(name).or_default().insert(version);
    }
    let mut lines: Vec<String> = packages
        .iter()
        .take(limit)
        .map(|(name, versions)| {
            let versions: Vec<_> = versions.iter().filter(|v| !v.is_empty()).cloned().collect();
            format!("[{tag}] {name} {}", versions.join(", "))
                .trim_end()
                .to_string()
        })
        .collect();
    if packages.len() > limit {
        lines.push(format!("… and {} more", packages.len() - limit));
    }
    lines
}

/// What deserves the approver's attention before the details.
fn attention(
    current: &DevShell,
    lock: &Value,
    summary: &Summary,
    cost: &Cost,
    config: bool,
) -> Vec<String> {
    let empty = serde_json::Map::new();
    let old = current.lock["nodes"].as_object().unwrap_or(&empty);
    let mut lines = Vec::new();
    for (name, node) in lock["nodes"].as_object().unwrap_or(&empty) {
        if node.get("locked").is_none() {
            continue;
        }
        match old.get(name).filter(|n| n.get("locked").is_some()) {
            Some(previous) if node_source(previous) != node_source(node) => lines.push(format!(
                "! input '{name}' now comes from {} (was {})",
                node_source(node),
                node_source(previous)
            )),
            None => lines.push(format!("! new input '{name}' from {}", node_source(node))),
            _ => {}
        }
    }
    if config {
        lines.push("! the flake declares nixConfig (ignored, never applied)".into());
    }
    if !cost.built.is_empty() {
        lines.push(format!(
            "! {} derivation(s) will be built on this machine by the host Nix daemon",
            cost.built.len()
        ));
    }
    if current.summary.env.get("shellHook") != summary.env.get("shellHook") {
        lines.push("! shellHook changed; it runs inside the sandbox after approval".into());
    }
    lines
}

fn env_lines(old: &Summary, new: &Summary) -> Vec<String> {
    let names: BTreeSet<_> = old.env.keys().chain(new.env.keys()).collect();
    let mut lines = Vec::new();
    for name in names.into_iter().filter(|n| !NOISY.contains(&n.as_str())) {
        match (old.env.get(name), new.env.get(name)) {
            (Some(a), Some(b)) if without_hashes(a) == without_hashes(b) => {}
            (Some(a), Some(b)) if short_value(a) && short_value(b) => lines.push(format!(
                "~ {name}: {} → {}",
                without_hashes(a),
                without_hashes(b)
            )),
            (Some(_), Some(_)) => lines.push(format!("~ {name} (see source diff)")),
            (None, Some(b)) if short_value(b) => {
                lines.push(format!("+ {name} = {}", without_hashes(b)))
            }
            (None, Some(_)) => lines.push(format!("+ {name}")),
            (Some(_), None) => lines.push(format!("- {name}")),
            (None, None) => {}
        }
    }
    lines
}

/// Drop the top-level `flake.lock` from a patch.
fn without_lock(patch: &str) -> String {
    let mut out = String::new();
    for (i, chunk) in patch.split("diff --git ").enumerate() {
        if i == 0 {
            out.push_str(chunk);
        } else if !chunk.starts_with("a/flake.lock b/flake.lock\n") {
            out.push_str("diff --git ");
            out.push_str(chunk);
        }
    }
    out
}

/// `git diff --no-index` between two store trees; both are immutable and
/// world-readable, and no user configuration or external diff runs. Returns
/// a per-file summary and the patch, with paths relative to the flake.
fn source_diff(git: Option<&Path>, old: &Path, new: &Path) -> (Vec<String>, String) {
    if old == new {
        return (vec![], String::new());
    }
    let (Some(git), Ok(a), Ok(b)) = (
        git,
        old.strip_prefix("/nix/store"),
        new.strip_prefix("/nix/store"),
    ) else {
        return (vec!["changed (no diff available)".into()], String::new());
    };
    let (a, b) = (a.display().to_string(), b.display().to_string());
    let run = |args: &[&str]| -> String {
        use std::process::{Command, Stdio};
        let child = Command::new(git)
            .current_dir("/nix/store")
            .env_clear()
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("HOME", "/nonexistent")
            .args([
                "-c",
                "core.quotepath=false",
                "diff",
                "--no-index",
                "--no-ext-diff",
            ])
            .args(["--no-color", "--no-textconv", "--no-renames"])
            .args(args)
            .args([&a, &b])
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn();
        let Ok(mut child) = child else {
            return String::new();
        };
        let mut output = Vec::new();
        let _ = child
            .stdout
            .take()
            .unwrap()
            .take(MAX_DIFF_BYTES as u64 + 1)
            .read_to_end(&mut output);
        let _ = child.kill();
        let _ = child.wait();
        let mut text = String::from_utf8_lossy(&output).into_owned();
        if text.len() > MAX_DIFF_BYTES {
            text.truncate(text.floor_char_boundary(MAX_DIFF_BYTES));
            text.push_str("\n… (diff truncated)\n");
        }
        text
    };
    let files: Vec<String> = run(&["--numstat"])
        .lines()
        .filter_map(|line| {
            let mut parts = line.splitn(3, '\t');
            let (added, removed, path) = (parts.next()?, parts.next()?, parts.next()?);
            let path = match path.split_once("}/") {
                Some((_, rest)) => rest.to_string(),
                None => {
                    let side = match path.split_once(" => ") {
                        Some(("/dev/null", new)) => new,
                        Some((old, _)) => old,
                        None => path,
                    };
                    side.strip_prefix(&format!("{b}/"))
                        .or_else(|| side.strip_prefix(&format!("{a}/")))
                        .unwrap_or(side)
                        .to_string()
                }
            };
            Some(format!("{path} +{added} −{removed}"))
        })
        .collect();
    let mut patch = run(&[]);
    for side in [&a, &b] {
        for prefix in ["a/", "b/"] {
            patch = patch.replace(&format!("{prefix}{side}/"), prefix);
        }
    }
    // The lock is shown as input changes, not as JSON.
    let files = files
        .into_iter()
        .filter(|f: &String| !f.starts_with("flake.lock "))
        .collect();
    (files, without_lock(&patch))
}

fn describe(
    current: &DevShell,
    generation: u32,
    lock: &Value,
    summary: &Summary,
    cost: &Cost,
    config: bool,
    (files, patch): (Vec<String>, String),
) -> String {
    let mut text = format!(
        "Dev shell {} · generation {} → {generation}\n",
        current.reference, current.generation
    );
    let section = |text: &mut String, title: &str, lines: Vec<String>| {
        if lines.is_empty() {
            text.push_str(&format!("{title:<10}unchanged\n"));
        }
        for (i, line) in lines.iter().enumerate() {
            text.push_str(&format!("{:<10}{line}\n", if i == 0 { title } else { "" }));
        }
    };
    let notes = attention(current, lock, summary, cost, config);
    if notes.is_empty() {
        text.push_str("Attention nothing unusual · inputs locked to allowed sources\n");
    } else {
        section(&mut text, "Attention", notes);
    }
    section(&mut text, "Source", files);
    section(&mut text, "Inputs", input_lines(&current.lock, lock));
    section(
        &mut text,
        "Packages",
        package_lines(&current.summary, summary),
    );
    section(&mut text, "Env", env_lines(&current.summary, summary));
    text.push_str(&format!(
        "Cost      {} to fetch{} · {} to build\n",
        cost.fetched.len(),
        cost.download
            .as_ref()
            .map(|d| format!(" ({d})"))
            .unwrap_or_default(),
        cost.built.len()
    ));
    // Everything new to this machine, including transitive dependencies.
    let incoming: Vec<String> = incoming_lines("build", &cost.built, 40)
        .into_iter()
        .chain(incoming_lines("fetch", &cost.fetched, 40))
        .collect();
    if !incoming.is_empty() {
        section(&mut text, "Incoming", incoming);
    }
    if !patch.trim().is_empty() {
        text.push_str("\nSource diff:\n");
        text.push_str(&patch);
    }
    text
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
    fn lock_validation_allows_only_content_locked_public_sources() {
        let lock = |locked: Value| json!({"nodes": {"root": {"inputs": {"x": "x"}}, "x": {"locked": locked}}, "version": 7});
        for ok in [
            json!({"type":"github","owner":"NixOS","repo":"nixpkgs","rev":"a","narHash":"sha256-x"}),
            json!({"type":"tarball","url":"https://example.org/x.tar.gz","narHash":"sha256-x"}),
            json!({"type":"path","path":"/nix/store/abc-source","narHash":"sha256-x"}),
            json!({"type":"path","path":"./sub"}),
        ] {
            assert!(validate_lock(&lock(ok.clone())).is_ok(), "{ok}");
        }
        for bad in [
            json!({"type":"path","path":"/home/user/.ssh","narHash":"sha256-x"}),
            json!({"type":"path","path":"../outside"}),
            json!({"type":"git","url":"file:///home/user/repo","narHash":"sha256-x"}),
            json!({"type":"git","url":"ssh://git@example.org/x","narHash":"sha256-x"}),
            json!({"type":"tarball","url":"http://example.org/x.tar.gz","narHash":"sha256-x"}),
            json!({"type":"github","owner":"a","repo":"b"}),
            json!({"type":"mercurial","url":"https://example.org/x","narHash":"sha256-x"}),
        ] {
            assert!(validate_lock(&lock(bad.clone())).is_err(), "{bad}");
        }
    }

    #[test]
    fn dry_run_cost_names_what_enters_the_machine() {
        let h = "0123456789abcdfghijklmnpqrsvwxyz";
        let text = format!(
            "these 2 derivations will be built:\n  /nix/store/{h}-nix-shell.drv\n  /nix/store/{h}-tool-1.2.drv\n\
             these 2 paths will be fetched (1.5 MiB download, 6 MiB unpacked):\n  /nix/store/{h}-glibc-2.42\n  /nix/store/{h}-glibc-2.42-bin\n"
        );
        let cost = cost(&text, Path::new(&format!("/nix/store/{h}-nix-shell.drv")));
        assert_eq!(cost.built, vec!["tool-1.2"]);
        assert_eq!(cost.fetched, vec!["glibc-2.42", "glibc-2.42-bin"]);
        assert_eq!(cost.download.as_deref(), Some("1.5 MiB"));
        assert_eq!(
            incoming_lines("fetch", &cost.fetched, 40),
            vec!["[fetch] glibc 2.42, 2.42-bin"]
        );
        let many: Vec<String> = (0..45).map(|i| format!("p{i}-1.0")).collect();
        assert_eq!(
            incoming_lines("build", &many, 40).last().unwrap(),
            "… and 5 more"
        );
    }

    #[test]
    fn lock_chunks_are_left_out_of_the_source_patch() {
        let patch = "diff --git a/flake.nix b/flake.nix\n-a\n+b\ndiff --git a/flake.lock b/flake.lock\n-x\n+y\n";
        assert_eq!(
            without_lock(patch),
            "diff --git a/flake.nix b/flake.nix\n-a\n+b\n"
        );
    }

    #[test]
    fn packages_dates_and_input_changes_are_readable() {
        assert_eq!(package("hello-2.12.3"), ("hello".into(), "2.12.3".into()));
        assert_eq!(
            package("rust-analyzer-2026-09-01"),
            ("rust-analyzer".into(), "2026-09-01".into())
        );
        assert_eq!(
            package("source-stdenv.sh"),
            ("source-stdenv.sh".into(), String::new())
        );
        assert_eq!(date(0), "1970-01-01");
        assert_eq!(date(1_790_549_635), "2026-09-27");
        let node = |rev: &str, t: i64| json!({"locked":{"type":"github","owner":"NixOS","repo":"nixpkgs","rev":rev,"lastModified":t,"narHash":"sha256-x"}});
        let old = json!({"nodes":{"root":{},"nixpkgs":node("9f3c1a2aaaa", 0)}});
        let new = json!({"nodes":{"root":{},"nixpkgs":node("4be07d1bbbb", 1_790_549_635),"extra":node("1a2b3c4cccc", 0)}});
        assert_eq!(
            input_lines(&old, &new),
            vec![
                "+ extra  github:NixOS/nixpkgs @ 1a2b3c4".to_string(),
                "~ nixpkgs  github:NixOS/nixpkgs  9f3c1a2 → 4be07d1 (2026-09-27)".to_string(),
            ]
        );
        assert_eq!(
            without_hashes("/nix/store/0123456789abcdfghijklmnpqrsvwxyz-x/bin"),
            "/nix/store/…-x/bin"
        );
    }

    #[test]
    fn changed_lock_paths_name_the_difference() {
        let mut out = Vec::new();
        changed_paths(
            &json!({"nodes":{"nixpkgs":{"locked":{"rev":"a","narHash":"x"}}}}),
            &json!({"nodes":{"nixpkgs":{"locked":{"rev":"b","narHash":"x"}}}}),
            "",
            &mut out,
            5,
        );
        assert_eq!(out, vec!["nodes.nixpkgs.locked.rev"]);
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
