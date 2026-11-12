mod manifest;
use crate::Result;
pub use manifest::{
    allows_nix, build_spec, editor, read, run_targets, select, select_inherited, validate,
    worktrees_directory,
};
use serde::{Deserialize, Serialize};
use std::{collections::BTreeMap, path::PathBuf};

#[derive(Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Configuration {
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub network: bool,
    pub build_spec: PathBuf,
    pub args: Vec<String>,
    /// The agent's arguments for a recreated goblin, instead of `args`, with
    /// `{key}` placeholders for its recorded variables.
    #[serde(default)]
    pub resume_command: Option<Vec<String>>,
    /// Run the agent without a terminal and start this client at each attach.
    #[serde(default)]
    pub on_attach: Option<OnAttach>,
    #[serde(default)]
    pub on_detach: Option<Vec<String>>,
    pub env: BTreeMap<String, String>,
    pub client_package: PathBuf,
    pub helper: PathBuf,
    pub flake: String,
    #[serde(default)]
    pub sandbox_etc: BTreeMap<String, PathBuf>,
    /// Further /etc files mounted only in sandboxes with a flake dev shell.
    #[serde(default)]
    pub dev_shell_sandbox_etc: BTreeMap<String, PathBuf>,
    #[serde(default)]
    pub integration: Option<String>,
    #[serde(default)]
    pub docker: Option<Docker>,
    /// mkGoblin's hostCommands.enable: `goblin host run` may ask the host.
    #[serde(default)]
    pub host_commands: bool,
    /// mkGoblin's bypassPermissions: the agent launcher skips the agent's own
    /// permission prompts. Only shown; the launcher already applies it.
    #[serde(default)]
    pub bypass_permissions: bool,
    /// Default scope; `allowed_scopes` may be selected at launch instead.
    #[serde(default)]
    pub scope: Option<String>,
    #[serde(default)]
    pub allowed_scopes: Vec<String>,
    /// Scope storage name to its path in the sandbox ($HOME may be used).
    #[serde(default)]
    pub scope_storage: BTreeMap<String, String>,
    /// The same goblin with each permitted scope's defaults merged in by Nix.
    #[serde(default)]
    pub scopes: BTreeMap<String, Configuration>,
    /// Configurations this one may run as raid members; none when absent.
    #[serde(default)]
    pub allowed_run_targets: Vec<String>,
    /// mkGoblin's `worktrees` (docs/specs/WORKTREES.org).
    #[serde(default)]
    pub worktrees: WorktreeOptions,
    /// mkGoblin's `launchOptions`, on top-level configurations only
    /// (docs/specs/LAUNCH-OPTIONS.org).
    #[serde(default)]
    pub launch_options: BTreeMap<String, LaunchOption>,
    #[serde(skip)]
    pub selected_scope: Option<Scope>,
}
/// A launch option a raid owner picks with `goblin run -o NAME=CHOICE`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LaunchOption {
    #[serde(default)]
    pub description: Option<String>,
    pub choices: BTreeMap<String, LaunchChoice>,
}
/// The arguments a choice adds after the configuration's `args`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LaunchChoice {
    #[serde(default)]
    pub description: Option<String>,
    pub args: Vec<String>,
}
/// What the daemon needs from a manifest to check launch options and list
/// run targets without reading the manifest again
/// (docs/specs/LAUNCH-OPTIONS.org). Built where the manifest is read.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Catalogue {
    pub goblins: BTreeMap<String, CatalogueEntry>,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CatalogueEntry {
    pub description: String,
    pub args: Vec<String>,
    /// The `args` of each scope variant, which a member in that scope runs.
    pub scope_args: BTreeMap<String, Vec<String>>,
    pub launch_options: BTreeMap<String, LaunchOption>,
}
impl Catalogue {
    pub fn new(manifest: &Manifest) -> Self {
        let goblins = manifest
            .goblins
            .iter()
            .map(|(name, config)| {
                let entry = CatalogueEntry {
                    description: config.description.clone(),
                    args: config.args.clone(),
                    scope_args: config
                        .scopes
                        .iter()
                        .map(|(scope, variant)| (scope.clone(), variant.args.clone()))
                        .collect(),
                    launch_options: config.launch_options.clone(),
                };
                (name.clone(), entry)
            })
            .collect();
        Self { goblins }
    }
    /// The complete arguments of a member of `configuration` started in the
    /// owner's `scope` with these choices: the variant's `args`, then each
    /// chosen choice's `args` in the order of the option names. None without
    /// choices, which leaves the launch as it is; an error names the valid
    /// options or choices.
    pub fn arguments(
        &self,
        configuration: &str,
        scope: Option<&str>,
        options: &BTreeMap<String, String>,
    ) -> std::result::Result<Option<Vec<String>>, String> {
        if options.is_empty() {
            return Ok(None);
        }
        let entry = self
            .goblins
            .get(configuration)
            .ok_or_else(|| format!("unknown goblin '{configuration}'"))?;
        let names = |keys: Vec<&String>| match keys.is_empty() {
            true => "none".to_string(),
            false => keys.into_iter().cloned().collect::<Vec<_>>().join(", "),
        };
        let mut arguments = scope
            .and_then(|s| entry.scope_args.get(s))
            .unwrap_or(&entry.args)
            .clone();
        for (option, choice) in options {
            let declared = entry.launch_options.get(option).ok_or_else(|| {
                format!(
                    "unknown launch option '{option}'; options: {}",
                    names(entry.launch_options.keys().collect())
                )
            })?;
            let chosen = declared.choices.get(choice).ok_or_else(|| {
                format!(
                    "option '{option}' has no choice '{choice}'; choices: {}",
                    names(declared.choices.keys().collect())
                )
            })?;
            arguments.extend(chosen.args.iter().cloned());
        }
        Ok(Some(arguments))
    }
}
/// mkGoblin's `worktrees.enable`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorktreeOptions {
    /// False: no worktree folder, no `GOBLINS_WORKTREES`, no `goblin worktree`.
    #[serde(default = "enabled")]
    pub enable: bool,
}
impl Default for WorktreeOptions {
    fn default() -> Self {
        Self { enable: true }
    }
}
fn enabled() -> bool {
    true
}
/// mkGoblins' `worktrees`: where Git worktrees live, from the manifest.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Worktrees {
    /// An absolute or `$HOME`-relative path, expanded like file grants from the
    /// daemon's environment. Absent turns worktrees off.
    #[serde(default)]
    pub directory: Option<String>,
}
#[derive(Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Docker {
    pub daemon: PathBuf,
    #[serde(default)]
    pub client: Option<PathBuf>,
    /// Attach to the scope's engine at startup.
    #[serde(default)]
    pub enabled: bool,
}
pub use goblins_protocol::contract::scope_metadata::ScopeDefinition;
/// The scope a launch joins, with this goblin's storage mount points.
#[derive(Clone, Debug)]
pub struct Scope {
    pub name: String,
    pub definition: ScopeDefinition,
    pub mounts: BTreeMap<String, PathBuf>,
}
#[derive(Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Manifest {
    pub api: u32,
    pub helper_api: u32,
    pub goblins: BTreeMap<String, Configuration>,
    #[serde(default)]
    pub scopes: BTreeMap<String, ScopeDefinition>,
    /// The host editor sandboxes may open files in (`editor.emacs.enable`).
    #[serde(default)]
    pub editor: Option<Editor>,
    /// mkGoblins' `worktrees`.
    #[serde(default)]
    pub worktrees: Worktrees,
}
/// A host editor command, run without a shell with editor-specific arguments.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Editor {
    /// Only `emacs` exists: `command` is emacsclient-compatible.
    pub kind: String,
    pub command: String,
}
pub const MAX_RUN_TARGETS: usize = 64;

