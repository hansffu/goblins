//! Host CLI maintenance. Persistent stores require explicit scope selection;
//! GC only collects temporary stores and resources of a stopped server.
use crate::Result;
use crate::docker;
use crate::process;
use crate::scope_storage;
use goblins_unix as unix;
use std::{
    collections::BTreeSet,
    ffi::CString,
    fs::{self, File},
    io::Write,
    os::{
        fd::AsRawFd,
        unix::{
            ffi::{OsStrExt, OsStringExt},
            fs::{MetadataExt, OpenOptionsExt},
            process::CommandExt,
        },
    },
    path::{Path, PathBuf},
    process::{Command, Stdio},
    time::Duration,
};

use goblins_runtime::lease::flock;

fn lock_root() -> Result<PathBuf> {
    unix::cache_directory("locks", true)
}

/// Serializes temporary-store creation against GC's enumeration and deletion.
/// Held only until a store owns its lifetime lease, not while members run.
pub(crate) fn storage_gate() -> Result<File> {
    flock(&lock_root()?.join("storage.lock"), false)?
        .ok_or_else(|| "Goblins storage maintenance is running; try again shortly".into())
}

pub(crate) fn temporary_lease(kind: &str, path: &Path) -> Result<File> {
    flock(&temporary_lock(kind, path)?, false)?
        .ok_or_else(|| "temporary storage is being collected".into())
}

fn temporary_lock(kind: &str, path: &Path) -> Result<PathBuf> {
    Ok(lock_root()?.join(format!(
        "{kind}-{}",
        path.file_name()
            .ok_or("missing storage name")?
            .to_string_lossy()
    )))
}

fn private_existing(path: &Path) -> Result<bool> {
    match fs::symlink_metadata(path) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(e) => Err(e.into()),
        Ok(_) => {
            unix::private_directory(path)?;
            Ok(true)
        }
    }
}

fn stored_names(root: &Path) -> Result<BTreeSet<String>> {
    let mut names = BTreeSet::new();
    if private_existing(root)? {
        for entry in fs::read_dir(root)? {
            let entry = entry?;
            if let Some(name) = entry
                .file_name()
                .to_str()
                .and_then(|s| s.strip_prefix("scope-"))
            {
                if !goblins_protocol::scope_name(name) {
                    return Err(
                        format!("invalid stored scope name at {}", entry.path().display()).into(),
                    );
                }
                names.insert(name.to_owned());
            }
        }
    }
    Ok(names)
}

pub fn clear_scope(runtime: Option<&Path>, name: Option<&str>, dry_run: bool) -> Result<()> {
    if name.is_some_and(|n| !goblins_protocol::scope_name(n)) {
        return Err("invalid scope name".into());
    }
    let _gate = storage_gate()?;
    let scopes = scope_storage::root(true)?;
    let engines = docker::storage_root(true)?;
    let names = if let Some(name) = name {
        BTreeSet::from([name.to_owned()])
    } else {
        stored_names(&scopes)?
            .union(&stored_names(&engines)?)
            .cloned()
            .collect()
    };
    // Acquire every selected scope/engine lock before deleting anything. Never
    // unlink these locks: a process may already have opened their inodes.
    let mut locks = Vec::new();
    let mut paths = Vec::new();
    for name in names {
        for root in [&scopes, &engines] {
            if !private_existing(root)? {
                continue;
            }
            let directory = root.join("locks");
            unix::private_directory(&directory)?;
            locks.push(flock(&directory.join(&name), true)?.ok_or_else(|| {
                format!("scope '{name}' is in use; stop its members before clearing it")
            })?);
            let path = root.join(format!("scope-{name}"));
            match fs::symlink_metadata(&path) {
                Ok(meta) if meta.is_dir() && meta.uid() == unsafe { libc::getuid() } => {
                    paths.push(path)
                }
                Ok(_) => {
                    return Err(format!("unsafe scope data directory {}", path.display()).into());
                }
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => (),
                Err(e) => return Err(e.into()),
            }
        }
    }
    let leases: Vec<_> = std::iter::once(&_gate).chain(locks.iter()).collect();
    for path in &paths {
        remove(runtime, path, true, dry_run, &leases)?;
    }
    if paths.is_empty() {
        println!("No stored scope data");
    }
    Ok(())
}

fn session_name(name: &str) -> bool {
    name.split_once("-s").is_some_and(|(instance, number)| {
        instance.len() == 32
            && instance.bytes().all(|b| b.is_ascii_hexdigit())
            && !number.is_empty()
            && number.bytes().all(|b| b.is_ascii_digit())
    })
}

