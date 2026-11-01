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
pub(crate) struct Context<'a> {
    pub output: &'a mut dyn Write,
    pub error: &'a File,
    pub cancel: &'a process::Cancellation,
}

fn lock_root() -> Result<PathBuf> {
    unix::cache_directory("locks", true)
}

/// Serializes temporary-store creation against GC's enumeration and deletion.
/// Held only until a store owns its lifetime lease, not while members run.
pub(crate) fn storage_gate() -> Result<File> {
    storage_gate_at(&lock_root()?)
}

pub(crate) fn storage_gate_at(locks: &Path) -> Result<File> {
    flock(&locks.join("storage.lock"), false)?
        .ok_or_else(|| "Goblins storage maintenance is running; try again shortly".into())
}

pub(crate) fn temporary_lease(kind: &str, path: &Path) -> Result<File> {
    temporary_lease_at(&lock_root()?, kind, path)
}

pub(crate) fn temporary_lease_at(locks: &Path, kind: &str, path: &Path) -> Result<File> {
    flock(&temporary_lock_at(locks, kind, path)?, false)?
        .ok_or_else(|| "temporary storage is being collected".into())
}

fn temporary_lock(kind: &str, path: &Path) -> Result<PathBuf> {
    temporary_lock_at(&lock_root()?, kind, path)
}

