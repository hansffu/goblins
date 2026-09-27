//! Managed scope storage under `$XDG_CACHE_HOME/goblins/scopes`. Members see
//! only the named storage directories they mount, never this host tree.
use crate::{Result, config::ScopeDefinition, unix};
use std::{
    collections::{BTreeMap, BTreeSet},
    ffi::{CString, OsString},
    fs::{self, File},
    os::unix::{
        ffi::{OsStrExt, OsStringExt},
        fs::{MetadataExt, OpenOptionsExt},
    },
    path::{Path, PathBuf},
    sync::{Mutex, OnceLock},
};

/// Persistent scopes whose storage lock this controller holds, including
/// instances that are still stopping.
static HELD: OnceLock<Mutex<BTreeSet<String>>> = OnceLock::new();
fn held() -> std::sync::MutexGuard<'static, BTreeSet<String>> {
    HELD.get_or_init(Mutex::default)
        .lock()
        .unwrap_or_else(|e| e.into_inner())
}

pub fn root(create: bool) -> Result<PathBuf> {
    unix::cache_directory("scopes", create)
}

pub struct Storage {
    name: String,
    path: PathBuf,
    temporary: bool,
    directories: BTreeMap<String, File>,
    lock: Option<File>,
}
impl Storage {
    /// `None` while a stopping instance in this controller still holds the
    /// persistent scope's storage; an error if another controller holds it.
    pub fn open(name: &str, definition: &ScopeDefinition) -> Result<Option<Self>> {
        if !goblins_protocol::scope_name(name) {
            return Err("invalid scope name".into());
        }
        let root = root(true)?;
        let (path, lock) = if definition.persistent {
            let locks = root.join("locks");
            unix::private_directory(&locks)?;
            let mut held = held();
            if held.contains(name) {
                return Ok(None);
            }
            let lock = unix::lock(&locks.join(name))
                .map_err(|_| format!("scope '{name}' is in use by another controller"))?;
            held.insert(name.to_string());
            let path = root.join(format!("scope-{name}"));
            unix::private_directory(&path)?;
            (path, Some(lock))
        } else {
            let mut template = CString::new(root.join("temporary-XXXXXX").as_os_str().as_bytes())?
                .into_bytes_with_nul();
            if unsafe { libc::mkdtemp(template.as_mut_ptr().cast()) }.is_null() {
                return Err(std::io::Error::last_os_error().into());
            }
            template.pop();
            (PathBuf::from(OsString::from_vec(template)), None)
        };
        let mut storage = Self {
            name: name.to_string(),
            path,
            temporary: !definition.persistent,
            directories: BTreeMap::new(),
            lock,
        };
        for entry in &definition.storage {
            if !goblins_protocol::scope_name(entry) {
                return Err("invalid scope storage name".into());
            }
            let directory = storage.path.join(entry);
            match fs::create_dir(&directory) {
                Ok(()) => {}
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
                Err(e) => return Err(e.into()),
            }
            // Members may change a storage directory's mode, but it must stay
            // a real directory owned by this user.
            let meta = fs::symlink_metadata(&directory)?;
            if !meta.is_dir() || meta.uid() != unsafe { libc::getuid() } {
                return Err(
                    format!("unsafe scope storage directory {}", directory.display()).into(),
                );
            }
            let file = File::options()
                .read(true)
                .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW)
                .open(&directory)?;
            storage.directories.insert(entry.clone(), file);
        }
        Ok(Some(storage))
    }
    pub fn directory(&self, name: &str) -> Result<&File> {
        self.directories
            .get(name)
            .ok_or_else(|| format!("scope has no storage named '{name}'").into())
    }
}
impl Drop for Storage {
    fn drop(&mut self) {
        if !self.temporary {
            // Release the lock before another instance may take it.
            self.lock.take();
            held().remove(&self.name);
            return;
        }
        self.directories.clear();
        if let Err(error) = remove_tree(&self.path) {
            eprintln!(
                "scope storage cleanup incomplete at {}: {error}",
                self.path.display()
            );
        }
    }
}

/// Members own their files and may have removed write or search permission.
/// Restore owner access on directories before deleting, without following
/// symbolic links out of the tree.
fn remove_tree(path: &Path) -> std::io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let meta = fs::symlink_metadata(path)?;
    if meta.is_dir() {
        if meta.mode() & 0o700 != 0o700 {
            fs::set_permissions(path, fs::Permissions::from_mode(meta.mode() | 0o700))?;
        }
        for entry in fs::read_dir(path)? {
            remove_tree(&entry?.path())?;
        }
        fs::remove_dir(path)
    } else {
        fs::remove_file(path)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn temporary_storage_is_removed_even_without_owner_permissions() {
        let temp = std::env::temp_dir().join(format!("goblins-storage-{}", std::process::id()));
        fs::create_dir_all(temp.join("a/b")).unwrap();
        fs::write(temp.join("a/b/file"), "x").unwrap();
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(temp.join("a"), fs::Permissions::from_mode(0o000)).unwrap();
        remove_tree(&temp).unwrap();
        assert!(!temp.exists());
    }
}
