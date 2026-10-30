use crate::Result;
use serde::{Deserialize, Serialize};
use std::{
    fs::{self, File},
    os::{
        fd::{AsRawFd, OwnedFd},
        unix::fs::{MetadataExt, OpenOptionsExt},
    },
    path::{Path, PathBuf},
};

pub fn flock(path: &Path, exclusive: bool) -> Result<Option<File>> {
    let file = File::options()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(path)?;
    flock_file(file, exclusive, path)
}
/// Open a lifetime lock relative to a pinned directory, without crossing a
/// bind mount or following a symlink while opening the lock itself.
pub fn flock_at(parent: &File, name: &std::ffi::OsStr, exclusive: bool) -> Result<Option<File>> {
    flock_file(
        goblins_unix::cleanup::open_lock_at(parent, name)?,
        exclusive,
        Path::new(name),
    )
}
fn flock_file(file: File, exclusive: bool, path: &Path) -> Result<Option<File>> {
    let meta = file.metadata()?;
    if !meta.is_file() || meta.uid() != unsafe { libc::getuid() } {
        return Err(format!("unsafe cleanup lock {}", path.display()).into());
    }
    if unsafe {
        libc::flock(
            file.as_raw_fd(),
            (if exclusive {
                libc::LOCK_EX
            } else {
                libc::LOCK_SH
            }) | libc::LOCK_NB,
        )
    } == 0
    {
        Ok(Some(file))
    } else if std::io::Error::last_os_error().kind() == std::io::ErrorKind::WouldBlock {
        Ok(None)
    } else {
        Err(std::io::Error::last_os_error().into())
    }
}

pub fn resource_lease(directory: &Path) -> Result<File> {
    flock(&directory.join(".gc-lease"), false)?
        .ok_or_else(|| "session resources are being collected".into())
}
/// The exclusive lock on a resources directory, when no lease holds it.
pub fn resource_exclusive(directory: &Path) -> Result<Option<File>> {
    flock(&directory.join(".gc-lease"), true)
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Identity {
    pub pid: u32,
    pub helper_userns: u64,
    pub mount_owner: u64,
    pub source_mountns: u64,
    pub mountns: u64,
}
/// A shared lock on one goblin's resources directory, and which directory it
/// protects. The directory goes when the last lease on it is released: a
/// holder that lets go removes it only if it can then take the lock
/// exclusively, otherwise the last holder or `goblins gc` removes it later.
/// Every lease is its own open file description, never a `dup`, because the
/// exclusive-lock test cannot tell `dup`s apart.
#[derive(Debug)]
pub struct ResourceLease {
    directory: PathBuf,
    lock: Option<File>,
}
impl ResourceLease {
    pub fn new(directory: PathBuf) -> Result<Self> {
        let lock = Some(resource_lease(&directory)?);
        Ok(Self { directory, lock })
    }
    /// A fresh lease on the same directory, for another holder.
    pub fn share(&self) -> Result<Self> {
        Self::new(self.directory.clone())
    }
    pub fn fd(&self) -> std::os::fd::RawFd {
        self.lock.as_ref().map_or(-1, AsRawFd::as_raw_fd)
    }
    pub fn directory(&self) -> &Path {
        &self.directory
    }
    /// Move a fresh lease into a private descriptor-bearing message. The
    /// receiver assumes cleanup ownership with `from_parts`; closing an
    /// undelivered descriptor still releases the in-flight shared lock.
    pub fn into_parts(mut self) -> (PathBuf, OwnedFd) {
        let lock = self.lock.take().expect("resource lease has a lock");
        (std::mem::take(&mut self.directory), lock.into())
    }
    /// Adopt a transferred lease, checking that its metadata names the lock
    /// capability received in the same frame. Do not reopen the lease: its
    /// open file description already protects the directory during handoff.
    pub fn from_parts(directory: PathBuf, lock: OwnedFd) -> Result<Self> {
        if !directory.is_absolute() {
            return Err("resource lease directory must be absolute".into());
        }
        let lock = File::from(lock);
        let actual = lock.metadata()?;
        let expected = fs::symlink_metadata(directory.join(".gc-lease"))?;
        if !actual.is_file()
            || actual.uid() != unsafe { libc::getuid() }
            || !expected.is_file()
            || (actual.dev(), actual.ino()) != (expected.dev(), expected.ino())
        {
            return Err("resource lease descriptor does not match its directory".into());
        }
        Ok(Self {
            directory,
            lock: Some(lock),
        })
    }
}
impl Drop for ResourceLease {
    fn drop(&mut self) {
        // Closing our descriptor releases our lock; a copy another process
        // holds, such as the helper's, keeps its own.
        let Some(lock) = self.lock.take() else {
            return;
        };
        drop(lock);
        let Ok(Some(_exclusive)) = resource_exclusive(&self.directory) else {
            return;
        };
        let _ = fs::remove_dir_all(&self.directory);
        if self.directory.file_name().is_some_and(|n| n == "resources")
            && let Some(parent) = self.directory.parent()
        {
            let _ = fs::remove_dir(parent);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn transferred_fresh_lease_retains_resources_until_the_receiver_releases_it() {
        let _no_forks = goblins_unix::no_forks();
        let root = goblins_unix::temp_directory().unwrap();
        let resources = root.join("resources");
        fs::create_dir(&resources).unwrap();
        fs::write(resources.join("root"), "retained").unwrap();
        let owner = ResourceLease::new(resources.clone()).unwrap();
        let (directory, fd) = owner.share().unwrap().into_parts();
        // An independent shared OFD prevents upgrading the owner's lock. A
        // dup of the owner's descriptor would let this succeed instead.
        assert_ne!(
            unsafe { libc::flock(owner.fd(), libc::LOCK_EX | libc::LOCK_NB) },
            0
        );
        assert_eq!(
            std::io::Error::last_os_error().kind(),
            std::io::ErrorKind::WouldBlock
        );
        let receiver = ResourceLease::from_parts(directory, fd).unwrap();
        drop(owner);
        assert!(resources.join("root").exists());
        assert!(resource_exclusive(&resources).unwrap().is_none());
        drop(receiver);
        assert!(!resources.exists());
        assert!(!root.exists());
    }

    #[test]
    fn transferred_lease_rejects_a_different_directory_without_deleting_it() {
        let _no_forks = goblins_unix::no_forks();
        let root = goblins_unix::temp_directory().unwrap();
        let first = root.join("first");
        let second = root.join("second");
        fs::create_dir(&first).unwrap();
        fs::create_dir(&second).unwrap();
        let owner = ResourceLease::new(first.clone()).unwrap();
        let other = ResourceLease::new(second.clone()).unwrap();
        let (_, fd) = owner.share().unwrap().into_parts();
        assert!(ResourceLease::from_parts(second.clone(), fd).is_err());
        assert!(first.exists());
        assert!(second.exists());
        drop((owner, other));
        fs::remove_dir(root).unwrap();
    }
}
