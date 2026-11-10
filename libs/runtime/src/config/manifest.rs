//! Bounded manifest data parsing and selection; no sandbox or Nix work.
use super::{Configuration, Editor, Handlers, MAX_RUN_TARGETS, Manifest, Scope};
use crate::Result;
use std::{collections::BTreeMap, path::Path};
const MAX_MANIFEST_BYTES: u64 = 1024 * 1024;
pub fn read(path: &Path) -> Result<Manifest> {
    serde_json::from_slice(&goblins_unix::read_regular(
        path,
        MAX_MANIFEST_BYTES,
        "configuration",
    )?)
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
pub fn select(
    mut manifest: Manifest,
    name: &str,
    scope: Option<&str>,
    env: impl Fn(&str) -> Option<String>,
) -> Result<Configuration> {
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
    resolve(&manifest, name, config, scope, env)
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
        check_launch_options(name, config)?;
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
/// The bounds and grammars mkGoblin applies to `launchOptions`. Scope
/// variants carry none, and no variant's `args` may end the options before
/// the chosen arguments that follow them.
fn check_launch_options(name: &str, config: &Configuration) -> Result<()> {
    let text = |t: &Option<String>| {
        t.as_ref()
            .is_none_or(|t| t.len() <= 200 && !t.contains(['\n', '\r']))
    };
    let valid = config.launch_options.len() <= goblins_protocol::MAX_LAUNCH_OPTIONS
        && config.launch_options.iter().all(|(option, o)| {
            goblins_protocol::launch_option_name(option)
                && text(&o.description)
                && (1..=64).contains(&o.choices.len())
                && o.choices.iter().all(|(choice, c)| {
                    goblins_protocol::launch_choice(choice)
                        && text(&c.description)
                        && (1..=16).contains(&c.args.len())
                        && c.args.iter().all(|a| a.len() <= 256 && a != "--")
                })
        })
        && config.scopes.values().all(|v| v.launch_options.is_empty())
        && (config.launch_options.is_empty()
            || std::iter::once(config)
                .chain(config.scopes.values())
                .all(|v| !v.args.iter().any(|a| a == "--")));
    if valid {
        Ok(())
    } else {
        Err(format!("invalid launch_options for '{name}'").into())
    }
}
/// mkGoblins' `worktrees.directory`, expanded like file grants from the
/// daemon's environment. None when worktrees are off.
pub fn worktrees_directory(
    manifest: &Manifest,
    env: impl Fn(&str) -> Option<String>,
) -> Result<Option<std::path::PathBuf>> {
    let Some(raw) = &manifest.worktrees.directory else {
        return Ok(None);
    };
    let path = crate::binds::expand(raw, env)?;
    if path.parent().is_none() {
        return Err("worktrees.directory cannot be the root".into());
    }
    Ok(Some(path))
}
/// The configurations a top-level configuration may run as raid
/// members; unknown names have none. Scope variants never carry the list.
pub fn run_targets(manifest: &Manifest, name: &str) -> Vec<String> {
    match manifest.goblins.get(name) {
        Some(config) => config.allowed_run_targets.clone(),
        None => vec![],
    }
}
/// Resolve a run target for the scope its owner runs in. It never selects
/// a scope: a different default scope is ignored.
pub fn select_inherited(
    manifest: &Manifest,
    name: &str,
    scope: Option<&str>,
    env: impl Fn(&str) -> Option<String>,
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
        _ => resolve(&manifest, name, config, scope, env),
    }
}
fn resolve(
    manifest: &Manifest,
    name: &str,
    mut config: Configuration,
    scope: Option<&str>,
    env: impl Fn(&str) -> Option<String>,
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
        let path = crate::binds::expand(raw, &env)?;
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
pub fn build_spec(config: &Configuration) -> Result<serde_json::Value> {
    Ok(serde_json::from_slice(&goblins_unix::read_regular(
        &config.build_spec,
        MAX_MANIFEST_BYTES,
        "configuration",
    )?)?)
}
/// mkGoblin's allowNix, from the build spec the launch also reads.
pub fn allows_nix(config: &Configuration) -> Result<bool> {
    Ok(build_spec(config)?["allow_nix"] == true)
}
#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{OnAttach, handler_arguments, resume_arguments};
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
    fn reading_is_distinct_from_validation_and_selection_preserves_error_order() {
        let dir = goblins_unix::temp_directory().unwrap();
        let path = dir.join("manifest.json");
        let mut old = manifest(json!({"shell": goblin(json!({}))}));
        old["api"] = json!(1);
        old["editor"] = json!({"kind":"invalid", "command":""});
        std::fs::write(&path, old.to_string()).unwrap();
        let data = read(&path).unwrap();
        assert_eq!(data.api, 1);
        assert_eq!(
            validate(&path).err().unwrap().to_string(),
            "incompatible manifest/helper API"
        );
        assert_eq!(
            select(data, "absent", None, |_| None)
                .err()
                .unwrap()
                .to_string(),
            "incompatible manifest/helper API"
        );
        old["api"] = json!(2);
        std::fs::write(&path, old.to_string()).unwrap();
        let data = read(&path).unwrap();
        assert_eq!(
            select(data.clone(), "absent", None, |_| None)
                .err()
                .unwrap()
                .to_string(),
            "unknown goblin; available: shell"
        );
        // Selected launch validation has never checked the global editor.
        assert!(select(data, "shell", None, |_| None).is_ok());
        assert_eq!(
            validate(&path).err().unwrap().to_string(),
            "invalid editor configuration"
        );
        std::fs::remove_dir_all(dir).unwrap();
    }
    #[test]
    fn scope_mounts_expand_only_the_injected_environment_in_the_existing_order() {
        let mut value = manifest(json!({"shell": goblin(json!({
            "scope":"work",
            "scopes":{"work":goblin(json!({"scope_storage":{"cache":"${CACHE}/data"}}))}
        }))}));
        value["scopes"]["work"]["storage"] = json!(["cache"]);
        let selected = select(parse(value.clone()), "shell", None, |name| {
            assert_eq!(name, "CACHE");
            Some("/home/injected".into())
        })
        .unwrap();
        assert_eq!(
            selected.selected_scope.unwrap().mounts["cache"],
            Path::new("/home/injected/data")
        );
        assert_eq!(
            select(parse(value.clone()), "shell", None, |_| None)
                .err()
                .unwrap()
                .to_string(),
            "unset host bind variable: CACHE"
        );
        value["scopes"]["work"]["storage"] = json!([]);
        // Expansion precedes storage membership, which precedes protected paths.
        assert_eq!(
            select(parse(value.clone()), "shell", None, |_| None)
                .err()
                .unwrap()
                .to_string(),
            "unset host bind variable: CACHE"
        );
        assert_eq!(
            select(parse(value), "shell", None, |_| Some("/nix".into()))
                .err()
                .unwrap()
                .to_string(),
            "invalid scope storage mount 'cache'"
        );
    }
    #[test]
    fn manifest_and_build_spec_reads_keep_the_one_mib_limit_and_follow_regular_symlinks() {
        let dir = goblins_unix::temp_directory().unwrap();
        let path = dir.join("manifest.json");
        let value = manifest(json!({"shell": goblin(json!({}))}));
        let mut bytes = value.to_string().into_bytes();
        bytes.resize(MAX_MANIFEST_BYTES as usize, b' ');
        std::fs::write(&path, &bytes).unwrap();
        assert!(read(&path).is_ok());
        let alias = dir.join("alias.json");
        std::os::unix::fs::symlink(&path, &alias).unwrap();
        assert!(read(&alias).is_ok());
        bytes.push(b' ');
        std::fs::write(&path, &bytes).unwrap();
        assert_eq!(
            read(&path).err().unwrap().to_string(),
            "configuration exceeds 1 MiB"
        );
        let mut config = parse(value).goblins.remove("shell").unwrap();
        config.build_spec = path.clone();
        assert_eq!(
            allows_nix(&config).err().unwrap().to_string(),
            "configuration exceeds 1 MiB"
        );
        std::fs::write(&path, r#"{"allow_nix":true,"platform":"unrelated"}"#).unwrap();
        assert!(allows_nix(&config).unwrap()); // This read does not validate launch policy.
        std::fs::write(&path, "invalid").unwrap();
        assert!(
            read(&path)
                .err()
                .unwrap()
                .to_string()
                .starts_with("invalid configuration ")
        );
        assert!(
            !allows_nix(&config)
                .err()
                .unwrap()
                .to_string()
                .starts_with("invalid configuration ")
        );
        std::fs::remove_dir_all(dir).unwrap();
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
        let member = select_inherited(&m, "member", Some("work"), |_| None).unwrap();
        assert_eq!(member.description, "member in work");
        assert_eq!(member.selected_scope.as_ref().unwrap().name, "work");
        assert!(select_inherited(&m, "required", Some("work"), |_| None).is_ok());
        assert!(select_inherited(&m, "joins", Some("work"), |_| None).is_ok());
        assert!(
            select_inherited(&m, "joins", None, |_| None)
                .unwrap()
                .selected_scope
                .is_none()
        );
        assert!(select_inherited(&m, "unscoped", None, |_| None).is_ok());
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
                select_inherited(&m, name, scope, |_| None)
                    .err()
                    .unwrap()
                    .to_string(),
                error
            );
        }
    }

    #[test]
    fn launch_options_are_parsed_and_bounded_like_mkgoblin() {
        let options =
            |value: Value| manifest(json!({"shell": goblin(json!({"launch_options": value}))}));
        let model = json!({"model": {"description": "Model", "choices": {
            "gpt-5.5": {"description": null, "args": ["-c", "model=\"gpt-5.5\""]}}}});
        let parsed = validated(&options(model.clone())).unwrap();
        let option = &parsed.goblins["shell"].launch_options["model"];
        assert_eq!(option.description.as_deref(), Some("Model"));
        assert_eq!(option.choices["gpt-5.5"].args, ["-c", "model=\"gpt-5.5\""]);
        // Absent means none, so manifests before launch options still parse.
        let none = validated(&manifest(json!({"shell": goblin(json!({}))}))).unwrap();
        assert!(none.goblins["shell"].launch_options.is_empty());
        let choice = |c: Value| json!({"mode": {"choices": c}});
        let many = |n: usize, item: Value| -> Value {
            (0..n)
                .map(|i| (format!("x{i}"), item.clone()))
                .collect::<serde_json::Map<_, _>>()
                .into()
        };
        for bad in [
            choice(json!({})),
            choice(json!({"fast": {"args": []}})),
            choice(json!({"fast": {"args": ["--"]}})),
            choice(json!({"fast": {"args": ["x".repeat(257)]}})),
            choice(json!({"fast": {"args": vec!["x"; 17]}})),
            choice(json!({"a=b": {"args": ["x"]}})),
            choice(json!({".a": {"args": ["x"]}})),
            choice(json!({"x".repeat(65): {"args": ["x"]}})),
            choice(json!({"fast": {"args": ["x"], "description": "a\nb"}})),
            choice(json!({"fast": {"args": ["x"], "description": "x".repeat(201)}})),
            // Bytes, as Nix counts them.
            choice(json!({"fast": {"args": ["x"], "description": "æ".repeat(101)}})),
            choice(many(65, json!({"args": ["x"]}))),
            json!({"mo.de": {"choices": {"fast": {"args": ["x"]}}}}),
            json!({"x".repeat(33): {"choices": {"fast": {"args": ["x"]}}}}),
            many(17, json!({"choices": {"fast": {"args": ["x"]}}})),
        ] {
            let error = validated(&options(bad.clone())).err().unwrap().to_string();
            assert_eq!(error, "invalid launch_options for 'shell'", "{bad}");
        }
        // Unknown fields, such as a choice environment, are refused.
        assert!(
            validated(&options(choice(
                json!({"fast": {"args": ["x"], "env": {}}})
            )))
            .is_err()
        );
        // Variants carry none, and no variant's args end the options early.
        let scoped = |variant: Value, args: Value| {
            manifest(json!({"shell": goblin(json!({
                "launch_options": model, "args": args, "allowed_scopes": ["work"],
                "scopes": {"work": goblin(variant)},
            }))}))
        };
        assert!(validated(&scoped(json!({}), json!(["--base"]))).is_ok());
        for bad in [
            scoped(json!({"launch_options": model}), json!([])),
            scoped(json!({"args": ["--"]}), json!([])),
            scoped(json!({}), json!(["--"])),
        ] {
            assert!(validated(&bad).is_err(), "{bad}");
        }
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
