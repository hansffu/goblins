//! Descriptor-relative cleanup shared with the mount helper. Mounts, including
//! same-device bind mounts, are never traversed; symlinks are only unlinked.
use std::{
    ffi::{CString, OsStr},
    fs::{self, File},
    io,
    os::{
        fd::{AsRawFd, FromRawFd, OwnedFd},
        unix::{ffi::OsStrExt, fs::MetadataExt},
    },
};
#[repr(C)]
struct OpenHow {
    flags: u64,
    mode: u64,
    resolve: u64,
}
fn cvt(value: i32) -> io::Result<i32> {
    if value < 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(value)
    }
}
fn fd(value: i32) -> io::Result<OwnedFd> {
    Ok(unsafe { OwnedFd::from_raw_fd(cvt(value)?) })
}
pub fn open_at(parent: &File, name: &OsStr) -> io::Result<File> {
    open_with(
        parent,
        name,
        libc::O_PATH | libc::O_NOFOLLOW | libc::O_CLOEXEC,
        0,
    )
}
pub fn open_lock_at(parent: &File, name: &OsStr) -> io::Result<File> {
    open_with(
        parent,
        name,
        libc::O_RDWR | libc::O_NOFOLLOW | libc::O_CLOEXEC | libc::O_CREAT | libc::O_NONBLOCK,
        0o600,
    )
}
fn open_with(parent: &File, name: &OsStr, flags: i32, mode: u64) -> io::Result<File> {
    if name.is_empty()
        || name.as_bytes() == b"."
        || name.as_bytes() == b".."
        || name.as_bytes().contains(&b'/')
    {
        return Err(io::Error::other("invalid cleanup entry name"));
    }
    let name = CString::new(name.as_bytes())?;
    // O_PATH pins even a mode-000 directory without following a symlink.
    // NO_XDEV also refuses bind mounts, including mounts on the same device.
    let how = OpenHow {
        flags: flags as u64,
        mode,
        resolve: 0x08 | 0x02 | 0x01,
    };
    let entry = File::from(fd(unsafe {
        libc::syscall(
            libc::SYS_openat2,
            parent.as_raw_fd(),
            name.as_ptr(),
            &how,
            size_of::<OpenHow>(),
        ) as i32
    })?);
    Ok(entry)
}
pub fn remove_at(parent: &File, name: &OsStr) -> io::Result<()> {
    let entry = open_at(parent, name)?;
    let name = CString::new(name.as_bytes())?;
    let meta = entry.metadata()?;
    let flags = if meta.is_dir() {
        let path = format!("/proc/self/fd/{}", entry.as_raw_fd());
        // This procfs link names our pinned directory, never a tree symlink.
        // File modes are not changed: hard links outside the tree stay intact.
        cvt(unsafe {
            libc::chmod(
                CString::new(path.as_bytes())?.as_ptr(),
                (meta.mode() | 0o700) & 0o7777,
            )
        })?;
        let directory = File::open(&path)?;
        for child in fs::read_dir(&path)? {
            remove_at(&directory, &child?.file_name())?;
        }
        libc::AT_REMOVEDIR
    } else {
        0
    };
    cvt(unsafe { libc::unlinkat(parent.as_raw_fd(), name.as_ptr(), flags) })?;
    Ok(())
}
