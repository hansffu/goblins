use crate::Result;
use serde::Deserialize;
use std::{
    collections::BTreeMap,
    fs::OpenOptions,
    io::Read,
    os::unix::fs::OpenOptionsExt,
    path::{Path, PathBuf},
};

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
    /// Configurations this one may launch as children. Nix always writes it
    /// on top-level configurations; absent (an older build) means itself only.
    #[serde(default)]
    pub allowed_children: Option<Vec<String>>,
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
#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
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
#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Editor {
    /// Only `emacs` exists: `command` is emacsclient-compatible.
    pub kind: String,
    pub command: String,
}
// Host manifests contain only launch metadata, not package contents. Bound the
// read and reject special files so a mistaken FIFO/device cannot stall startup.
const MAX_MANIFEST_BYTES: u64 = 1024 * 1024;
/// Bound on a configuration's `allowed_children`, as in mkGoblins.
pub const MAX_CHILDREN: usize = 64;
fn read_regular(path: &Path, limit: u64) -> Result<Vec<u8>> {
    let file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NONBLOCK)
        .open(path)
        .map_err(|e| format!("cannot open configuration {}: {e}", path.display()))?;
    if !file.metadata()?.is_file() {
        return Err("configuration must be a regular file".into());
    }
    let mut bytes = Vec::new();
    file.take(limit + 1).read_to_end(&mut bytes)?;
    if bytes.len() as u64 > limit {
        return Err(format!("configuration exceeds {} MiB", limit / (1024 * 1024)).into());
    }
    Ok(bytes)
}
impl Manifest {
    pub fn read(path: &Path) -> Result<Self> {
        serde_json::from_slice(&read_regular(path, MAX_MANIFEST_BYTES)?)
            .map_err(|e| format!("invalid configuration {}: {e}", path.display()).into())
    }
    /// The configured host editor, if any.
    pub fn editor(&self) -> Result<Option<Editor>> {
        match &self.editor {
            Some(editor)
                if editor.kind != "emacs"
                    || editor.command.is_empty()
                    || editor.command.contains('\0') =>
            {
                Err("invalid editor configuration".into())
            }
            editor => Ok(editor.clone()),
        }
    }
    pub fn select(mut self, name: &str, scope: Option<&str>) -> Result<Configuration> {
        self.check()?;
        let config = self.goblins.remove(name).ok_or_else(|| {
            format!(
                "unknown goblin; available: {}",
                self.goblins.keys().cloned().collect::<Vec<_>>().join(", ")
            )
        })?;
        self.resolve(name, config, scope)
    }
    /// Select the root like `select`, then snapshot every configuration its
    /// `allowed_children` reach, resolved for the root's actual scope.
    pub(crate) fn branch(
        self,
        name: &str,
        scope: Option<&str>,
    ) -> Result<(Configuration, crate::branch::Branch)> {
        self.check()?;
        let config = self.goblins.get(name).cloned().ok_or_else(|| {
            format!(
                "unknown goblin; available: {}",
                self.goblins.keys().cloned().collect::<Vec<_>>().join(", ")
            )
        })?;
        let root = self.resolve(name, config, scope)?;
        let selected = root.selected_scope.as_ref().map(|s| s.name.clone());
        let branch = crate::branch::Branch::snapshot(
            name,
            |key| self.children(key),
            |key| self.select_inherited(key, selected.as_deref()),
        );
        Ok((root, branch))
    }
    fn check(&self) -> Result<()> {
        if self.api != 2 || self.helper_api != 5 {
            return Err("incompatible manifest/helper API".into());
        }
        for (scope, definition) in &self.scopes {
            if !goblins_protocol::scope_name(scope)
                || definition
                    .storage
                    .iter()
                    .any(|s| !goblins_protocol::scope_name(s))
            {
                return Err("invalid scope catalog".into());
            }
        }
        for (name, config) in &self.goblins {
            if let Some(children) = &config.allowed_children
                && (children.len() > MAX_CHILDREN
                    || !children.iter().all(|c| goblins_protocol::identifier(c)))
            {
                return Err(format!(
                    "invalid allowed_children for '{name}': at most {MAX_CHILDREN} names of at most 64 characters"
                )
                .into());
            }
        }
        Ok(())
    }
    /// A top-level configuration's permitted children; unknown names have none.
    fn children(&self, name: &str) -> Vec<String> {
        match self.goblins.get(name) {
            Some(config) => config
                .allowed_children
                .clone()
                .unwrap_or_else(|| vec![name.to_string()]),
            None => vec![],
        }
    }
    /// Resolve a child configuration for the scope its branch runs in. A child
    /// never selects a scope: a different default scope is ignored.
    fn select_inherited(&self, name: &str, scope: Option<&str>) -> Result<Configuration> {
        let config = self
            .goblins
            .get(name)
            .cloned()
            .ok_or_else(|| format!("unknown goblin '{name}'"))?;
        match (scope, &config.scope) {
            (None, Some(required)) => {
                Err(format!("'{name}' requires scope '{required}'; its parent has no scope").into())
            }
            (Some(scope), default)
                if default.as_deref() != Some(scope)
                    && !config.allowed_scopes.iter().any(|s| s == scope) =>
            {
                Err(format!("'{name}' does not allow scope '{scope}'").into())
            }
            _ => self.resolve(name, config, scope),
        }
    }
    fn resolve(
        &self,
        name: &str,
        mut config: Configuration,
        scope: Option<&str>,
    ) -> Result<Configuration> {
        let permitted: Vec<_> = config
            .scope
            .iter()
            .chain(&config.allowed_scopes)
            .cloned()
            .collect();
        if permitted
            .iter()
            .any(|s| !self.scopes.contains_key(s) || !config.scopes.contains_key(s))
        {
            return Err("configuration references an undeclared scope".into());
        }
        let Some(selected) = scope.map(str::to_string).or(config.scope.clone()) else {
            if !config.scope_storage.is_empty() {
                return Err("scope storage requires a scope".into());
            }
            return Ok(config);
        };
        if !permitted.contains(&selected) {
            return Err(format!(
                "scope '{selected}' is not allowed for '{name}'; allowed: {}",
                permitted.join(", ")
            )
            .into());
        }
        let definition = self.scopes[&selected].clone();
        let mut variant = config.scopes.remove(&selected).unwrap();
        let mut mounts = BTreeMap::new();
        for (storage, raw) in &variant.scope_storage {
            // Expanded like file grants, with the host environment.
            let path = crate::binds::expand(raw, |name| std::env::var(name).ok())?;
            if !definition.storage.contains(storage) || path.parent().is_none() {
                return Err(format!("invalid scope storage mount '{storage}'").into());
            }
            crate::binds::validate(&path, &[])?;
            mounts.insert(storage.clone(), path);
        }
        variant.selected_scope = Some(Scope {
            name: selected,
            definition,
            mounts,
        });
        Ok(variant)
    }
}
/// Trusted launch configuration, never accepted over the sandbox socket.
#[derive(Clone, Deserialize)]
pub struct Launch {
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub pasta: Option<PathBuf>,
    #[serde(skip)]
    pub cwd: Option<PathBuf>,
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
    #[serde(default)]
    pub env: BTreeMap<String, String>,
    #[serde(default)]
    pub binds: crate::binds::Declarations,
    #[serde(skip)]
    pub protected_paths: Vec<PathBuf>,
    #[serde(default)]
    pub sandbox_etc: BTreeMap<String, PathBuf>,
    #[serde(default)]
    pub dev_shell_sandbox_etc: BTreeMap<String, PathBuf>,
    #[serde(default)]
    pub integration: Option<String>,
    #[serde(default)]
    pub docker: Option<Docker>,
    #[serde(skip)]
    pub scope: Option<Scope>,
    /// Host-evaluated flake dev shell; children inherit this generation.
    #[serde(skip)]
    pub dev_shell: Option<crate::devshell::DevShell>,
    /// The manifest's host editor; children inherit it.
    #[serde(skip)]
    pub editor: Option<Editor>,
}
impl Launch {
    /// The sandbox's /etc mounts, including the dev-shell-only files when it
    /// has a dev shell.
    pub fn etc_mounts(&self) -> Result<Vec<(PathBuf, PathBuf)>> {
        let dev_shell = match self.dev_shell {
            Some(_) => &self.dev_shell_sandbox_etc,
            None => &BTreeMap::new(),
        };
        crate::sandbox_etc::mounts(&self.sandbox_etc, dev_shell)
    }
}
fn default_args() -> Vec<String> {
    vec!["--noprofile".into(), "--norc".into()]
}
impl Configuration {
    pub fn launch(&self) -> Result<Launch> {
        let spec: serde_json::Value =
            serde_json::from_slice(&read_regular(&self.build_spec, MAX_MANIFEST_BYTES)?)?;
        if spec["platform"] != "linux"
            || spec["allow_nix"] != false
            || spec["allow_unix_sockets"] != true
        {
            return Err("unsupported Goblin sandbox policy".into());
        }
        for key in ["allowed_host_ports", "published_ports"] {
            if !spec[key].as_array().is_some_and(|v| v.is_empty()) {
                return Err("live launcher does not support network grants".into());
            }
        }
        let string = |key: &str| {
            spec[key]
                .as_str()
                .ok_or_else(|| format!("missing build spec string: {key}"))
        };
        let mut env = BTreeMap::from([
            ("PKG_CONFIG_PATH".into(), string("pkg_config_path")?.into()),
            ("SSL_CERT_DIR".into(), string("cacert_dir")?.into()),
            ("SSL_CERT_FILE".into(), string("cacert_bundle")?.into()),
        ]);
        env.extend(self.env.clone());
        Ok(Launch {
            description: self.description.clone(),
            pasta: if self.network {
                Some(
                    spec["dependencies"]["pasta"]
                        .as_str()
                        .ok_or("missing pasta")?
                        .into(),
                )
            } else {
                None
            },
            cwd: None,
            shell: string("sandboxed_binary")?.into(),
            shell_args: self.args.clone(),
            resume_command: self.resume_command.clone(),
            posix_shell: string("shell")?.into(),
            helper: self.helper.clone(),
            bwrap: spec["dependencies"]["bwrap"]
                .as_str()
                .ok_or("missing bwrap")?
                .into(),
            flake: self.flake.clone(),
            client_package: self.client_package.clone(),
            initial_packages: string("sandbox_path")?
                .split(':')
                .filter(|p| !p.is_empty())
                .map(|p| Path::new(p).parent().unwrap().to_path_buf())
                .collect(),
            initial_closure: String::from_utf8(read_regular(
                Path::new(string("closure_paths_file")?),
                8 * MAX_MANIFEST_BYTES,
            )?)?
            .lines()
            .map(PathBuf::from)
            .collect(),
            env,
            protected_paths: vec![],
            sandbox_etc: self.sandbox_etc.clone(),
            dev_shell_sandbox_etc: self.dev_shell_sandbox_etc.clone(),
            integration: self.integration.clone(),
            docker: self.docker.clone(),
            scope: self.selected_scope.clone(),
            dev_shell: None,
            editor: None,
            binds: serde_json::from_value(serde_json::json!({
                "rw_dirs": spec["rw_dirs"], "rw_files": spec["rw_files"],
                "ro_dirs": spec["ro_dirs"], "ro_files": spec["ro_files"],
            }))?,
        })
    }
}

