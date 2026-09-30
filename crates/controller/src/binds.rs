//! Startup grants from trusted mkSandbox declarations. These are ordinary host
//! bind mounts, never snapshots or paths supplied by the sandbox request client.
use crate::{Result, process::Cancellation, session::store_path, unix};
use serde::Deserialize;
use std::{
    collections::{BTreeMap, BTreeSet},
    ffi::CString,
    fs::{self, File},
    os::{
        fd::{AsRawFd, RawFd},
        unix::ffi::OsStrExt,
    },
    path::{Component, Path, PathBuf},
};

#[derive(Clone, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Declarations {
    pub rw_dirs: Vec<String>,
    pub rw_files: Vec<String>,
    pub ro_dirs: Vec<String>,
    pub ro_files: Vec<String>,
}
pub struct Mount {
    // Pin the object, not merely its current pathname. This descriptor remains
    // CLOEXEC in the daemon and is inherited only by this launch's helper/bwrap.
    source_fd: File,
    source: PathBuf,
    destination: PathBuf,
    readonly: bool,
}
impl Mount {
    pub(crate) fn docker_grant(&self) -> std::io::Result<(File, PathBuf, bool)> {
        Ok((
            self.source_fd.try_clone()?,
            self.destination.clone(),
            self.readonly,
        ))
    }
}
pub struct Plan {
    pub home: PathBuf,
    /// Kept apart from the declared mounts so a child with another
    /// configuration can share its parent's pinned directory.
    working_directory: Option<Mount>,
    mounts: Vec<Mount>,
    pub store_targets: BTreeMap<PathBuf, PathBuf>,
}
impl Plan {
    /// Every mount in bind order: the working directory first, so declared
    /// read-only paths beneath it still take precedence.
    pub(crate) fn mounts(&self) -> impl Iterator<Item = &Mount> {
        self.working_directory.iter().chain(&self.mounts)
    }
    pub(crate) fn destinations(&self) -> impl Iterator<Item = &Path> {
        self.mounts().map(|m| m.destination.as_path())
    }
}