/// A validated manifest the daemon may launch goblins from, kept from
/// garbage collection while anything holds the lease: the daemon while it is
/// current, and every launch accepted against it until that launch started.
#[derive(Debug)]
pub struct ManifestLease {
    pub path: PathBuf,
    root: Option<PathBuf>,
    // Used only across the supervisor channel: both peers retain a shared
    // ownership lock until adoption/cleanup, so a lost acknowledgement can
    // neither unroot a live receiver nor leak a rejected root.
    transfer: Option<std::fs::File>,
}
impl ManifestLease {
    /// A lease on `path` that removes its `root`, if it has one, once the last
    /// holder drops it.
    pub fn adopt(path: PathBuf, root: Option<PathBuf>) -> Self {
        Self {
            path,
            root,
            transfer: None,
        }
    }
}
impl ManifestLease {
    fn transfer_path(root: &std::path::Path) -> PathBuf {
        let mut path = root.as_os_str().to_os_string();
        path.push(".ownership");
        path.into()
    }
    /// Reserve an absent root for construction. Keep exclusive ownership until
    /// the creating command and its process group have finished.
    pub fn begin_root(path: PathBuf, root: PathBuf) -> Result<Self> {
        let lock = crate::lease::flock(&Self::transfer_path(&root), true)?
            .ok_or_else(|| format!("manifest root is already owned: {}", root.display()))?;
        match std::fs::symlink_metadata(&root) {
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => (),
            Err(e) => return Err(e.into()),
            Ok(_) => return Err(format!("manifest root already exists: {}", root.display()).into()),
        }
        // No root-removing guard exists before the absence check succeeds.
        Ok(Self {
            path,
            root: Some(root),
            transfer: Some(lock),
        })
    }
    /// Verify creation before allowing independent shared receiver ownership.
    /// Lock conversions need not be atomic: a competing constructor in the
    /// conversion gap must now reject the existing root before adopting it.
    pub fn finish_root(&mut self) -> Result<()> {
        self.finish_root_with(|fd| {
            goblins_unix::cvt(unsafe { libc::flock(fd, libc::LOCK_SH | libc::LOCK_NB) }).map(|_| ())
        })
    }
    fn finish_root_with(
        &mut self,
        downgrade: impl FnOnce(std::os::fd::RawFd) -> std::io::Result<()>,
    ) -> Result<()> {
        use std::os::fd::AsRawFd;
        let root = self.root.as_ref().ok_or("manifest has no root")?;
        if !std::fs::symlink_metadata(root)?.file_type().is_symlink() {
            return Err("manifest root must be a symlink".into());
        }
        let lock = self
            .transfer
            .as_ref()
            .ok_or("manifest root has no construction owner")?;
        downgrade(lock.as_raw_fd())?;
        Ok(())
    }
    pub(crate) fn root(&self) -> Option<&std::path::Path> {
        self.root.as_deref()
    }
    pub(crate) fn prepare_transfer(&mut self) -> crate::Result<Option<std::os::fd::OwnedFd>> {
        let Some(root) = &self.root else {
            return Ok(None);
        };
        let path = Self::transfer_path(root);
        if self.transfer.is_none() {
            self.transfer =
                Some(crate::lease::flock(&path, false)?.ok_or("manifest root is being collected")?);
        }
        Ok(Some(
            crate::lease::flock(&path, false)?
                .ok_or("manifest root is being collected")?
                .into(),
        ))
    }
    pub(crate) fn from_transfer(
        path: PathBuf,
        root: Option<PathBuf>,
        lock: Option<std::os::fd::OwnedFd>,
    ) -> crate::Result<Self> {
        use std::os::unix::fs::MetadataExt;
        let transfer = match (&root, lock) {
            (None, None) => None,
            (Some(root), Some(lock)) => {
                let file = std::fs::File::from(lock);
                let actual = file.metadata()?;
                let expected = std::fs::symlink_metadata(Self::transfer_path(root))?;
                if !actual.is_file()
                    || !expected.is_file()
                    || actual.uid() != unsafe { libc::getuid() }
                    || (actual.dev(), actual.ino()) != (expected.dev(), expected.ino())
                {
                    return Err("invalid manifest ownership descriptor".into());
                }
                Some(file)
            }
            _ => return Err("manifest root ownership descriptor missing or unexpected".into()),
        };
        Ok(Self {
            path,
            root,
            transfer,
        })
    }
}
#[cfg(any(test, feature = "test-support"))]
impl ManifestLease {
    /// A lease on a manifest that is not in the store, for tests.
    pub fn unrooted(path: &std::path::Path) -> Self {
        Self {
            path: path.to_path_buf(),
            root: None,
            transfer: None,
        }
    }
}
impl Drop for ManifestLease {
    fn drop(&mut self) {
        if let Some(root) = &self.root {
            let _exclusive = if let Some(lock) = self.transfer.take() {
                drop(lock);
                // Keep the lock inode stable, as with other GC leases, and
                // retain exclusivity through the actual root removal.
                match crate::lease::flock(&Self::transfer_path(root), true) {
                    Ok(Some(lock)) => Some(lock),
                    _ => return,
                }
            } else {
                None
            };
            let _ = std::fs::remove_file(root);
        }
    }
}
/// mkGoblin's `onAttach`: the client started on a new terminal at each attach.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OnAttach {
    pub command: Vec<String>,
    /// Seconds an attach waits for the variables the client needs.
    pub timeout: u64,
    /// Variables to wait for beyond the command's own placeholders.
    #[serde(default)]
    pub wait_for: Vec<String>,
}
impl OnAttach {
    /// Every variable an attach waits for.
    pub fn needs(&self) -> Vec<String> {
        let mut names = placeholders(&self.command);
        names.extend(self.wait_for.iter().cloned());
        names.sort();
        names.dedup();
        names
    }
}
/// The attach handlers of a goblin that has `onAttach`.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Handlers {
    pub on_attach: OnAttach,
    pub on_detach: Option<Vec<String>>,
}
impl Handlers {
    pub fn check(on_attach: &OnAttach, on_detach: Option<&Vec<String>>) -> Result<()> {
        let template = |command: &Vec<String>| {
            !command.is_empty() && substitute(command, true, |key| Ok(key.to_string())).is_ok()
        };
        if !template(&on_attach.command)
            || !(1..=3600).contains(&on_attach.timeout)
            || !on_attach
                .wait_for
                .iter()
                .all(|name| goblins_protocol::variable_key(name))
            || on_detach.is_some_and(|command| !template(command))
        {
            return Err("invalid onAttach or onDetach".into());
        }
        Ok(())
    }
}