/// A `resume_command` with its `{key}` placeholders filled from recorded
/// variables (`{{` and `}}` are literal braces), or why it cannot be used.
/// Each element stays one argument; no shell is involved.
pub fn resume_arguments(
    template: &[String],
    variables: &BTreeMap<String, String>,
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
                        if !closed || !goblins_protocol::variable_key(&key) {
                            return Err(format!(
                                "resume command element '{element}' has an invalid placeholder"
                            ));
                        }
                        let value = variables
                            .get(&key)
                            .ok_or_else(|| format!("variable '{key}' is not recorded"))?;
                        // An option-like value could change what the agent does.
                        if value.starts_with('-') {
                            return Err(format!("variable '{key}' starts with '-'"));
                        }
                        argument.push_str(value);
                    }
                    '}' => {
                        return Err(format!(
                            "resume command element '{element}' has an unmatched '}}'"
                        ));
                    }
                    c => argument.push(c),
                }
            }
            Ok(argument)
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn resume_arguments_fill_placeholders_or_say_why_not() {
        let variables = BTreeMap::from([
            ("session_id".to_string(), "c8193c93".to_string()),
            ("session_name".to_string(), "Plan it; $(false)".to_string()),
            ("flag".to_string(), "--dangerous".to_string()),
        ]);
        let template = |parts: &[&str]| parts.iter().map(|p| p.to_string()).collect::<Vec<_>>();
        assert_eq!(
            resume_arguments(
                &template(&[
                    "-p",
                    "--resume",
                    "{session_id}",
                    "--name={session_name}",
                    "{{x}}"
                ]),
                &variables
            ),
            Ok(template(&[
                "-p",
                "--resume",
                "c8193c93",
                "--name=Plan it; $(false)",
                "{x}"
            ]))
        );
        assert_eq!(
            resume_arguments(&template(&["{missing}"]), &variables),
            Err("variable 'missing' is not recorded".into())
        );
        assert_eq!(
            resume_arguments(&template(&["--resume={flag}"]), &variables),
            Err("variable 'flag' starts with '-'".into())
        );
        for bad in ["{session_id", "close}", "{Bad}", "{}", "{{a}"] {
            assert!(
                resume_arguments(&template(&[bad]), &variables).is_err(),
                "{bad}"
            );
        }
    }
}