pub fn gc(runtime: Option<&Path>, state: &Path, dry_run: bool) -> Result<()> {
    // Holding the gate excludes stores halfway through creation. Temporary
    // leases stay held by their owners, and by Docker's cleanup guardian.
    let gate = flock(&lock_root()?.join("storage.lock"), true)?
        .ok_or("Goblins storage is being opened or cleared; try GC again shortly")?;
    for (kind, root, prefix) in [
        (
            "scopes",
            unix::cache_directory("", false)?.join("scopes"),
            "temporary-",
        ),
        (
            "docker",
            unix::cache_directory("", false)?.join("docker"),
            "session-",
        ),
    ] {
        if !private_existing(&root)? {
            continue;
        }
        for entry in fs::read_dir(&root)? {
            let entry = entry?;
            let name = entry.file_name();
            let Some(suffix) = name.to_str().and_then(|n| n.strip_prefix(prefix)) else {
                continue;
            };
            if suffix.len() != 6 || !suffix.bytes().all(|b| b.is_ascii_alphanumeric()) {
                continue;
            }
            let path = entry.path();
            let lock_path = temporary_lock(kind, &path)?;
            match fs::symlink_metadata(&lock_path) {
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                    println!(
                        "Keeping legacy temporary storage without a lifetime lease: {}",
                        path.display()
                    );
                    continue;
                }
                Err(e) => return Err(e.into()),
                Ok(_) => (),
            }
            let Some(_lease) = flock(&lock_path, true)? else {
                println!("Keeping active storage {}", path.display());
                continue;
            };
            // Reject top-level links, even though the worker never follows
            // them, so a corrupt storage layout is visible to the operator.
            let meta = fs::symlink_metadata(&path)?;
            if !meta.is_dir() || meta.uid() != unsafe { libc::getuid() } {
                return Err(format!("unsafe temporary store {}", path.display()).into());
            }
            remove(runtime, &path, true, dry_run, &[&gate, &_lease])?;
            if !dry_run {
                fs::remove_file(lock_path)?;
            }
        }
    }
    // Normal teardown can leave empty lease files. The gate prevents a
    // temporary store from being created between this check and unlink.
    for entry in fs::read_dir(lock_root()?)? {
        let entry = entry?;
        let name = entry.file_name();
        let Some(name) = name.to_str() else {
            continue;
        };
        if name
            .strip_prefix("cleanup-")
            .is_some_and(|s| s.len() == 6 && s.bytes().all(|b| b.is_ascii_alphanumeric()))
        {
            let meta = fs::symlink_metadata(entry.path())?;
            if !meta.is_dir() || meta.uid() != unsafe { libc::getuid() } {
                return Err("unsafe cleanup scratch directory".into());
            }
            remove(runtime, &entry.path(), false, dry_run, &[&gate])?;
            continue;
        }
        let target = [
            ("scopes-", "scopes", "temporary-"),
            ("docker-", "docker", "session-"),
        ]
        .into_iter()
        .find_map(|(key, kind, prefix)| {
            let target = name.strip_prefix(key)?;
            let suffix = target.strip_prefix(prefix)?;
            (suffix.len() == 6 && suffix.bytes().all(|b| b.is_ascii_alphanumeric()))
                .then_some((kind, target))
        });
        if let Some((kind, target)) = target {
            let path = unix::cache_directory("", false)?.join(kind).join(target);
            if fs::symlink_metadata(path).is_err_and(|e| e.kind() == std::io::ErrorKind::NotFound)
                && let Some(_lease) = flock(&entry.path(), true)?
            {
                if dry_run {
                    println!("Would remove {}", entry.path().display());
                } else {
                    fs::remove_file(entry.path())?;
                }
            }
        }
    }
    drop(gate);
    if !private_existing(state)? {
        return Ok(());
    }
    // The background launcher also holds server-control.lock before opening
    // its log. Keep both locks, including their files, for the whole sweep.
    let Some(_management) = flock(&state.join("server-control.lock"), true)? else {
        println!("Keeping resources of a starting or stopping server");
        return Ok(());
    };
    let Some(_daemon) = flock(&state.join("daemon.lock"), true)? else {
        println!(
            "Keeping resources of the running server at {}",
            state.display()
        );
        return Ok(());
    };
    let scopes = state.join("scopes");
    if private_existing(&scopes)? {
        for entry in fs::read_dir(&scopes)? {
            let entry = entry?;
            let name = entry.file_name();
            if !name.to_str().is_some_and(goblins_protocol::scope_name) {
                return Err(
                    format!("invalid scope runtime name at {}", entry.path().display()).into(),
                );
            }
            if goblins_runtime::scope::remove_stale(&entry.path(), dry_run)? {
                println!(
                    "{} {}",
                    if dry_run { "Would remove" } else { "Removed" },
                    entry.path().display()
                );
            }
        }
    }
    for entry in fs::read_dir(state)? {
        let entry = entry?;
        let name = entry.file_name();
        let Some(name) = name.to_str() else {
            continue;
        };
        if name == "manifests" || session_name(name) {
            let path = entry.path();
            let meta = fs::symlink_metadata(&path)?;
            if !meta.is_dir() || meta.uid() != unsafe { libc::getuid() } {
                return Err(format!("unsafe server resource directory {}", path.display()).into());
            }
            let resources = path.join("resources");
            if session_name(name) {
                match fs::symlink_metadata(&resources) {
                    Ok(meta) if !meta.is_dir() || meta.uid() != unsafe { libc::getuid() } => {
                        return Err(
                            format!("unsafe session resources {}", resources.display()).into()
                        );
                    }
                    Err(e) if e.kind() != std::io::ErrorKind::NotFound => return Err(e.into()),
                    _ => (),
                }
            }
            let lease_path = resources.join(".gc-lease");
            let lease = if session_name(name) && lease_path.exists() {
                let Some(lease) = flock(&lease_path, true)? else {
                    println!("Keeping active session resources {}", path.display());
                    continue;
                };
                Some(lease)
            } else {
                None
            };
            let leases: Vec<_> = [&_management, &_daemon]
                .into_iter()
                .chain(lease.iter())
                .collect();
            remove(runtime, &path, false, dry_run, &leases)?;
        }
    }
    Ok(())
}