fn temporary_lock_at(locks: &Path, kind: &str, path: &Path) -> Result<PathBuf> {
    Ok(locks.join(format!(
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

fn stored_names(root: &Path, context: &Context) -> Result<BTreeSet<String>> {
    let mut names = BTreeSet::new();
    if private_existing(root)? {
        for entry in fs::read_dir(root)? {
            context.cancel.check()?;
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

pub(crate) fn clear_scope(
    runtime: Option<&Path>,
    name: Option<&str>,
    dry_run: bool,
    context: &mut Context,
) -> Result<()> {
    if name.is_some_and(|n| !goblins_protocol::scope_name(n)) {
        return Err("invalid scope name".into());
    }
    context.cancel.check()?;
    let _gate = storage_gate()?;
    let scopes = scope_storage::root(true)?;
    let engines = docker::storage_root(true)?;
    let names = if let Some(name) = name {
        BTreeSet::from([name.to_owned()])
    } else {
        stored_names(&scopes, context)?
            .union(&stored_names(&engines, context)?)
            .cloned()
            .collect()
    };
    // Acquire every selected scope/engine lock before deleting anything. Never
    // unlink these locks: a process may already have opened their inodes.
    let mut locks = Vec::new();
    let mut paths = Vec::new();
    for name in names {
        context.cancel.check()?;
        for root in [&scopes, &engines] {
            context.cancel.check()?;
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
        remove(runtime, path, true, dry_run, &leases, context)?;
    }
    if paths.is_empty() {
        writeln!(context.output, "No stored scope data")?;
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

pub(crate) fn gc(
    runtime: Option<&Path>,
    state: &Path,
    dry_run: bool,
    context: &mut Context,
) -> Result<()> {
    context.cancel.check()?;
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
            context.cancel.check()?;
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
                    writeln!(
                        context.output,
                        "Keeping legacy temporary storage without a lifetime lease: {}",
                        path.display()
                    )?;
                    continue;
                }
                Err(e) => return Err(e.into()),
                Ok(_) => (),
            }
            let Some(_lease) = flock(&lock_path, true)? else {
                writeln!(context.output, "Keeping active storage {}", path.display())?;
                continue;
            };
            // Reject top-level links, even though the worker never follows
            // them, so a corrupt storage layout is visible to the operator.
            let meta = fs::symlink_metadata(&path)?;
            if !meta.is_dir() || meta.uid() != unsafe { libc::getuid() } {
                return Err(format!("unsafe temporary store {}", path.display()).into());
            }
            remove(runtime, &path, true, dry_run, &[&gate, &_lease], context)?;
            if !dry_run {
                context.cancel.check()?;
                fs::remove_file(lock_path)?;
            }
        }
    }
    // Normal teardown can leave empty lease files. The gate prevents a
    // temporary store from being created between this check and unlink.
    for entry in fs::read_dir(lock_root()?)? {
        context.cancel.check()?;
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
            remove(runtime, &entry.path(), false, dry_run, &[&gate], context)?;
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
                    writeln!(context.output, "Would remove {}", entry.path().display())?;
                } else {
                    context.cancel.check()?;
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
        writeln!(
            context.output,
            "Keeping resources of a starting or stopping server"
        )?;
        return Ok(());
    };
    let Some(_daemon) = flock(&state.join("daemon.lock"), true)? else {
        writeln!(
            context.output,
            "Keeping resources of the running server at {}",
            state.display()
        )?;
        return Ok(());
    };
    let scopes = state.join("scopes");
    if private_existing(&scopes)? {
        for entry in fs::read_dir(&scopes)? {
            context.cancel.check()?;
            let entry = entry?;
            let name = entry.file_name();
            if !name.to_str().is_some_and(goblins_protocol::scope_name) {
                return Err(
                    format!("invalid scope runtime name at {}", entry.path().display()).into(),
                );
            }
            if goblins_runtime::scope::remove_stale(&entry.path(), dry_run)? {
                writeln!(
                    context.output,
                    "{} {}",
                    if dry_run { "Would remove" } else { "Removed" },
                    entry.path().display()
                )?;
            }
        }
    }
    for entry in fs::read_dir(state)? {
        context.cancel.check()?;
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
                    writeln!(
                        context.output,
                        "Keeping active session resources {}",
                        path.display()
                    )?;
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
            remove(runtime, &path, false, dry_run, &leases, context)?;
        }
    }
    Ok(())
}

#[cfg(test)]
std::thread_local! {
    static SCRATCH_REAPS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}
struct Scratch {
    path: PathBuf,
    #[cfg(test)]
    worker: libc::pid_t,
}
impl Scratch {
    fn new(worker: u32) -> Result<Self> {
        let mut template =
            CString::new(lock_root()?.join("cleanup-XXXXXX").as_os_str().as_bytes())?
                .into_bytes_with_nul();
        if unsafe { libc::mkdtemp(template.as_mut_ptr().cast()) }.is_null() {
            return Err(std::io::Error::last_os_error().into());
        }
        template.pop();
        #[cfg(not(test))]
        let _ = worker;
        Ok(Self {
            path: PathBuf::from(std::ffi::OsString::from_vec(template)),
            #[cfg(test)]
            worker: worker as libc::pid_t,
        })
    }
}
impl Drop for Scratch {
    fn drop(&mut self) {
        #[cfg(test)]
        {
            // Kernel observation at the cleanup boundary: the real helper
            // must already have been reaped, not merely signalled or exited.
            let mut status: libc::siginfo_t = unsafe { std::mem::zeroed() };
            assert_eq!(
                unsafe {
                    libc::waitid(
                        libc::P_PID,
                        self.worker as libc::id_t,
                        &mut status,
                        libc::WEXITED | libc::WNOHANG | libc::WNOWAIT,
                    )
                },
                -1,
                "scratch cleanup preceded helper reap"
            );
            assert_eq!(
                std::io::Error::last_os_error().raw_os_error(),
                Some(libc::ECHILD)
            );
            SCRATCH_REAPS.with(|count| count.set(count.get() + 1));
        }
        let _ = fs::remove_dir_all(&self.path);
    }
}

fn remove(
    runtime: Option<&Path>,
    path: &Path,
    mapped: bool,
    dry_run: bool,
    leases: &[&File],
    context: &mut Context,
) -> Result<()> {
    context.cancel.check()?;
    if dry_run {
        writeln!(context.output, "Would remove {}", path.display())?;
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
    context.cancel.check()?;
    let mut command = Command::new(helper);
    command
        .arg("--clean-storage")
        .arg(directory_fd.to_string())
        .arg(path.file_name().ok_or("missing storage name")?)
        .arg(if mapped { "mapped" } else { "host" })
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::from(context.error.try_clone()?))
        .process_group(0);
    let owner = unsafe { libc::getpid() };
    unsafe {
        command.pre_exec(move || {
            unix::cvt(libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL))?;
            if libc::getppid() != owner {
                return Err(std::io::Error::from_raw_os_error(libc::ESRCH));
            }
            unix::cvt(libc::syscall(libc::SYS_close_range, 3u32, u32::MAX, 4u32) as i32)?;
            unix::cvt(libc::fcntl(directory_fd, libc::F_SETFD, 0))?;
            for &fd in &lease_fds {
                unix::cvt(libc::fcntl(fd, libc::F_SETFD, 0))?;
            }
            Ok(())
        });
    }
    // Reverse lexical drop order is part of the safety contract: reap the
    // helper before scratch cleanup, and before the caller releases leases.
    let mut scratch = None;
    let mut child = process::CommandChild::new(command.spawn()?);
    let mut info = File::from(std::os::fd::OwnedFd::from(
        child.child.stdout.take().ok_or("missing worker stdout")?,
    ));
    let cancel = context.cancel;
    if process::helper_line(&mut info, Duration::from_secs(10), cancel)?.trim() != "ready" {
        return Err("storage cleanup worker did not become ready".into());
    }
    if mapped {
        scratch = Some(Scratch::new(child.child.id())?);
        for (kind, id) in [
            ("uid", unsafe { libc::getuid() }),
            ("gid", unsafe { libc::getgid() }),
        ] {
            docker::map_ids(
                kind,
                child.child.id(),
                id,
                docker::subordinate(kind, id)?,
                &scratch.as_ref().unwrap().path,
                cancel,
            )?;
        }
    }
    child
        .child
        .stdin
        .take()
        .ok_or("missing worker stdin")?
        .write_all(b"x")?;
    let status = loop {
        cancel.check()?;
        if let Some(status) = child.finished()? {
            break status;
        }
        std::thread::sleep(Duration::from_millis(20));
    };
    drop(child);
    drop(scratch);
    if !status.success() {
        return Err(format!(
            "cleanup failed for {} ({status}); data may be partially removed",
            path.display()
        )
        .into());
    }
    writeln!(context.output, "Removed {}", path.display())?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;
    #[test]
    fn mapping_error_reaps_the_helper_before_scratch_and_inherited_gate_cleanup() {
        if let Some(root) = std::env::var_os("GOBLINS_TEST_SCRATCH_PEER") {
            let _fork = unix::forking();
            let root = PathBuf::from(root);
            let target = root.join("target");
            unix::private_directory(&target).unwrap();
            fs::write(target.join("payload"), "retained").unwrap();
            let helper = root.join("helper");
            fs::write(
                &helper,
                "#!/bin/sh
echo $$ > helper.pid
printf 'ready\n'
exec sleep 30
",
            )
            .unwrap();
            fs::set_permissions(&helper, fs::Permissions::from_mode(0o700)).unwrap();
            let runtime = root.join("runtime.json");
            fs::write(
                &runtime,
                serde_json::json!({
                    "api":2,"helper_api":6,
                    "goblins":{"shell":{
                        "build_spec":"/unused","args":[],"env":{},"client_package":"/unused",
                        "helper":helper,"flake":"unused"
                    }}
                })
                .to_string(),
            )
            .unwrap();
            let gate = storage_gate().unwrap();
            let mut output = Vec::new();
            let error = File::create(root.join("stderr")).unwrap();
            let cancel = process::Cancellation::default().with_limit(Duration::from_secs(5));
            let mut context = Context {
                output: &mut output,
                error: &error,
                cancel: &cancel,
            };
            SCRATCH_REAPS.with(|count| count.set(0));
            // The fake deletion helper deliberately does not create a user
            // namespace. Mapping must fail, exercising the actual error path
            // after scratch construction, without changing host credentials.
            assert!(remove(Some(&runtime), &target, true, false, &[&gate], &mut context).is_err());
            let pid: libc::pid_t = fs::read_to_string(root.join("helper.pid"))
                .unwrap()
                .trim()
                .parse()
                .unwrap();
            assert_eq!(
                unsafe { libc::waitpid(pid, std::ptr::null_mut(), libc::WNOHANG) },
                -1
            );
            assert_eq!(
                std::io::Error::last_os_error().raw_os_error(),
                Some(libc::ECHILD)
            );
            // Require the mapping/scratch path was actually reached; an early
            // readiness failure cannot stand in for this ownership proof.
            assert_eq!(SCRATCH_REAPS.with(std::cell::Cell::get), 1);
            // Scratch::drop above independently observed ECHILD before removal.
            assert!(!fs::read_dir(lock_root().unwrap()).unwrap().any(|e| {
                e.unwrap()
                    .file_name()
                    .to_string_lossy()
                    .starts_with("cleanup-")
            }));
            assert!(
                flock(&lock_root().unwrap().join("storage.lock"), true)
                    .unwrap()
                    .is_none()
            );
            drop(gate);
            assert!(
                flock(&lock_root().unwrap().join("storage.lock"), true)
                    .unwrap()
                    .is_some()
            );
            assert_eq!(
                fs::read_to_string(target.join("payload")).unwrap(),
                "retained"
            );
            return;
        }
        let _fork = unix::forking();
        let root = unix::temp_directory().unwrap();
        let output = Command::new(std::env::current_exe().unwrap())
            .args(["--exact","cleanup::tests::mapping_error_reaps_the_helper_before_scratch_and_inherited_gate_cleanup","--nocapture"])
            .env("GOBLINS_TEST_SCRATCH_PEER",&root)
            .env("XDG_CACHE_HOME",root.join("cache"))
            .env("XDG_STATE_HOME",root.join("history"))
            .current_dir(&root).output().unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        fs::remove_dir_all(root).unwrap();
    }
}
