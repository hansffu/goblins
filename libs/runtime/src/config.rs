use crate::Result;
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
    /// Pinned private scope executable, resolved with the configuration.
    #[serde(default)]
    pub scope_runtime: PathBuf,
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
    #[serde(skip)]
    pub selected_scope: Option<Scope>,
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
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ScopeDefinition {
    pub persistent: bool,
    pub pid: bool,
    #[serde(default)]
    pub storage: Vec<String>,
    #[serde(default)]
    pub docker: bool,
}
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
}
impl ManifestLease {
    /// A lease on `path` that removes its `root`, if it has one, once the last
    /// holder drops it.
    pub fn adopt(path: PathBuf, root: Option<PathBuf>) -> Self {
        Self { path, root }
    }
}
#[cfg(any(test, feature = "test-support"))]
impl ManifestLease {
    /// A lease on a manifest that is not in the store, for tests.
    pub fn unrooted(path: &std::path::Path) -> Self {
        Self {
            path: path.to_path_buf(),
            root: None,
        }
    }
}
impl Drop for ManifestLease {
    fn drop(&mut self) {
        if let Some(root) = &self.root {
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
    /// Pinned private scope executable, resolved with the configuration.
    #[serde(default)]
    pub scope_runtime: PathBuf,
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
