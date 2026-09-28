//! Host-side evaluator sandbox for flake commands (ADR 0007). Nix runs under
//! Bubblewrap with the store, the daemon socket, the flake source and a
//! private state directory only: no host home, SSH agent, netrc, access tokens
//! or system Nix configuration. Builds still go through the host daemon.
use crate::{
    Result,
    process::{self, Cancellation},
};
use std::{
    ffi::OsString,
    fs,
    path::{Path, PathBuf},
    process::Command,
};

/// Goblins-owned client configuration; the host's `nix.conf`, netrc and
/// access tokens are never read. The empty registry disables the global one.
const CONFIGURATION: &str = "\
experimental-features = nix-command flakes
accept-flake-config = false
flake-registry =
";
const DAEMON_SOCKET: &str = "/nix/var/nix/daemon-socket";

pub struct Evaluator {
    root: PathBuf,
    bwrap: PathBuf,
    nix: PathBuf,
    git: Option<PathBuf>,
    cacert: Option<String>,
}

/// Resolve a host command to its canonical store path; the evaluator sees the
/// store, never the host's profile directories.
fn store_command(name: &str) -> Option<PathBuf> {
    std::env::var_os("PATH")?
        .to_str()?
        .split(':')
        .map(|dir| Path::new(dir).join(name))
        .find(|p| p.is_file())
        .and_then(|p| fs::canonicalize(p).ok())
        .filter(|p| p.starts_with("/nix/store/"))
}

/// The directory Nix treats as the flake's source tree: the nearest ancestor
/// containing `.git`, or the flake directory itself.
pub fn source_tree(flake: &Path) -> PathBuf {
    flake
        .ancestors()
        .find(|dir| dir.join(".git").exists())
        .unwrap_or(flake)
        .to_path_buf()
}

impl Evaluator {
    /// `directory` is the session's private host directory. The evaluator
    /// state (configuration, fetcher cache, GC-root links) lives beneath it and
    /// is deleted with the session.
    pub fn new(directory: &Path, bwrap: &Path, cacert: Option<&str>) -> Result<Self> {
        let root = directory.join("eval");
        for name in ["conf", "cache", "state", "roots"] {
            fs::create_dir_all(root.join(name))?;
        }
        fs::write(root.join("conf/nix.conf"), CONFIGURATION)?;
        Ok(Self {
            root,
            bwrap: bwrap.to_path_buf(),
            nix: store_command("nix").ok_or("host nix is not a Nix store command")?,
            git: store_command("git"),
            cacert: cacert.map(str::to_string),
        })
    }

    /// Where GC-root links for evaluated outputs must be created.
    pub fn roots(&self) -> PathBuf {
        self.root.join("roots")
    }

    /// Run one `nix` command. `visible` paths are bound read-only at their host
    /// location, for example the flake source tree.
    pub fn nix(
        &self,
        args: &[OsString],
        visible: &[PathBuf],
        log: &Path,
        cancel: &Cancellation,
    ) -> Result<String> {
        let uid = unsafe { libc::getuid() }.to_string();
        let gid = unsafe { libc::getgid() }.to_string();
        let mut path = vec![self.nix.parent().unwrap().display().to_string()];
        if let Some(git) = &self.git {
            path.push(git.parent().unwrap().display().to_string());
        }
        let mut command = Command::new(&self.bwrap);
        command.args(["--unshare-all", "--share-net", "--die-with-parent"]);
        command.args(["--uid", &uid, "--gid", &gid, "--clearenv"]);
        command.args(["--ro-bind", "/nix/store", "/nix/store"]);
        command.args(["--ro-bind", DAEMON_SOCKET, DAEMON_SOCKET]);
        command.args(["--ro-bind-try", "/etc/resolv.conf", "/etc/resolv.conf"]);
        command.args(["--ro-bind-try", "/etc/hosts", "/etc/hosts"]);
        // An empty private home: no dotfiles, SSH keys or credentials.
        command.args(["--proc", "/proc", "--dev", "/dev", "--tmpfs", "/tmp"]);
        for dir in visible {
            command.arg("--ro-bind").arg(dir).arg(dir);
        }
        command.arg("--bind").arg(&self.root).arg(&self.root);
        let env: [(&str, OsString); 9] = [
            ("HOME", "/tmp".into()),
            ("PATH", path.join(":").into()),
            ("NIX_REMOTE", "daemon".into()),
            ("NIX_CONF_DIR", self.root.join("conf").into()),
            (
                "NIX_USER_CONF_FILES",
                self.root.join("conf/nix.conf").into(),
            ),
            ("XDG_CACHE_HOME", self.root.join("cache").into()),
            ("XDG_STATE_HOME", self.root.join("state").into()),
            ("XDG_CONFIG_HOME", "/tmp/.config".into()),
            ("TMPDIR", "/tmp".into()),
        ];
        for (name, value) in env {
            command.arg("--setenv").arg(name).arg(value);
        }
        if let Some(cacert) = &self.cacert {
            command.args(["--setenv", "NIX_SSL_CERT_FILE", cacert]);
        }
        command.arg("--").arg(&self.nix).args(args);
        process::command(&mut command, log, cancel)
    }
}
