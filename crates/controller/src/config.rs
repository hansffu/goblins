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
    pub network: bool,
    pub build_spec: PathBuf,
    pub args: Vec<String>,
    pub env: BTreeMap<String, String>,
    pub client_package: PathBuf,
    pub helper: PathBuf,
    pub flake: String,
    #[serde(default)]
    pub sandbox_etc: BTreeMap<String, PathBuf>,
    #[serde(default)]
    pub integration: Option<String>,
}
#[derive(Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Manifest {
    pub api: u32,
    pub helper_api: u32,
    pub goblins: BTreeMap<String, Configuration>,
}
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
impl Manifest {
    pub fn read(path: &Path) -> Result<Self> {
        serde_json::from_slice(&read_regular(path, MAX_MANIFEST_BYTES)?)
            .map_err(|e| format!("invalid configuration {}: {e}", path.display()).into())
    }
    pub fn select(mut self, name: &str) -> Result<Configuration> {
        if self.api != 1 || self.helper_api != 2 {
            return Err("incompatible manifest/helper API".into());
        }
        self.goblins.remove(name).ok_or_else(|| {
            format!(
                "unknown goblin; available: {}",
                self.goblins.keys().cloned().collect::<Vec<_>>().join(", ")
            )
            .into()
        })
    }
}
/// Trusted launch configuration, never accepted over the sandbox socket.
#[derive(Clone, Deserialize)]
pub struct Launch {
    #[serde(default)]
    pub pasta: Option<PathBuf>,
    #[serde(skip)]
    pub cwd: Option<PathBuf>,
    pub shell: PathBuf,
    #[serde(default = "default_args")]
    pub shell_args: Vec<String>,
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
    pub integration: Option<String>,
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
            integration: self.integration.clone(),
            binds: serde_json::from_value(serde_json::json!({
                "rw_dirs": spec["rw_dirs"], "rw_files": spec["rw_files"],
                "ro_dirs": spec["ro_dirs"], "ro_files": spec["ro_files"],
            }))?,
        })
    }
}
