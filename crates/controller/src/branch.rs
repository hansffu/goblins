//! The configurations a running branch may launch, fixed at root launch.
//! Children never reopen the manifest; their store paths are GC-rooted in the
//! root session's directory, so rebuilds and garbage collection cannot change
//! what the branch launches. Nothing else is opened until a child starts.
use crate::{config::Configuration, session::Session};
use std::{
    collections::{BTreeMap, BTreeSet, VecDeque},
    path::{Path, PathBuf},
};

#[derive(Default)]
pub(crate) struct Branch {
    entries: BTreeMap<String, Entry>,
}
pub(crate) struct Entry {
    pub allowed_children: Vec<String>,
    /// The configuration resolved for the branch's scope, or why it cannot
    /// run here. None for the root entry until another configuration lists it.
    configuration: Option<std::result::Result<Configuration, String>>,
}
impl Branch {
    /// Walk `allowed_children` breadth-first from the root. A broken reachable
    /// configuration is recorded, not returned: the root must still start.
    pub(crate) fn snapshot(
        root: &str,
        children: impl Fn(&str) -> Vec<String>,
        resolve: impl Fn(&str) -> crate::Result<Configuration>,
    ) -> Self {
        let mut entries = BTreeMap::new();
        let mut listed = BTreeSet::new();
        let mut queue = VecDeque::from([root.to_string()]);
        while let Some(name) = queue.pop_front() {
            if entries.contains_key(&name) {
                continue;
            }
            let allowed_children = children(&name);
            for child in &allowed_children {
                if *child != name {
                    listed.insert(child.clone());
                }
                queue.push_back(child.clone());
            }
            entries.insert(
                name,
                Entry {
                    allowed_children,
                    configuration: None,
                },
            );
        }
        // A configuration listing itself uses the same-configuration path,
        // which never needs its own entry.
        for name in listed {
            entries.get_mut(&name).unwrap().configuration =
                Some(resolve(&name).map_err(|e| e.to_string()));
        }
        Self { entries }
    }
    /// GC-root every resolved configuration's top-level store paths in the
    /// root session's directory. A failure makes only that entry unusable.
    pub(crate) fn prepare(&mut self, session: &Session) {
        for entry in self.entries.values_mut() {
            if let Some(Ok(config)) = &entry.configuration
                && let Err(error) = roots(config)
                    .into_iter()
                    .try_for_each(|path| session.root(&path).map(drop))
            {
                entry.configuration = Some(Err(format!("cannot retain its store paths: {error}")));
            }
        }
    }
    pub(crate) fn allowed_children(&self, configuration: &str) -> &[String] {
        self.entries
            .get(configuration)
            .map_or(&[], |e| &e.allowed_children)
    }
    /// Whether a session of `parent` may launch a `child`, with the reason
    /// shown to the caller otherwise.
    pub(crate) fn permit(&self, parent: &str, child: &str) -> std::result::Result<(), String> {
        let allowed = self.allowed_children(parent);
        if allowed.is_empty() {
            return Err(format!("configuration '{parent}' permits no children"));
        }
        if !allowed.iter().any(|c| c == child) {
            return Err(format!(
                "configuration '{child}' is not an allowed child of '{parent}'; allowed: {}",
                allowed.join(", ")
            ));
        }
        if child == parent {
            return Ok(());
        }
        self.configuration(child)
            .map(drop)
            .map_err(|e| format!("configuration '{child}' cannot be launched here: {e}"))
    }
    /// The snapshot of a different child configuration.
    pub(crate) fn configuration(&self, name: &str) -> std::result::Result<&Configuration, String> {
        match self
            .entries
            .get(name)
            .and_then(|e| e.configuration.as_ref())
        {
            Some(Ok(config)) => Ok(config),
            Some(Err(error)) => Err(error.clone()),
            None => Err("not in this branch's configuration snapshot".into()),
        }
    }
}
/// Top-level store paths a later launch reads: the live build spec (whose
/// references retain the closure file and closure), helper, client, /etc
/// sources and the Docker packages.
fn roots(config: &Configuration) -> BTreeSet<PathBuf> {
    let package = |p: &Path| p.parent().and_then(Path::parent).map(Path::to_path_buf);
    [config.build_spec.clone(), config.client_package.clone()]
        .into_iter()
        .chain(package(&config.helper))
        .chain(config.sandbox_etc.values().cloned())
        .chain(config.dev_shell_sandbox_etc.values().cloned())
        .chain(
            config
                .docker
                .iter()
                .flat_map(|d| package(&d.daemon).into_iter().chain(d.client.clone())),
        )
        .map(|p| crate::session::containing_store_output(&p).unwrap_or(p))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Manifest;
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
    fn manifest(goblins: Value) -> Manifest {
        serde_json::from_value(json!({
            "api": 2, "helper_api": 5, "goblins": goblins,
            "scopes": {
                "work": {"persistent": false, "pid": true},
                "other": {"persistent": false, "pid": true},
            },
        }))
        .unwrap()
    }
    fn names(branch: &Branch) -> Vec<&str> {
        branch.entries.keys().map(String::as_str).collect()
    }

    #[test]
    fn missing_or_null_list_permits_only_the_configuration_itself() {
        // Manifests from older builds carry no allowed_children.
        let (_, branch) = manifest(json!({"shell": goblin(json!({})), "other": goblin(json!({}))}))
            .branch("shell", None)
            .unwrap();
        assert_eq!(names(&branch), ["shell"]);
        assert_eq!(branch.allowed_children("shell"), ["shell"]);
        assert!(branch.permit("shell", "shell").is_ok());
        assert_eq!(
            branch.permit("shell", "other").unwrap_err(),
            "configuration 'other' is not an allowed child of 'shell'; allowed: shell"
        );
        // The default self-child needs no snapshot entry and roots nothing.
        assert!(branch.entries["shell"].configuration.is_none());
    }

    #[test]
    fn explicit_lists_cycles_and_empty_lists() {
        let (_, branch) = manifest(json!({
            "coordinator": goblin(json!({"allowed_children": ["coordinator", "reviewer", "shell"]})),
            "reviewer": goblin(json!({"allowed_children": []})),
            "shell": goblin(json!({"allowed_children": ["coordinator"]})),
            "unreachable": goblin(json!({"allowed_children": ["unreachable"]})),
        }))
        .branch("coordinator", None)
        .unwrap();
        assert_eq!(names(&branch), ["coordinator", "reviewer", "shell"]);
        // Listed by shell, so the root needs its own entry as well.
        assert!(branch.configuration("coordinator").is_ok());
        assert!(branch.permit("coordinator", "reviewer").is_ok());
        assert!(branch.permit("shell", "coordinator").is_ok());
        assert_eq!(
            branch.permit("reviewer", "reviewer").unwrap_err(),
            "configuration 'reviewer' permits no children"
        );
        assert_eq!(
            branch.permit("shell", "reviewer").unwrap_err(),
            "configuration 'reviewer' is not an allowed child of 'shell'; allowed: coordinator"
        );
        assert!(branch.permit("coordinator", "unreachable").is_err());
    }

    #[test]
    fn entries_resolve_for_the_root_scope_and_record_errors() {
        let goblins = json!({
            "root": goblin(json!({
                "scope": "work", "scopes": {"work": goblin(json!({"description": "root in work"}))},
                "allowed_children": ["root", "member", "elsewhere", "unscoped", "required", "joins"],
            })),
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
        let (root, branch) = manifest(goblins.clone()).branch("root", None).unwrap();
        assert_eq!(root.description, "root in work");
        let member = branch.configuration("member").unwrap();
        assert_eq!(member.description, "member in work");
        assert_eq!(member.selected_scope.as_ref().unwrap().name, "work");
        assert!(branch.configuration("required").is_ok());
        assert!(branch.configuration("joins").is_ok());
        assert_eq!(
            branch.permit("root", "elsewhere").unwrap_err(),
            "configuration 'elsewhere' cannot be launched here: 'elsewhere' does not allow scope 'work'"
        );
        assert_eq!(
            branch.configuration("unscoped").err().unwrap(),
            "'unscoped' does not allow scope 'work'"
        );
        // The same configurations under a scopeless root.
        let mut goblins = goblins;
        goblins["root"]["scope"] = Value::Null;
        goblins["root"]["scopes"] = json!({});
        let (_, branch) = manifest(goblins).branch("root", None).unwrap();
        assert!(branch.configuration("unscoped").is_ok());
        assert!(
            branch
                .configuration("joins")
                .unwrap()
                .selected_scope
                .is_none()
        );
        assert_eq!(
            branch.configuration("required").err().unwrap(),
            "'required' requires scope 'work'; its parent has no scope"
        );
    }

    #[test]
    fn list_bounds_are_enforced_when_reading_the_manifest() {
        let many: Vec<_> = (0..65).map(|i| format!("g{i}")).collect();
        for children in [json!(many), json!(["x".repeat(65)]), json!(["../bad"])] {
            let error = manifest(json!({"shell": goblin(json!({"allowed_children": children}))}))
                .branch("shell", None)
                .err()
                .unwrap();
            assert!(
                error.to_string().contains("invalid allowed_children"),
                "{error}"
            );
        }
        assert!(
            manifest(json!({"shell": goblin(json!({"allowed_children": many[..64]}))}))
                .select("shell", None)
                .is_ok()
        );
    }
}