struct Worker(std::process::Child);
impl Drop for Worker {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}
struct Scratch(PathBuf);
impl Scratch {
    fn new() -> Result<Self> {
        let mut template =
            CString::new(lock_root()?.join("cleanup-XXXXXX").as_os_str().as_bytes())?
                .into_bytes_with_nul();
        if unsafe { libc::mkdtemp(template.as_mut_ptr().cast()) }.is_null() {
            return Err(std::io::Error::last_os_error().into());
        }
        template.pop();
        Ok(Self(PathBuf::from(std::ffi::OsString::from_vec(template))))
    }
}
impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn remove(
    runtime: Option<&Path>,
    path: &Path,
    mapped: bool,
    dry_run: bool,
    leases: &[&File],
) -> Result<()> {
    if dry_run {
        println!("Would remove {}", path.display());
        return Ok(());
    }
    let runtime =
        runtime.ok_or("cleanup requires --runtime (use the Nix-built goblins command)")?;
    let manifest = crate::config::validate(runtime)?;
    let helper = &manifest
        .goblins
        .values()
        .next()
        .ok_or("runtime contains no goblins")?
        .helper;
    let parent = File::options()
        .read(true)
        .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW)
        .open(path.parent().ok_or("missing storage parent")?)?;
    let directory_fd = parent.as_raw_fd();
    let lease_fds: Vec<_> = leases.iter().map(|f| f.as_raw_fd()).collect();
    let mut command = Command::new(helper);
    command
        .arg("--clean-storage")
        .arg(directory_fd.to_string())
        .arg(path.file_name().ok_or("missing storage name")?)
        .arg(if mapped { "mapped" } else { "host" })
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit());
    unsafe {
        command.pre_exec(move || {
            unix::cvt(libc::syscall(libc::SYS_close_range, 3u32, u32::MAX, 4u32) as i32)?;
            unix::cvt(libc::fcntl(directory_fd, libc::F_SETFD, 0))?;
            for &fd in &lease_fds {
                unix::cvt(libc::fcntl(fd, libc::F_SETFD, 0))?;
            }
            Ok(())
        });
    }
    let mut child = Worker(command.spawn()?);
    let mut info = File::from(std::os::fd::OwnedFd::from(
        child.0.stdout.take().ok_or("missing worker stdout")?,
    ));
    let cancel = process::Cancellation::default();
    if process::helper_line(&mut info, Duration::from_secs(10), &cancel)?.trim() != "ready" {
        return Err("storage cleanup worker did not become ready".into());
    }
    if mapped {
        let scratch = Scratch::new()?;
        for (kind, id) in [
            ("uid", unsafe { libc::getuid() }),
            ("gid", unsafe { libc::getgid() }),
        ] {
            docker::map_ids(
                kind,
                child.0.id(),
                id,
                docker::subordinate(kind, id)?,
                &scratch.0,
                &cancel,
            )?;
        }
    }
    child
        .0
        .stdin
        .take()
        .ok_or("missing worker stdin")?
        .write_all(b"x")?;
    let status = child.0.wait()?;
    if !status.success() {
        return Err(format!(
            "cleanup failed for {} ({status}); data may be partially removed",
            path.display()
        )
        .into());
    }
    println!("Removed {}", path.display());
    Ok(())
}
