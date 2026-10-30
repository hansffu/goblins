use crate::Result;
use serde::{Deserialize, Serialize};
use std::{
    fs::{self, File},
    os::{
        fd::AsRawFd,
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
    #[cfg(any(test, feature = "test-support"))]
    pub fn directory(&self) -> &Path {
        &self.directory
    }
}
impl Drop for ResourceLease {
    fn drop(&mut self) {
        // Closing our descriptor releases our lock; a copy another process
        // holds, such as the helper's, keeps its own.
        drop(self.lock.take());
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
