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
    os::{fd::RawFd, unix::process::CommandExt},
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

/// Where evaluation-time fetches leave the host.
pub enum Network {
    /// Host networking, for host-initiated launches.
    Host,
    /// No network, matching an offline goblin.
    None,
    /// The requesting goblin's user and network namespaces and its resolver.
    Goblin {
        user: RawFd,
        net: RawFd,
        resolv: PathBuf,
    },
}

pub struct Evaluator {
    root: PathBuf,
    bwrap: PathBuf,
    nix: PathBuf,
    git: Option<PathBuf>,
    cacert: Option<String>,
    network: Network,
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
    pub fn new(
        directory: &Path,
        bwrap: &Path,
        cacert: Option<&str>,
        network: Network,
        import_from_derivation: bool,
    ) -> Result<Self> {
        let root = directory.join("eval");
        for name in ["conf", "cache", "state", "roots"] {
            fs::create_dir_all(root.join(name))?;
        }
        // Evaluation before approval must never build: import-from-derivation
        // would run builds the preview cannot show.
        let mut configuration = CONFIGURATION.to_string();
        if !import_from_derivation {
            configuration.push_str("allow-import-from-derivation = false\n");
        }
        fs::write(root.join("conf/nix.conf"), configuration)?;
        Ok(Self {
            root,
            bwrap: bwrap.to_path_buf(),
            nix: store_command("nix").ok_or("host nix is not a Nix store command")?,
            git: store_command("git"),
            cacert: cacert.map(str::to_string),
            network,
        })
    }

    /// A private writable copy for computing a candidate lock.
    pub fn work(&self) -> PathBuf {
        self.root.join("work")
    }

    /// The host `git` in the store, for diffs of store trees.
    pub fn git(&self) -> Option<&Path> {
        self.git.as_deref()
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
        command.args(["--unshare-all", "--die-with-parent"]);
        command.args(["--uid", &uid, "--gid", &gid, "--clearenv"]);
        command.args(["--ro-bind", "/nix/store", "/nix/store"]);
        command.args(["--ro-bind", DAEMON_SOCKET, DAEMON_SOCKET]);
        let mut keep = Vec::new();
        match &self.network {
            Network::Host => {
                command.arg("--share-net");
                command.args(["--ro-bind-try", "/etc/resolv.conf", "/etc/resolv.conf"]);
                command.args(["--ro-bind-try", "/etc/hosts", "/etc/hosts"]);
            }
            Network::None => {}
            &Network::Goblin {
                user,
                net,
                ref resolv,
            } => {
                // Share the joined namespace; Bubblewrap nests its own user
                // namespace inside the goblin's, as the mount helper does.
                command.arg("--share-net");
                command.arg("--ro-bind").arg(resolv).arg("/etc/resolv.conf");
                keep.extend([user, net]);
                unsafe {
                    command.pre_exec(move || {
                        crate::unix::cvt(libc::setns(user, libc::CLONE_NEWUSER))?;
                        crate::unix::cvt(libc::setns(net, libc::CLONE_NEWNET))?;
                        Ok(())
                    });
                }
            }
        }
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
        process::command_with_fds(&mut command, log, cancel, &keep)
    }
}