/// Trusted launch configuration, never accepted over the sandbox socket.
#[derive(Clone, Deserialize)]
pub struct Launch {
    #[serde(default)]
    pub description: String,
    /// The host store and Nix daemon are visible (mkGoblin's allowNix).
    #[serde(default)]
    pub allow_nix: bool,
    /// The sandbox may ask the host to run commands outside it.
    #[serde(default)]
    pub host_commands: bool,
    /// The configuration asks the agent to skip its own permission prompts.
    #[serde(default)]
    pub bypass_permissions: bool,
    #[serde(default)]
    pub pasta: Option<PathBuf>,
    pub shell: PathBuf,
    #[serde(default = "default_args")]
    pub shell_args: Vec<String>,
    /// Replaces `shell_args` on recreate when every placeholder has a value.
    #[serde(default)]
    pub resume_command: Option<Vec<String>>,
    pub posix_shell: PathBuf,
    pub helper: PathBuf,
    pub bwrap: PathBuf,
    pub flake: String,
    pub client_package: PathBuf,
    #[serde(default)]
    pub initial_packages: Vec<PathBuf>,
    #[serde(default)]
    pub initial_closure: Vec<PathBuf>,
    /// Registered references of this file may retain initial paths together.
    /// This is GC protection only; it never widens the sandbox's store view.
    #[serde(default)]
    pub initial_closure_file: Option<PathBuf>,
    #[serde(default)]
    pub env: BTreeMap<String, String>,
    #[serde(default)]
    pub binds: crate::binds::Declarations,
    #[serde(default)]
    pub sandbox_etc: BTreeMap<String, PathBuf>,
    #[serde(default)]
    pub dev_shell_sandbox_etc: BTreeMap<String, PathBuf>,
    #[serde(default)]
    pub integration: Option<String>,
    #[serde(default)]
    pub docker: Option<Docker>,
    /// mkGoblin's `worktrees`.
    #[serde(default)]
    pub worktrees: WorktreeOptions,
}
fn default_args() -> Vec<String> {
    vec!["--noprofile".into(), "--norc".into()]
}
/// A `resume_command` with its `{key}` placeholders filled from recorded
/// variables (`{{` and `}}` are literal braces), or why it cannot be used.
/// Each element stays one argument; no shell is involved.
pub fn resume_arguments(
    template: &[String],
    variables: &BTreeMap<String, String>,
) -> std::result::Result<Vec<String>, String> {
    substitute(template, false, |key| value(variables, key))
}
/// An attach handler filled like `resume_arguments`; `{attach.pid}` is the
/// attached client's process ID.
pub fn handler_arguments(
    template: &[String],
    variables: &BTreeMap<String, String>,
    pid: Option<i32>,
) -> std::result::Result<Vec<String>, String> {
    substitute(template, true, |key| match key {
        "attach.pid" => pid
            .map(|pid| pid.to_string())
            .ok_or_else(|| "no attached client".to_string()),
        key => value(variables, key),
    })
}
/// The variable names a template's placeholders use.
pub fn placeholders(template: &[String]) -> Vec<String> {
    let mut names = vec![];
    let _ = substitute(template, true, |key| {
        if goblins_protocol::variable_key(key) {
            names.push(key.to_string());
        }
        Ok(String::new())
    });
    names
}
fn value(variables: &BTreeMap<String, String>, key: &str) -> std::result::Result<String, String> {
    let value = variables
        .get(key)
        .ok_or_else(|| format!("variable '{key}' is not recorded"))?;
    // An option-like value could change what the agent does.
    if value.starts_with('-') {
        return Err(format!("variable '{key}' starts with '-'"));
    }
    Ok(value.clone())
}
fn substitute(
    template: &[String],
    attach: bool,
    mut value: impl FnMut(&str) -> std::result::Result<String, String>,
) -> std::result::Result<Vec<String>, String> {
    template
        .iter()
        .map(|element| {
            let mut argument = String::new();
            let mut chars = element.chars().peekable();
            while let Some(c) = chars.next() {
                match c {
                    '{' if chars.peek() == Some(&'{') => {
                        chars.next();
                        argument.push('{');
                    }
                    '}' if chars.peek() == Some(&'}') => {
                        chars.next();
                        argument.push('}');
                    }
                    '{' => {
                        let mut key = String::new();
                        let mut closed = false;
                        for c in chars.by_ref() {
                            if c == '}' {
                                closed = true;
                                break;
                            }
                            key.push(c);
                        }
                        if !closed
                            || !(goblins_protocol::variable_key(&key)
                                || attach && key == "attach.pid")
                        {
                            return Err(format!(
                                "command element '{element}' has an invalid placeholder"
                            ));
                        }
                        argument.push_str(&value(&key)?);
                    }
                    '}' => {
                        return Err(format!("command element '{element}' has an unmatched '}}'"));
                    }
                    c => argument.push(c),
                }
            }
            Ok(argument)
        })
        .collect()
}
#[cfg(test)]
mod root_tests {
    use super::*;
    #[test]
    fn failed_downgrade_keeps_attempt_cleanup_and_respects_a_racing_owner() {
        let _no_forks = goblins_unix::no_forks();
        for contended in [false, true] {
            let dir = goblins_unix::temp_directory().unwrap();
            let root = dir.join("root");
            let mut lease = ManifestLease::begin_root("/manifest".into(), root.clone()).unwrap();
            std::os::unix::fs::symlink("/owned", &root).unwrap();
            let error = lease
                .finish_root_with(|fd| {
                    // Model a failed non-atomic conversion: its EX may be lost.
                    goblins_unix::cvt(unsafe { libc::flock(fd, libc::LOCK_UN) })?;
                    Err(std::io::ErrorKind::PermissionDenied.into())
                })
                .unwrap_err();
            assert!(error.downcast_ref::<std::io::Error>().is_some());
            let racing = contended.then(|| {
                crate::lease::flock(&ManifestLease::transfer_path(&root), false)
                    .unwrap()
                    .unwrap()
            });
            drop(lease);
            assert_eq!(root.is_symlink(), contended);
            drop(racing);
            // Contended failure intentionally leaves cleanup to existing GC.
            std::fs::remove_dir_all(dir).unwrap();
        }
    }
}