// Expand only the agent-sandbox path syntax, never a shell expression. Literal env
// values in mkGoblin.env do not pass through this function.
pub(crate) fn expand(value: &str, env: impl Fn(&str) -> Option<String>) -> Result<PathBuf> {
    let mut input = value;
    let mut output = String::new();
    if input.starts_with('~') {
        if input != "~" && !input.starts_with("~/") {
            return Err("host bind paths do not support ~user; use $HOME".into());
        }
        output.push_str(&env("HOME").ok_or("HOME is unset")?);
        input = &input[1..];
    }
    while let Some(index) = input.find('$') {
        output.push_str(&input[..index]);
        input = &input[index + 1..];
        let name;
        if input.starts_with('{') {
            let end = input
                .find('}')
                .ok_or("unclosed variable in host bind path")?;
            name = &input[1..end];
            input = &input[end + 1..];
        } else {
            let end = input
                .find(|c: char| !c.is_ascii_alphanumeric() && c != '_')
                .unwrap_or(input.len());
            name = &input[..end];
            input = &input[end..];
        }
        if name.is_empty()
            || !name
                .bytes()
                .enumerate()
                .all(|(i, c)| c.is_ascii_alphabetic() || c == b'_' || i > 0 && c.is_ascii_digit())
        {
            return Err("host bind paths accept $VAR or ${VAR}, not shell expressions".into());
        }
        output.push_str(&env(name).ok_or_else(|| format!("unset host bind variable: {name}"))?);
    }
    output.push_str(input);
    let path = PathBuf::from(output);
    if !path.is_absolute()
        || path.components().any(|c| matches!(c, Component::ParentDir))
        || path.as_os_str().as_bytes().contains(&0)
    {
        return Err(format!("host bind path must be absolute without '..': {value}").into());
    }
    Ok(path.components().collect())
}
fn overlaps(a: &Path, b: &Path) -> bool {
    a.starts_with(b) || b.starts_with(a)
}
pub(crate) fn validate(path: &Path, private: &[PathBuf]) -> Result<()> {
    // These trees establish the helper/control/store boundary. A startup grant
    // cannot replace them, expose their host counterparts, or cover an ancestor.
    for protected in [
        "/proc",
        "/sys",
        "/dev",
        "/nix",
        "/run",
        "/bin",
        "/workspace",
    ]
    .into_iter()
    .map(Path::new)
    .chain(private.iter().map(PathBuf::as_path))
    {
        if overlaps(path, protected) {
            return Err(format!(
                "host bind {} overlaps protected path {}",
                path.display(),
                protected.display()
            )
            .into());
        }
    }
    Ok(())
}
impl Declarations {
    pub fn plan(&self, private: &[PathBuf], cancel: &Cancellation) -> Result<Plan> {
        let env = |name: &str| std::env::var(name).ok();
        let home = expand("$HOME", env)?;
        validate(&home, &[])?;
        let mut plan = Plan {
            home,
            working_directory: None,
            mounts: vec![],
            store_targets: BTreeMap::new(),
        };
        for (paths, directory, readonly) in [
            (&self.rw_dirs, true, false),
            (&self.ro_dirs, true, true),
            (&self.rw_files, false, false),
            (&self.ro_files, false, true),
        ] {
            for raw in paths {
                cancel.check()?;
                let destination = expand(raw, env)?;
                validate(&destination, private)?;
                let source_fd = open_source(&destination)
                    .map_err(|e| format!("host bind {}: {e}", destination.display()))?;
                // Read the resolved location of the OPENED object. Resolving a
                // name first and opening it afterwards would introduce another
                // race during validation, including in any ancestor component.
                let source = fs::read_link(format!("/proc/self/fd/{}", source_fd.as_raw_fd()))?;
                // A declared symlink into the immutable store may select an exact
                // file/directory, but it cannot make that store content writable.
                if source.starts_with("/nix/store") {
                    store_root(&source)?;
                    if !readonly {
                        return Err("Nix store bind sources must be read-only".into());
                    }
                } else {
                    validate(&source, private)?;
                }
                let meta = source_fd.metadata()?;
                if if directory {
                    !meta.is_dir()
                } else {
                    !meta.is_file()
                } {
                    return Err(format!(
                        "host bind {} must be a {}",
                        destination.display(),
                        if directory {
                            "directory"
                        } else {
                            "regular file"
                        }
                    )
                    .into());
                }
                if plan
                    .mounts
                    .iter()
                    .any(|m| overlaps(&destination, &m.destination))
                {
                    return Err(format!(
                        "overlapping host bind destinations are not supported: {}",
                        destination.display()
                    )
                    .into());
                }
                if directory {
                    let dot = CString::new(".")?;
                    let dir = unix::owned(unsafe {
                        libc::openat(
                            source_fd.as_raw_fd(),
                            dot.as_ptr(),
                            libc::O_RDONLY | libc::O_DIRECTORY | libc::O_CLOEXEC,
                        )
                    })?;
                    scan(dir.as_raw_fd(), &source, &mut plan.store_targets, cancel)?;
                }
                plan.mounts.push(Mount {
                    source_fd,
                    source,
                    destination,
                    readonly,
                });
            }
        }
        Ok(plan)
    }
}
#[repr(C)]
struct OpenHow {
    flags: u64,
    mode: u64,
    resolve: u64,
}
fn open_source(path: &Path) -> Result<File> {
    let path = CString::new(path.as_os_str().as_bytes())?;
    let how = OpenHow {
        flags: (libc::O_PATH | libc::O_CLOEXEC) as u64,
        mode: 0,
        // Ordinary config symlinks are supported; procfs magic links must not
        // cross into another process's descriptors or mount namespace.
        resolve: 0x02, // RESOLVE_NO_MAGICLINKS
    };
    let fd = unix::owned(unsafe {
        libc::syscall(
            libc::SYS_openat2,
            libc::AT_FDCWD,
            path.as_ptr(),
            &how,
            size_of::<OpenHow>(),
        ) as i32
    })?;
    // Both execs remap stdio, so source descriptors must always be above it.
    Ok(File::from(unix::owned(unsafe {
        libc::fcntl(fd.as_raw_fd(), libc::F_DUPFD_CLOEXEC, 3)
    })?))
}
fn store_root(path: &Path) -> Result<PathBuf> {
    let name = path
        .strip_prefix("/nix/store")?
        .components()
        .next()
        .ok_or("cannot expose the entire Nix store")?;
    store_path(&Path::new("/nix/store").join(name))
}
// Read directory entries through pinned directory descriptors. Never recurse
// through host symlinks: a config link must not grant another host directory.
fn scan(
    dir: RawFd,
    source: &Path,
    targets: &mut BTreeMap<PathBuf, PathBuf>,
    cancel: &Cancellation,
) -> Result<()> {
    for entry in fs::read_dir(format!("/proc/self/fd/{dir}"))? {
        cancel.check()?;
        let entry = entry?;
        let name = entry.file_name();
        let meta = fs::symlink_metadata(entry.path())?;
        if meta.is_symlink() {
            let link = fs::read_link(entry.path())?;
            let landing = if link.is_absolute() {
                link
            } else {
                source.join(link)
            };
            // Immutable store symlinks (notably Home Manager) expose only the
            // selected target. Do not mount the whole home-manager-files output
            // or its closure, which could include unrelated private settings.
            if landing.starts_with("/nix/store")
                && !landing.components().any(|c| c == Component::ParentDir)
            {
                let Ok(resolved) = fs::canonicalize(&landing) else {
                    continue;
                };
                if resolved.starts_with("/nix/store") {
                    store_root(&landing)?;
                    store_root(&resolved)?;
                    targets.insert(landing, resolved);
                }
            }
        } else if meta.is_dir() {
            let name_c = CString::new(name.as_bytes())?;
            let child = unix::owned(unsafe {
                libc::openat(
                    dir,
                    name_c.as_ptr(),
                    libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
                )
            })?;
            scan(child.as_raw_fd(), &source.join(name), targets, cancel)?;
        }
    }
    Ok(())
}
impl Plan {
    pub fn add_working_directory(&mut self, cwd: &Path, private: &[PathBuf]) -> Result<()> {
        validate(cwd, private)?;
        let source_fd = open_source(cwd)?;
        let source = fs::read_link(format!("/proc/self/fd/{}", source_fd.as_raw_fd()))?;
        validate(&source, private)?;
        if !source_fd.metadata()?.is_dir() {
            return Err("working directory must be a directory".into());
        }
        // A directory bind includes descendants. Mount it before explicit binds
        // so configured read-only paths still take precedence. Pin the source
        // using the same descriptor mechanism as other startup grants.
        self.working_directory = Some(Mount {
            source_fd,
            source,
            destination: cwd.to_path_buf(),
            readonly: false,
        });
        Ok(())
    }
    /// Use the parent's pinned working directory: the exact object the root
    /// launch opened, not whatever its pathname names now.
    pub(crate) fn share_working_directory(&mut self, parent: &Plan) -> Result<()> {
        self.working_directory = parent
            .working_directory
            .as_ref()
            .map(|m| -> Result<Mount> {
                Ok(Mount {
                    source_fd: m.source_fd.try_clone()?,
                    source: m.source.clone(),
                    destination: m.destination.clone(),
                    readonly: m.readonly,
                })
            })
            .transpose()?;
        Ok(())
    }
    pub fn source_fds(&self) -> impl Iterator<Item = RawFd> + '_ {
        self.mounts().map(|m| m.source_fd.as_raw_fd())
    }
    pub fn store_roots(&self) -> Result<BTreeSet<PathBuf>> {
        self.store_targets
            .iter()
            .flat_map(|(a, b)| [a, b])
            .chain(
                self.mounts()
                    .map(|m| &m.source)
                    .filter(|p| p.starts_with("/nix/store")),
            )
            .map(|p| store_root(p))
            .collect()
    }
    pub fn append_args(
        &self,
        args: &mut Vec<String>,
        staging: &Path,
        initial: &BTreeSet<PathBuf>,
    ) -> Result<()> {
        for (destination, source) in &self.store_targets {
            if initial.iter().any(|p| destination.starts_with(p)) {
                continue;
            }
            let placeholder = staging.join(destination.strip_prefix("/nix/store")?);
            // The backing store skeleton is host-private and read-only in the
            // sandbox. Prepare nested destinations before Bubblewrap sees it.
            if source.is_dir() {
                fs::create_dir_all(placeholder)?;
            } else {
                fs::create_dir_all(placeholder.parent().unwrap())?;
                File::create(placeholder)?;
            }
            args.extend([
                "--ro-bind".into(),
                source.display().to_string(),
                destination.display().to_string(),
            ]);
        }
        for mount in self.mounts() {
            args.extend([
                if mount.readonly {
                    "--ro-bind-fd"
                } else {
                    "--bind-fd"
                }
                .into(),
                mount.source_fd.as_raw_fd().to_string(),
                mount.destination.display().to_string(),
            ]);
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn path_expansion_is_not_shell_evaluation() {
        let env = |name: &str| {
            if name == "HOME" {
                Some("/home/test".into())
            } else {
                None
            }
        };
        for input in [
            "~/.config/fish",
            "$HOME/.config/fish",
            "${HOME}/.config/fish",
        ] {
            assert_eq!(
                expand(input, env).unwrap(),
                Path::new("/home/test/.config/fish")
            );
        }
        for input in [
            "~someone/file",
            "$MISSING/file",
            "$(id)",
            "${HOME:-/tmp}",
            "relative",
            "$HOME/../secret",
        ] {
            assert!(expand(input, env).is_err(), "{input}");
        }
    }
    #[test]
    fn reserved_trees_cannot_be_covered_or_exposed() {
        for input in [
            "/",
            "/nix",
            "/nix/var/nix/daemon-socket",
            "/run/goblins",
            "/proc/self/root",
            "/workspace/subdir",
        ] {
            assert!(validate(Path::new(input), &[]).is_err());
        }
        assert!(validate(Path::new("/tmp"), &["/tmp/session-private".into()]).is_err());
        assert!(validate(Path::new("/home/test/.config/fish"), &[]).is_ok());
    }
    #[test]
    fn shared_working_directory_stays_first_and_pinned() {
        let root = unix::temp_directory().unwrap();
        let (work, declared) = (root.join("work"), root.join("declared"));
        fs::create_dir(&work).unwrap();
        fs::create_dir(&declared).unwrap();
        let cancel = Cancellation::default();
        let mut parent = Declarations::default().plan(&[], &cancel).unwrap();
        parent.add_working_directory(&work, &[]).unwrap();
        // The child's own declared bind, opened by the child's launch.
        let mut child = Declarations {
            ro_dirs: vec![declared.display().to_string()],
            ..Default::default()
        }
        .plan(&[], &cancel)
        .unwrap();
        // Replace the pathname: the child must get the parent's object.
        let moved = root.join("moved");
        fs::rename(&work, &moved).unwrap();
        fs::create_dir(&work).unwrap();
        child.share_working_directory(&parent).unwrap();
        assert_eq!(
            child.destinations().collect::<Vec<_>>(),
            [&*work, &*declared]
        );
        let fds: Vec<_> = child.source_fds().collect();
        assert_eq!(fds.len(), 2);
        assert_ne!(fds[0], parent.source_fds().next().unwrap());
        assert_eq!(
            fs::read_link(format!("/proc/self/fd/{}", fds[0])).unwrap(),
            moved
        );
        let mut args = vec![];
        child
            .append_args(&mut args, &root, &BTreeSet::new())
            .unwrap();
        assert_eq!(
            args,
            [
                "--bind-fd".to_string(),
                fds[0].to_string(),
                work.display().to_string(),
                "--ro-bind-fd".into(),
                fds[1].to_string(),
                declared.display().to_string(),
            ]
        );
        drop((parent, child));
        fs::remove_dir_all(root).unwrap();
    }
}
