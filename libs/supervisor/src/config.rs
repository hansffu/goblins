use crate::Result;
use goblins_runtime::config::Launch as Definition;
use goblins_runtime::config::{Configuration, Editor, Handlers, Manifest, ManifestLease, Scope};
use serde::Deserialize;
use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
};
/// Validate and root a manifest. The construction guard precedes Nix, and
/// outlives command cancellation/process-group cleanup on every error.
pub fn lease_manifest(
    path: &Path,
    directory: &Path,
    sequence: u64,
    cancel: &crate::process::Cancellation,
) -> Result<ManifestLease> {
    validate(path)?;
    let Some(output) = crate::session::containing_store_output(path) else {
        return Ok(ManifestLease::adopt(path.to_path_buf(), None));
    };
    goblins_unix::private_directory(directory)?;
    let name = output.file_name().unwrap().to_string_lossy();
    let link = directory.join(format!("{sequence}-{name}"));
    construct_root(path, link, |link| {
        crate::process::command(
            std::process::Command::new("nix-store")
                .arg("--realise")
                .arg(&output)
                .arg("--add-root")
                .arg(link)
                .arg("--indirect"),
            directory,
            cancel,
        )
        .map(|_| ())
    })
}
fn construct_root(
    path: &Path,
    link: PathBuf,
    create: impl FnOnce(&Path) -> Result<()>,
) -> Result<ManifestLease> {
    let mut lease = ManifestLease::begin_root(path.to_path_buf(), link.clone())?;
    create(&link)?;
    lease.finish_root()?;
    Ok(lease)
}
pub use goblins_runtime::config::{editor, read, run_targets, validate};
pub fn select(manifest: Manifest, name: &str, scope: Option<&str>) -> Result<Configuration> {
    goblins_runtime::config::select(manifest, name, scope, |name| std::env::var(name).ok())
}
pub fn select_inherited(
    manifest: &Manifest,
    name: &str,
    scope: Option<&str>,
) -> Result<Configuration> {
    goblins_runtime::config::select_inherited(manifest, name, scope, |name| {
        std::env::var(name).ok()
    })
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
pub fn launch(config: &Configuration) -> Result<Launch> {
    let spec = goblins_runtime::config::build_spec(config)?;
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
            initial_closure: String::from_utf8(goblins_unix::read_regular(
                Path::new(string("closure_paths_file")?),
                8 * 1024 * 1024,
                "configuration",
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
}

#[cfg(test)]
mod construction_tests {
    use super::*;
    use std::{
        fs,
        sync::mpsc,
        time::{Duration, Instant},
    };
    #[test]
    fn construction_is_exclusive_while_the_root_is_absent_and_preserves_collisions() {
        let _no_forks = goblins_unix::no_forks();
        let dir = goblins_unix::temp_directory().unwrap();
        let root = dir.join("root");
        let (entered, wait_entered) = mpsc::channel();
        let (release, released) = mpsc::channel();
        let link = root.clone();
        let first = std::thread::spawn(move || {
            construct_root(Path::new("/manifest"), link, |path| {
                entered.send(()).unwrap();
                released.recv().unwrap();
                std::os::unix::fs::symlink("/owned", path)?;
                Ok(())
            })
        });
        wait_entered.recv_timeout(Duration::from_secs(5)).unwrap();
        assert!(!root.is_symlink());
        assert!(
            construct_root(Path::new("/other"), root.clone(), |_| panic!(
                "second constructor entered"
            ))
            .is_err()
        );
        release.send(()).unwrap();
        let lease = first.join().unwrap().unwrap();
        assert!(
            construct_root(Path::new("/other"), root.clone(), |_| panic!(
                "live collision entered"
            ))
            .is_err()
        );
        assert_eq!(fs::read_link(&root).unwrap(), Path::new("/owned"));
        drop(lease);
        assert!(!root.is_symlink());
        for dangling in [false, true] {
            if dangling {
                std::os::unix::fs::symlink("/missing", &root).unwrap();
            } else {
                fs::write(&root, "preexisting").unwrap();
            }
            assert!(
                construct_root(Path::new("/other"), root.clone(), |_| panic!(
                    "preexisting collision entered"
                ))
                .is_err()
            );
            if dangling {
                assert_eq!(fs::read_link(&root).unwrap(), Path::new("/missing"));
            } else {
                assert_eq!(fs::read_to_string(&root).unwrap(), "preexisting");
            }
            fs::remove_file(&root).unwrap();
        }
        // A command reporting success without constructing its link is invalid.
        assert!(construct_root(Path::new("/manifest"), root, |_| Ok(())).is_err());
        fs::remove_dir_all(dir).unwrap();
    }
    #[test]
    fn failed_and_cancelled_creators_are_reaped_before_the_attempt_root_is_removed() {
        let _fork = goblins_unix::forking();
        for cancelled in [false, true] {
            let dir = goblins_unix::temp_directory().unwrap();
            let link = dir.join("root");
            let pid_file = dir.join("pid");
            let cancel = crate::process::Cancellation::default();
            let cancellation = if cancelled {
                let ready = pid_file.clone();
                let cancel = cancel.clone();
                Some(std::thread::spawn(move || {
                    let deadline = Instant::now() + Duration::from_secs(5);
                    while !ready.exists() {
                        assert!(Instant::now() < deadline);
                        std::thread::sleep(Duration::from_millis(1));
                    }
                    cancel.cancel();
                }))
            } else {
                None
            };
            let error = construct_root(Path::new("/manifest"), link.clone(), |path| {
                let result = crate::process::command(
                    std::process::Command::new("sh").arg("-c")
                        .arg(if cancelled {
                            "ln -s /owned \"$1\"; echo $$ > \"$2\"; sleep 30"
                        } else {
                            "ln -s /owned \"$1\"; echo $$ > \"$2\"; echo original-failure >&2; exit 7"
                        }).arg("creator").arg(path).arg(&pid_file), &dir, &cancel,
                );
                assert!(path.is_symlink(), "guard removed root before command completed");
                let pid: i32 = fs::read_to_string(&pid_file).unwrap().trim().parse().unwrap();
                assert_eq!(unsafe { libc::waitpid(pid, std::ptr::null_mut(), libc::WNOHANG) }, -1);
                assert_eq!(std::io::Error::last_os_error().raw_os_error(), Some(libc::ECHILD));
                assert!(result.is_err());
                result.map(|_| ())
            }).unwrap_err();
            if let Some(thread) = cancellation {
                thread.join().unwrap();
            }
            assert!(
                error.to_string().contains(if cancelled {
                    "cancelled"
                } else {
                    "original-failure"
                }),
                "{error}"
            );
            assert!(!link.is_symlink(), "attempt root leaked after creator reap");
            fs::remove_dir_all(dir).unwrap();
        }
    }
}
