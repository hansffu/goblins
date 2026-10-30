//! Docker ownership belongs exclusively to the scope process. A member's
//! connection counts use; retained donor capabilities live until engine stop.
use crate::{
    Result,
    docker::{Engine, Filesystem, storage_root},
    process::{self, Cancellation},
    scope_keeper::NamespaceFds,
};
use goblins_runtime::{
    ResourceLease,
    channel::{Descriptor, Frame},
    scope::{DockerConfig, DockerPart},
};
use std::{
    fs::{self, File},
    os::{
        fd::{AsRawFd, OwnedFd},
        unix::fs::MetadataExt,
    },
    path::{Path, PathBuf},
    process::Command,
};

pub(crate) enum Part {
    Arguments(Vec<String>),
    Bind(File, PathBuf, bool),
    Store,
    Grant(File, PathBuf, bool),
    Lease(ResourceLease),
}
impl Part {
    pub fn decode(parts: Vec<DockerPart>, frame: Frame) -> Result<Vec<Self>> {
        let mut fds: Vec<_> = frame.fds.into_iter().map(Some).collect();
        let mut take = |d: Descriptor| -> Result<OwnedFd> {
            fds.get_mut(d.index)
                .and_then(Option::take)
                .ok_or_else(|| "invalid Docker descriptor".into())
        };
        parts
            .into_iter()
            .map(|part| {
                Ok(match part {
                    DockerPart::Arguments { values } => Self::Arguments(values),
                    DockerPart::Bind {
                        source,
                        destination,
                        readonly,
                    } => Self::Bind(take(source)?.into(), destination, readonly),
                    DockerPart::Store {} => Self::Store,
                    DockerPart::Grant {
                        source,
                        destination,
                        readonly,
                    } => Self::Grant(take(source)?.into(), destination, readonly),
                    DockerPart::Lease { directory, lock } => {
                        Self::Lease(ResourceLease::from_parts(directory, take(lock)?)?)
                    }
                })
            })
            .collect()
    }
    pub fn encode(self, fds: &mut Vec<OwnedFd>) -> DockerPart {
        let mut add = |fd: OwnedFd| {
            let descriptor = Descriptor::new(fds.len());
            fds.push(fd);
            descriptor
        };
        match self {
            Self::Arguments(values) => DockerPart::Arguments { values },
            Self::Bind(source, destination, readonly) => DockerPart::Bind {
                source: add(source.into()),
                destination,
                readonly,
            },
            Self::Store => DockerPart::Store {},
            Self::Grant(source, destination, readonly) => DockerPart::Grant {
                source: add(source.into()),
                destination,
                readonly,
            },
            Self::Lease(lease) => {
                let (directory, lock) = lease.into_parts();
                DockerPart::Lease {
                    directory,
                    lock: add(lock),
                }
            }
        }
    }
}
/// Convert a member's local bwrap plan into typed operations while its source
/// fds still mean something. Only typed Bind fields cross the process boundary.
pub(crate) fn filesystem(args: &[String]) -> Result<Vec<Part>> {
    let mut parts = vec![];
    let mut i = 0;
    while i < args.len() {
        let flag = args[i].as_str();
        let count = match flag {
            "--setenv" | "--symlink" | "--bind-fd" | "--ro-bind-fd" | "--bind" | "--ro-bind" => 3,
            _ => 2,
        };
        if i + count > args.len() {
            return Err("invalid engine filesystem plan".into());
        }
        if matches!(flag, "--bind" | "--ro-bind" | "--bind-fd" | "--ro-bind-fd") {
            let destination = PathBuf::from(&args[i + 2]);
            if matches!(
                destination.to_str(),
                Some("/run/goblins/request.sock" | "/run/goblins/packages" | "/run/goblins/bin")
            ) {
                i += count;
                continue;
            }
            if destination == Path::new("/nix/store") {
                parts.push(Part::Store);
            } else {
                let file = if flag.ends_with("-fd") {
                    let fd: i32 = args[i + 1].parse()?;
                    goblins_unix::owned(unsafe { libc::fcntl(fd, libc::F_DUPFD_CLOEXEC, 3) })?
                        .into()
                } else {
                    File::open(&args[i + 1])?
                };
                parts.push(Part::Bind(file, destination, flag.starts_with("--ro-")));
            }
        } else {
            parts.push(Part::Arguments(args[i..i + count].to_vec()));
        }
        i += count;
    }
    Ok(parts)
}
pub(crate) struct Transaction {
    pub extending: bool,
    pub config: Option<DockerConfig>,
    pub parts: Vec<Part>,
}
type Granted = ((u64, u64), PathBuf, bool);
fn identity(file: &File) -> std::io::Result<(u64, u64)> {
    let m = file.metadata()?;
    Ok((m.dev(), m.ino()))
}
#[derive(Default)]
pub(crate) struct Docker {
    engine: Option<Engine>,
    config: Option<DockerConfig>,
    grants: Vec<Granted>,
    leases: Vec<ResourceLease>,
    users: usize,
}
impl Docker {
    pub fn alive(&mut self) -> bool {
        self.engine.as_mut().is_none_or(Engine::alive)
    }
    pub fn apply(
        &mut self,
        transaction: Transaction,
        directory: &Path,
        namespaces: &NamespaceFds,
        allow_nix: bool,
        name: Option<&str>,
        member: std::os::fd::RawFd,
    ) -> Result<PathBuf> {
        let cancel = Cancellation::default();
        let mut grants = vec![];
        let mut leases = vec![];
        let mut filesystem = vec![];
        for part in transaction.parts {
            match part {
                Part::Grant(source, dest, ro) => grants.push((source, dest, ro)),
                Part::Lease(lease) => leases.push(lease),
                part if !transaction.extending => filesystem.push(part),
                _ => return Err("ExtendDocker may only transfer grants and leases".into()),
            }
        }
        if leases.is_empty() {
            return Err("Docker requires donor resource leases".into());
        }
        // Check the whole transaction before changing a live engine, including
        // conflicts within a single member's batch stream.
        let mut checked = self.grants.clone();
        for (source, dest, ro) in &grants {
            check(&checked, identity(source)?, dest, *ro)?;
            checked.push((identity(source)?, dest.clone(), *ro));
        }
        if self.engine.is_none() {
            if transaction.extending {
                return Err("scope Docker engine stopped".into());
            }
            let config = transaction.config.ok_or("missing Docker configuration")?;
            if !config.helper.is_absolute()
                || !config.bwrap.is_absolute()
                || !config.daemon.is_absolute()
            {
                return Err("invalid Docker configuration paths".into());
            }
            let store = directory.join("store");
            if store.exists() {
                goblins_unix::cleanup::remove_at(
                    &File::open(directory)?,
                    std::ffi::OsStr::new("store"),
                )?;
            }
            fs::create_dir(&store)?;
            fs::create_dir_all(directory.join("docker-socket"))?;
            let mut args = vec![];
            let mut sources = vec![];
            for part in filesystem {
                match part {
                    Part::Arguments(values) => args.extend(values),
                    Part::Store => args.extend([
                        "--ro-bind".into(),
                        if allow_nix {
                            "/nix/store".into()
                        } else {
                            store.display().to_string()
                        },
                        "/nix/store".into(),
                    ]),
                    Part::Bind(file, dest, ro) => {
                        placeholder(&store, &file, &dest, allow_nix)?;
                        args.extend([
                            if ro { "--ro-bind-fd" } else { "--bind-fd" }.into(),
                            file.as_raw_fd().to_string(),
                            dest.display().to_string(),
                        ]);
                        sources.push(file);
                    }
                    _ => unreachable!(),
                }
            }
            // Grants include the launch closure and paths added after the
            // original filesystem plan was built.
            for (file, dest, ro) in &grants {
                placeholder(&store, file, dest, allow_nix)?;
                args.extend([
                    if *ro { "--ro-bind-fd" } else { "--bind-fd" }.into(),
                    file.as_raw_fd().to_string(),
                    dest.display().to_string(),
                ]);
            }
            let fds: Vec<_> = sources
                .iter()
                .map(AsRawFd::as_raw_fd)
                .chain(grants.iter().map(|(f, _, _)| f.as_raw_fd()))
                .collect();
            let engine = Engine::start(
                &config,
                Filesystem {
                    args: &args,
                    fds: &fds,
                },
                &storage_root(true)?,
                directory,
                &leases,
                &Cancellation::until_readable(member)?,
                namespaces,
                name,
            )?;
            self.engine = Some(engine);
            self.config = Some(config);
        } else {
            self.engine.as_mut().unwrap().retain(&leases, &cancel)?;
            // Partial live bindings are retained even when a later bind fails.
            self.leases.append(&mut leases);
            for (file, dest, ro) in &grants {
                self.bind(directory, namespaces, file, dest, *ro, allow_nix, &cancel)?;
            }
        }
        self.leases.append(&mut leases);
        for (file, dest, ro) in grants {
            if !self.grants.iter().any(|(_, old, _)| old == &dest) {
                self.grants.push((identity(&file)?, dest, ro));
            }
        }
        if !transaction.extending {
            self.users += 1;
        }
        Ok(directory.join("docker-socket/docker.sock"))
    }
    fn bind(
        &mut self,
        directory: &Path,
        namespaces: &NamespaceFds,
        source: &File,
        dest: &Path,
        ro: bool,
        allow_nix: bool,
        cancel: &Cancellation,
    ) -> Result<()> {
        if self.grants.iter().any(|(_, old, _)| old == dest) {
            return Ok(());
        }
        placeholder(&directory.join("store"), source, dest, allow_nix)?;
        let engine = self.engine.as_ref().ok_or("scope Docker engine stopped")?;
        let keep = [
            namespaces.outer.as_raw_fd(),
            engine.mounts.as_raw_fd(),
            engine.root.as_raw_fd(),
            source.as_raw_fd(),
        ];
        let mut command = Command::new(
            &self
                .config
                .as_ref()
                .ok_or("missing engine configuration")?
                .helper,
        );
        command
            .arg("--docker-bind")
            .args(keep.map(|fd| fd.to_string()))
            .arg(dest)
            .arg(if ro { "ro" } else { "rw" });
        process::command_with_fds(&mut command, directory, cancel, &keep)?;
        self.grants
            .push((identity(source)?, dest.to_path_buf(), ro));
        Ok(())
    }
    pub fn detach(&mut self) {
        self.users -= 1;
        if self.users == 0 {
            self.stop();
        }
    }
    fn stop(&mut self) {
        self.engine.take();
        self.leases.clear();
        self.grants.clear();
        self.config = None;
    }
}
impl Drop for Docker {
    fn drop(&mut self) {
        self.stop();
    }
}
fn placeholder(store: &Path, source: &File, dest: &Path, allow_nix: bool) -> Result<()> {
    if !allow_nix && let Ok(relative) = dest.strip_prefix("/nix/store") {
        if crate::session::containing_store_output(dest).is_none()
            || relative
                .components()
                .any(|c| !matches!(c, std::path::Component::Normal(_)))
        {
            return Err("invalid store grant".into());
        }
        // Bind declarations may target descendants of an output selected by
        // a store symlink. Preserve that full destination in the new skeleton.
        let target = store.join(relative);
        if source.metadata()?.is_dir() {
            fs::create_dir_all(target)?;
        } else if !target.exists() {
            fs::create_dir_all(target.parent().ok_or("store grant has no parent")?)?;
            File::create(target)?;
        }
    }
    Ok(())
}
fn check(grants: &[Granted], source: (u64, u64), dest: &Path, ro: bool) -> Result<()> {
    for (old, old_dest, old_ro) in grants {
        if old_dest == dest {
            if (*old, *old_ro) != (source, ro) {
                return Err(format!("scope Docker mount conflict at {}: one engine cannot give members different sources or access modes at the same path", dest.display()).into());
            }
        } else if dest.starts_with(old_dest) || old_dest.starts_with(dest) {
            return Err(format!(
                "scope Docker engine has overlapping grants at {} and {}",
                dest.display(),
                old_dest.display()
            )
            .into());
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn engine_filesystem_pins_member_fds_and_omits_private_endpoints() {
        let root = goblins_unix::temp_directory().unwrap();
        let path = root.join("source");
        fs::create_dir(&path).unwrap();
        fs::write(path.join("marker"), "original").unwrap();
        let source = File::open(&path).unwrap();
        let args = vec![
            "--bind-fd".into(),
            source.as_raw_fd().to_string(),
            "/work".into(),
            "--ro-bind".into(),
            "/private/request.sock".into(),
            "/run/goblins/request.sock".into(),
            "--ro-bind".into(),
            "/private/store".into(),
            "/nix/store".into(),
        ];
        let parts = filesystem(&args).unwrap();
        fs::rename(&path, root.join("moved")).unwrap();
        fs::create_dir(&path).unwrap();
        fs::write(path.join("marker"), "replacement").unwrap();
        let Part::Bind(pinned, dest, ro) = &parts[0] else {
            panic!("descriptor bind lost");
        };
        assert_eq!(dest, Path::new("/work"));
        assert!(!ro);
        assert_eq!(identity(pinned).unwrap(), identity(&source).unwrap());
        assert_ne!(
            identity(pinned).unwrap(),
            identity(&File::open(path).unwrap()).unwrap()
        );
        assert_eq!(
            fs::read_to_string(format!("/proc/self/fd/{}/marker", pinned.as_raw_fd())).unwrap(),
            "original"
        );
        assert_eq!(parts.len(), 2);
        assert!(matches!(parts[1], Part::Store));
        drop((parts, source));
        fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn engine_conflicts_preserve_source_access_mode_and_overlap_boundaries() {
        let grants = vec![((1, 2), PathBuf::from("/shared"), true)];
        assert!(check(&grants, (1, 2), Path::new("/shared"), true).is_ok());
        for (source, dest, ro) in [
            ((1, 3), "/shared", true),
            ((1, 2), "/shared", false),
            ((1, 2), "/shared/child", true),
            ((1, 2), "/", true),
        ] {
            assert!(check(&grants, source, Path::new(dest), ro).is_err());
        }
        assert!(check(&grants, (1, 2), Path::new("/shared-other"), true).is_ok());
    }
    #[test]
    fn scope_store_keeps_nested_store_symlink_destinations() {
        let root = goblins_unix::temp_directory().unwrap();
        let store = root.join("store");
        fs::create_dir(&store).unwrap();
        let file = File::create(root.join("source")).unwrap();
        let output = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-package";
        let destination = Path::new("/nix/store").join(output).join("lib/target");
        placeholder(&store, &file, &destination, false).unwrap();
        assert!(store.join(output).join("lib/target").is_file());
        assert!(
            placeholder(
                &store,
                &file,
                &destination.join("../../../../outside"),
                false
            )
            .is_err()
        );
        assert!(!root.join("outside").exists());
        fs::remove_dir_all(root).unwrap();
    }
}
