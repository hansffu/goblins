use crate::Result;
use goblins_runtime::config::Launch as Definition;
use goblins_runtime::config::{
    Configuration, Editor, Handlers, MAX_RUN_TARGETS, Manifest, ManifestLease, Scope,
};
use serde::Deserialize;
use std::{
    collections::BTreeMap,
    fs::OpenOptions,
    io::Read,
    os::unix::fs::OpenOptionsExt,
    path::{Path, PathBuf},
};
// Host manifests contain only launch metadata, not package contents. Bound the
// read and reject special files so a mistaken FIFO/device cannot stall startup.
const MAX_MANIFEST_BYTES: u64 = 1024 * 1024;
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
/// Validate `path` and root the store output that holds it under
/// `directory`, named for `sequence` so leases never share a root. The
/// root is the caller's until a lease adopts it.
pub fn root_manifest(
    path: &Path,
    directory: &Path,
    sequence: u64,
    cancel: &crate::process::Cancellation,
) -> Result<Option<PathBuf>> {
    validate(path)?;
    Ok(match crate::session::containing_store_output(path) {
        None => None,
        Some(output) => {
            goblins_unix::private_directory(directory)?;
            let name = output.file_name().unwrap().to_string_lossy();
            let link = directory.join(format!("{sequence}-{name}"));
            crate::process::command(
                std::process::Command::new("nix-store")
                    .arg("--realise")
                    .arg(&output)
                    .arg("--add-root")
                    .arg(&link)
                    .arg("--indirect"),
                directory,
                cancel,
            )?;
            Some(link)
        }
    })
}
/// Validate and root the manifest, and lease it.
pub fn lease_manifest(
    path: &Path,
    directory: &Path,
    sequence: u64,
    cancel: &crate::process::Cancellation,
) -> Result<ManifestLease> {
    let root = root_manifest(path, directory, sequence, cancel)?;
    Ok(ManifestLease::adopt(path.to_path_buf(), root))
}
pub fn read(path: &Path) -> Result<Manifest> {
    serde_json::from_slice(&read_regular(path, MAX_MANIFEST_BYTES)?)
        .map_err(|e| format!("invalid configuration {}: {e}", path.display()).into())
}
/// The configured host editor, if any.
pub fn editor(manifest: &Manifest) -> Result<Option<Editor>> {
    match &manifest.editor {
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
pub fn select(mut manifest: Manifest, name: &str, scope: Option<&str>) -> Result<Configuration> {
    check(&manifest)?;
    let config = manifest.goblins.remove(name).ok_or_else(|| {
        format!(
            "unknown goblin; available: {}",
            manifest
                .goblins
                .keys()
                .cloned()
                .collect::<Vec<_>>()
                .join(", ")
        )
    })?;
    resolve(&manifest, name, config, scope)
}
/// Read a manifest and check everything about it that does not depend
/// on the goblin selected: versions, scope catalog, run-target bounds and
/// editor. A manifest that passes may become the daemon's current one.
pub fn validate(path: &Path) -> Result<Manifest> {
    let manifest = read(path)?;
    check(&manifest)?;
    editor(&manifest)?;
    Ok(manifest)
}
fn check(manifest: &Manifest) -> Result<()> {
    if manifest.api != 2 || manifest.helper_api != 6 {
        return Err("incompatible manifest/helper API".into());
    }
    for (scope, definition) in &manifest.scopes {
        if !goblins_protocol::scope_name(scope)
            || definition
                .storage
                .iter()
                .any(|s| !goblins_protocol::scope_name(s))
        {
            return Err("invalid scope catalog".into());
        }
    }
    for (name, config) in &manifest.goblins {
        for config in std::iter::once(config).chain(config.scopes.values()) {
            match &config.on_attach {
                Some(on_attach) => Handlers::check(on_attach, config.on_detach.as_ref())
                    .map_err(|e| format!("{e} for '{name}'"))?,
                None if config.on_detach.is_some() => {
                    return Err(format!("onDetach without onAttach for '{name}'").into());
                }
                None => (),
            }
        }
        let targets = &config.allowed_run_targets;
        if targets.len() > MAX_RUN_TARGETS
            || !targets.iter().all(|c| goblins_protocol::identifier(c))
        {
            return Err(format!(
                "invalid allowed_run_targets for '{name}': at most {MAX_RUN_TARGETS} names of at most 64 characters"
            )
            .into());
        }
    }
    Ok(())
}
/// The configurations a top-level configuration may run as raid
/// members; unknown names have none. Scope variants never carry the list.
pub(crate) fn run_targets(manifest: &Manifest, name: &str) -> Vec<String> {
    match manifest.goblins.get(name) {
        Some(config) => config.allowed_run_targets.clone(),
        None => vec![],
    }
}
/// Resolve a run target for the scope its owner runs in. It never selects
/// a scope: a different default scope is ignored.
pub(crate) fn select_inherited(
    manifest: &Manifest,
    name: &str,
    scope: Option<&str>,
) -> Result<Configuration> {
    let config = manifest
        .goblins
        .get(name)
        .cloned()
        .ok_or_else(|| format!("unknown goblin '{name}'"))?;
    match (scope, &config.scope) {
        (None, Some(required)) => {
            Err(format!("'{name}' requires scope '{required}'; its owner has no scope").into())
        }
        (Some(scope), default)
            if default.as_deref() != Some(scope)
                && !config.allowed_scopes.iter().any(|s| s == scope) =>
        {
            Err(format!("'{name}' does not allow scope '{scope}'").into())
        }
        _ => resolve(&manifest, name, config, scope),
    }
}
fn resolve(
    manifest: &Manifest,
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
        .any(|s| !manifest.scopes.contains_key(s) || !config.scopes.contains_key(s))
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
    let definition = manifest.scopes[&selected].clone();
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
/// Implementation-side launch state; D moves this with the launcher.
#[derive(Clone, Deserialize)]
pub struct Launch {
    #[serde(flatten)]
    pub definition: Definition,
    #[serde(skip)]
    pub cwd: Option<PathBuf>,
    #[serde(skip)]
    pub handlers: Option<Handlers>,
    #[serde(skip)]
    pub protected_paths: Vec<PathBuf>,
    #[serde(skip)]
    pub scope: Option<Scope>,
    #[serde(skip)]
    pub dev_shell: Option<goblins_runtime::devshell::DevShell>,
    #[serde(skip)]
    pub editor: Option<Editor>,
}
impl std::ops::Deref for Launch {
    type Target = Definition;
    fn deref(&self) -> &Definition {
        &self.definition
    }
}
impl std::ops::DerefMut for Launch {
    fn deref_mut(&mut self) -> &mut Definition {
        &mut self.definition
    }
}
fn spec(config: &Configuration) -> Result<serde_json::Value> {
    Ok(serde_json::from_slice(&read_regular(
        &config.build_spec,
        MAX_MANIFEST_BYTES,
    )?)?)
}
/// mkGoblin's allowNix, from the build spec the launch also reads.
pub fn allows_nix(config: &Configuration) -> Result<bool> {
    Ok(spec(config)?["allow_nix"] == true)
}
pub fn launch(config: &Configuration) -> Result<Launch> {
    let spec = spec(config)?;
    if spec["platform"] != "linux"
        || !spec["allow_nix"].is_boolean()
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
    env.extend(config.env.clone());
    Ok(Launch {
        definition: Definition {
            description: config.description.clone(),
            allow_nix: spec["allow_nix"] == true,
            host_commands: config.host_commands,
            bypass_permissions: config.bypass_permissions,
            pasta: if config.network {
                Some(
                    spec["dependencies"]["pasta"]
                        .as_str()
                        .ok_or("missing pasta")?
                        .into(),
                )
            } else {
                None
            },
            shell: string("sandboxed_binary")?.into(),
            shell_args: config.args.clone(),
            resume_command: config.resume_command.clone(),
            posix_shell: string("shell")?.into(),
            helper: config.helper.clone(),
            scope_runtime: config.scope_runtime.clone(),
            bwrap: spec["dependencies"]["bwrap"]
                .as_str()
                .ok_or("missing bwrap")?
                .into(),
            flake: config.flake.clone(),
            client_package: config.client_package.clone(),
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
            initial_closure_file: Some(string("closure_paths_file")?.into()),
            env,
            sandbox_etc: config.sandbox_etc.clone(),
            dev_shell_sandbox_etc: config.dev_shell_sandbox_etc.clone(),
            integration: config.integration.clone(),
            docker: config.docker.clone(),
            binds: serde_json::from_value(serde_json::json!({
                "rw_dirs": spec["rw_dirs"], "rw_files": spec["rw_files"],
                "ro_dirs": spec["ro_dirs"], "ro_files": spec["ro_files"],
            }))?,
        },
        cwd: None,
        handlers: config.on_attach.clone().map(|on_attach| Handlers {
            on_attach,
            on_detach: config.on_detach.clone(),
        }),
        protected_paths: vec![],
        scope: config.selected_scope.clone(),
        dev_shell: None,
        editor: None,
    })
}
#[cfg(test)]
mod tests {
    use super::*;
    use goblins_runtime::config::{OnAttach, handler_arguments, resume_arguments};
    use serde_json::{Value, json};

    fn goblin(extra: Value) -> Value {
        let mut value = json!({
            "build_spec": "/nix/store/00000000000000000000000000000000-spec.json",
            "args": [], "env": {}, "client_package": "/unused", "helper": "/unused",
            "flake": "unused",
        });
        value
            .as_object_mut()
            .unwrap()
            .extend(extra.as_object().unwrap().clone());
        value
    }
    fn manifest(goblins: Value) -> Value {
        json!({
            "api": 2, "helper_api": 6, "goblins": goblins,
            "scopes": {
                "work": {"persistent": false, "pid": true},
                "other": {"persistent": false, "pid": true},
            },
        })
    }
    fn parse(value: Value) -> Manifest {
        serde_json::from_value(value).unwrap()
    }
    /// Validate a manifest written to a file, as the daemon reads it.
    fn validated(value: &Value) -> Result<Manifest> {
        let directory = goblins_unix::temp_directory().unwrap();
        let path = directory.join("manifest.json");
        std::fs::write(&path, value.to_string()).unwrap();
        let result = crate::config::validate(&path);
        std::fs::remove_dir_all(directory).unwrap();
        result
    }

    #[test]
    fn run_targets_come_from_the_top_level_configuration() {
        let m = parse(manifest(json!({
            "coordinator": goblin(json!({
                "allowed_run_targets": ["reviewer"], "scope": "work",
                "scopes": {"work": goblin(json!({}))},
            })),
            "reviewer": goblin(json!({})),
        })));
        assert_eq!(run_targets(&m, "coordinator"), ["reviewer"]);
        assert!(run_targets(&m, "reviewer").is_empty());
        assert!(run_targets(&m, "unknown").is_empty());
    }

    #[test]
    fn run_targets_resolve_for_the_owner_scope() {
        let goblins = json!({
            // A different default scope is ignored, never followed.
            "member": goblin(json!({
                "scope": "other", "allowed_scopes": ["work"],
                "scopes": {
                    "other": goblin(json!({"description": "member in other"})),
                    "work": goblin(json!({"description": "member in work"})),
                },
            })),
            "elsewhere": goblin(json!({
                "scope": "other", "scopes": {"other": goblin(json!({}))},
            })),
            "unscoped": goblin(json!({})),
            "required": goblin(json!({
                "scope": "work", "scopes": {"work": goblin(json!({}))},
            })),
            "joins": goblin(json!({
                "allowed_scopes": ["work"], "scopes": {"work": goblin(json!({}))},
            })),
        });
        let m = parse(manifest(goblins));
        let member = select_inherited(&m, "member", Some("work")).unwrap();
        assert_eq!(member.description, "member in work");
        assert_eq!(member.selected_scope.as_ref().unwrap().name, "work");
        assert!(select_inherited(&m, "required", Some("work")).is_ok());
        assert!(select_inherited(&m, "joins", Some("work")).is_ok());
        assert!(
            select_inherited(&m, "joins", None)
                .unwrap()
                .selected_scope
                .is_none()
        );
        assert!(select_inherited(&m, "unscoped", None).is_ok());
        for (name, scope, error) in [
            (
                "elsewhere",
                Some("work"),
                "'elsewhere' does not allow scope 'work'",
            ),
            (
                "unscoped",
                Some("work"),
                "'unscoped' does not allow scope 'work'",
            ),
            (
                "required",
                None,
                "'required' requires scope 'work'; its owner has no scope",
            ),
            ("absent", None, "unknown goblin 'absent'"),
        ] {
            assert_eq!(
                select_inherited(&m, name, scope).err().unwrap().to_string(),
                error
            );
        }
    }

    #[test]
    fn a_manifest_lease_roots_its_store_output_until_the_last_holder_drops_it() {
        let _fork = goblins_unix::forking();
        // Needs the host's Nix daemon, which a Nix build sandbox lacks.
        if std::process::Command::new("nix-store")
            .arg("--version")
            .output()
            .is_err()
        {
            return;
        }
        let directory = goblins_unix::temp_directory().unwrap();
        let file = directory.join("goblins-config.json");
        std::fs::write(
            &file,
            manifest(json!({"shell": goblin(json!({}))})).to_string(),
        )
        .unwrap();
        let added = std::process::Command::new("nix-store")
            .arg("--add")
            .arg(&file)
            .output()
            .unwrap();
        assert!(added.status.success());
        let stored = PathBuf::from(String::from_utf8(added.stdout).unwrap().trim());
        let roots = directory.join("manifests");
        let cancel = crate::process::Cancellation::default();
        let lease = std::sync::Arc::new(
            crate::config::lease_manifest(&stored, &roots, 7, &cancel).unwrap(),
        );
        let link = roots.join(format!(
            "7-{}",
            stored.file_name().unwrap().to_string_lossy()
        ));
        assert_eq!(std::fs::read_link(&link).ok(), Some(stored.clone()));
        // A launch holding a clone keeps it after the daemon replaced it.
        let launch = lease.clone();
        drop(lease);
        assert!(link.is_symlink());
        drop(launch);
        assert!(!link.is_symlink());
        // An invalid manifest is never leased.
        std::fs::write(&file, "invalid").unwrap();
        assert!(crate::config::lease_manifest(&file, &roots, 8, &cancel).is_err());
        std::fs::remove_dir_all(directory).unwrap();
    }
    #[test]
    fn validation_covers_versions_bounds_and_the_editor() {
        let good = manifest(json!({"shell": goblin(json!({}))}));
        assert!(validated(&good).is_ok());
        let mut old = good.clone();
        old["api"] = json!(1);
        assert!(
            validated(&old)
                .err()
                .unwrap()
                .to_string()
                .contains("incompatible")
        );
        let mut editor = good.clone();
        editor["editor"] = json!({"kind": "vim", "command": "vim"});
        assert!(
            validated(&editor)
                .err()
                .unwrap()
                .to_string()
                .contains("editor")
        );
        let many: Vec<_> = (0..65).map(|i| format!("g{i}")).collect();
        for targets in [json!(many), json!(["x".repeat(65)]), json!(["../bad"])] {
            let bad = manifest(json!({"shell": goblin(json!({"allowed_run_targets": targets}))}));
            let error = validated(&bad).err().unwrap().to_string();
            assert!(error.contains("invalid allowed_run_targets"), "{error}");
        }
        let fits = manifest(json!({"shell": goblin(json!({"allowed_run_targets": many[..64]}))}));
        assert!(validated(&fits).is_ok());
        assert!(crate::config::validate(Path::new("/nonexistent/manifest.json")).is_err());
    }
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
                    "{{x}}",
                    // An argument the agent builders escaped, such as JSON.
                    r#"{{"mcpServers":{{}}}}"#
                ]),
                &variables
            ),
            Ok(template(&[
                "-p",
                "--resume",
                "c8193c93",
                "--name=Plan it; $(false)",
                "{x}",
                r#"{"mcpServers":{}}"#
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
    #[test]
    fn attach_handlers_fill_variables_and_the_client_pid() {
        let variables = BTreeMap::from([("session_id".to_string(), "c819".to_string())]);
        let template = |parts: &[&str]| parts.iter().map(|p| p.to_string()).collect::<Vec<_>>();
        let command = template(&["resume", "{session_id}", "--pid={attach.pid}", "{{x}}"]);
        assert_eq!(
            handler_arguments(&command, &variables, Some(42)),
            Ok(template(&["resume", "c819", "--pid=42", "{x}"]))
        );
        assert!(handler_arguments(&command, &variables, None).is_err());
        assert!(resume_arguments(&template(&["{attach.pid}"]), &variables).is_err());
        let on_attach = OnAttach {
            command: template(&["{session_id}", "{attach.pid}", "{session_id}"]),
            timeout: 30,
            wait_for: template(&["ready", "session_id"]),
        };
        assert_eq!(on_attach.needs(), ["ready", "session_id"]);
        assert!(Handlers::check(&on_attach, None).is_ok());
        for (command, timeout, wait_for) in [
            (template(&[]), 30, vec![]),
            (template(&["{attach.other}"]), 30, vec![]),
            (template(&["{Bad}"]), 30, vec![]),
            (template(&["x"]), 0, vec![]),
            (template(&["x"]), 30, template(&["attach.pid"])),
        ] {
            let on_attach = OnAttach {
                command,
                timeout,
                wait_for,
            };
            assert!(Handlers::check(&on_attach, None).is_err());
        }
        let on_attach = OnAttach {
            command: template(&["x"]),
            timeout: 30,
            wait_for: vec![],
        };
        assert!(Handlers::check(&on_attach, Some(&template(&["{attach.x}"]))).is_err());
    }
}
