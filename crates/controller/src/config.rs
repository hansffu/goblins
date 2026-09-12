use crate::Result;
use serde::Deserialize;
use std::{
    collections::BTreeMap,
    fs::{self, OpenOptions},
    io::Read,
    os::unix::fs::OpenOptionsExt,
    path::{Path, PathBuf},
};

#[derive(Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Configuration {
    pub build_spec: PathBuf,
    pub args: Vec<String>,
    pub env: BTreeMap<String, String>,
    pub client_package: PathBuf,
    pub helper: PathBuf,
    pub flake: String,
}
#[derive(Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Manifest {
    pub goblins: BTreeMap<String, Configuration>,
}
// Host manifests contain only launch metadata, not package contents. Bound the
// read and reject special files so a mistaken FIFO/device cannot stall startup.
const MAX_MANIFEST_BYTES: u64 = 1024 * 1024;
impl Manifest {
    pub fn read(path: &Path) -> Result<Self> {
        let file = OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NONBLOCK)
            .open(path)
            .map_err(|e| format!("cannot open configuration {}: {e}", path.display()))?;
        if !file.metadata()?.is_file() {
            return Err("configuration must be a regular file".into());
        }
        let mut bytes = Vec::new();
        file.take(MAX_MANIFEST_BYTES + 1).read_to_end(&mut bytes)?;
        if bytes.len() as u64 > MAX_MANIFEST_BYTES {
            return Err("configuration exceeds 1 MiB".into());
        }
        serde_json::from_slice(&bytes)
            .map_err(|e| format!("invalid configuration {}: {e}", path.display()).into())
    }
    pub fn select(mut self, name: &str) -> Result<Configuration> {
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
}
fn default_args() -> Vec<String> {
    vec!["--noprofile".into(), "--norc".into()]
}
impl Configuration {
    pub fn launch(&self) -> Result<Launch> {
        let spec: serde_json::Value = serde_json::from_slice(&fs::read(&self.build_spec)?)?;
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
            initial_closure: fs::read_to_string(string("closure_paths_file")?)?
                .lines()
                .map(PathBuf::from)
                .collect(),
            env,
            protected_paths: vec![],
            binds: serde_json::from_value(serde_json::json!({
                "rw_dirs": spec["rw_dirs"], "rw_files": spec["rw_files"],
                "ro_dirs": spec["ro_dirs"], "ro_files": spec["ro_files"],
            }))?,
        })
    }
}
