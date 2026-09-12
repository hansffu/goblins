//! Descriptor-relative snapshot: never follow source links, skip .git and
//! special files. Destination is private and not yet exposed to the payload.
use crate::{Result, process::Cancellation, unix};
use std::{
    ffi::CString,
    fs::{self, File},
    os::{
        fd::{AsRawFd, RawFd},
        unix::{
            ffi::OsStrExt,
            fs::{MetadataExt, PermissionsExt, symlink},
        },
    },
    path::Path,
};
pub fn snapshot(source: &Path, dest: &Path, cancel: &Cancellation) -> Result<()> {
    let name = CString::new(source.as_os_str().as_bytes())?;
    let fd = unix::owned(unsafe {
        libc::open(
            name.as_ptr(),
            libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
        )
    })?;
    copy_dir(fd.as_raw_fd(), dest, cancel)
}
fn copy_dir(source: RawFd, dest: &Path, cancel: &Cancellation) -> Result<()> {
    // /proc/self/fd enumerates the already-open directory; each lookup below is
    // relative to that same descriptor and rejects symlink substitution.
    for entry in fs::read_dir(format!("/proc/self/fd/{source}"))? {
        cancel.check()?;
        let entry = entry?;
        let name = entry.file_name();
        if name == ".git" {
            continue;
        }
        let c = CString::new(name.as_bytes())?;
        let target = dest.join(&name);
        let path = format!("/proc/self/fd/{source}/");
        let meta = fs::symlink_metadata(Path::new(&path).join(&name))?;
        if meta.is_symlink() {
            symlink(fs::read_link(Path::new(&path).join(name))?, target)?;
        } else if meta.is_dir() {
            let child = unix::owned(unsafe {
                libc::openat(
                    source,
                    c.as_ptr(),
                    libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
                )
            })?;
            fs::create_dir(&target)?;
            copy_dir(child.as_raw_fd(), &target, cancel)?;
        } else if meta.is_file() {
            let fd = unix::owned(unsafe {
                libc::openat(
                    source,
                    c.as_ptr(),
                    libc::O_RDONLY | libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_CLOEXEC,
                )
            })?;
            let mut input = File::from(fd);
            let current = input.metadata()?;
            if !current.is_file() {
                return Err("workspace file changed type during snapshot".into());
            }
            let mut output = File::create_new(&target)?;
            // Check cancellation between bounded copies, including large files.
            use std::io::{Read, Write};
            let mut buffer = [0; 65536];
            loop {
                cancel.check()?;
                let n = input.read(&mut buffer)?;
                if n == 0 {
                    break;
                }
                output.write_all(&buffer[..n])?;
            }
            fs::set_permissions(target, fs::Permissions::from_mode(current.mode() & 0o777))?;
        }
    }
    Ok(())
}
