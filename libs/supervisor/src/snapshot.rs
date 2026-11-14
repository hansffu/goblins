//! Descriptor-relative snapshot: never follow source links, skip .git and
//! special files. Destination is private and not yet exposed to the payload.
use crate::Result;
use crate::process::Cancellation;
use goblins_unix as unix;
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::net::UnixListener;

    /// A source tree and an empty destination in a fresh temporary directory.
    fn tree() -> (std::path::PathBuf, std::path::PathBuf, std::path::PathBuf) {
        let root = fs::canonicalize(unix::temp_directory().unwrap()).unwrap();
        let (source, dest) = (root.join("source"), root.join("dest"));
        fs::create_dir(&source).unwrap();
        fs::create_dir(&dest).unwrap();
        (root, source, dest)
    }

    #[test]
    fn files_and_directories_are_copied_with_their_permission_bits() {
        let (root, source, dest) = tree();
        fs::write(source.join("script"), "#!/bin/sh\n").unwrap();
        fs::set_permissions(source.join("script"), fs::Permissions::from_mode(0o750)).unwrap();
        fs::write(source.join("private"), "mine").unwrap();
        fs::set_permissions(source.join("private"), fs::Permissions::from_mode(0o600)).unwrap();
        fs::create_dir_all(source.join("a/b")).unwrap();
        fs::write(source.join("a/b/deep"), "nested").unwrap();
        fs::set_permissions(source.join("a/b/deep"), fs::Permissions::from_mode(0o644)).unwrap();
        snapshot(&source, &dest, &Cancellation::default()).unwrap();
        let mode = |p: &str| fs::symlink_metadata(dest.join(p)).unwrap().mode() & 0o7777;
        assert_eq!(
            fs::read_to_string(dest.join("script")).unwrap(),
            "#!/bin/sh\n"
        );
        assert_eq!(mode("script"), 0o750);
        assert_eq!(fs::read_to_string(dest.join("private")).unwrap(), "mine");
        assert_eq!(mode("private"), 0o600);
        assert!(fs::symlink_metadata(dest.join("a/b")).unwrap().is_dir());
        assert_eq!(fs::read_to_string(dest.join("a/b/deep")).unwrap(), "nested");
        assert_eq!(mode("a/b/deep"), 0o644);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn links_are_copied_as_links_and_never_followed() {
        let (root, source, dest) = tree();
        let secret = root.join("secret");
        fs::create_dir(&secret).unwrap();
        fs::write(secret.join("key"), "SECRET-CONTENT").unwrap();
        symlink(&secret, source.join("to-directory")).unwrap();
        symlink(secret.join("key"), source.join("to-file")).unwrap();
        symlink("../secret/key", source.join("relative")).unwrap();
        symlink(root.join("missing"), source.join("dangling")).unwrap();
        snapshot(&source, &dest, &Cancellation::default()).unwrap();
        for (name, target) in [
            ("to-directory", secret.clone()),
            ("to-file", secret.join("key")),
            ("relative", "../secret/key".into()),
            ("dangling", root.join("missing")),
        ] {
            let copied = dest.join(name);
            assert!(
                fs::symlink_metadata(&copied).unwrap().is_symlink(),
                "{name}"
            );
            assert_eq!(fs::read_link(&copied).unwrap(), target, "{name}");
        }
        // Only the four links are there: nothing was read through them.
        let mut names: Vec<_> = fs::read_dir(&dest)
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        names.sort();
        assert_eq!(names, ["dangling", "relative", "to-directory", "to-file"]);
        // Removing the target shows the copy holds no content of its own.
        fs::remove_dir_all(&secret).unwrap();
        for name in ["to-directory", "to-file", "relative"] {
            assert!(fs::metadata(dest.join(name)).is_err(), "{name} is dangling");
        }
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn git_and_special_files_are_skipped() {
        let (root, source, dest) = tree();
        fs::create_dir_all(source.join(".git/objects")).unwrap();
        fs::write(source.join(".git/config"), "x").unwrap();
        fs::create_dir_all(source.join("module")).unwrap();
        // A worktree or submodule keeps a .git file, not a directory.
        fs::write(source.join("module/.git"), "gitdir: elsewhere").unwrap();
        fs::write(source.join("module/kept"), "kept").unwrap();
        fs::create_dir_all(source.join("nested/.git")).unwrap();
        let fifo = std::ffi::CString::new(source.join("fifo").as_os_str().as_bytes()).unwrap();
        assert_eq!(unsafe { libc::mkfifo(fifo.as_ptr(), 0o600) }, 0);
        let _socket = UnixListener::bind(source.join("socket")).unwrap();
        fs::write(source.join("file"), "kept").unwrap();
        snapshot(&source, &dest, &Cancellation::default()).unwrap();
        let mut copied = vec![];
        let mut queue = vec![dest.clone()];
        while let Some(directory) = queue.pop() {
            for entry in fs::read_dir(directory).unwrap() {
                let path = entry.unwrap().path();
                if fs::symlink_metadata(&path).unwrap().is_dir() {
                    queue.push(path.clone());
                }
                copied.push(path.strip_prefix(&dest).unwrap().to_path_buf());
            }
        }
        copied.sort();
        assert_eq!(
            copied,
            ["file", "module", "module/kept", "nested"].map(std::path::PathBuf::from)
        );
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn a_source_that_is_a_link_is_refused() {
        let (root, source, dest) = tree();
        fs::write(source.join("file"), "x").unwrap();
        let link = root.join("link");
        symlink(&source, &link).unwrap();
        let error = snapshot(&link, &dest, &Cancellation::default()).unwrap_err();
        // O_NOFOLLOW with O_DIRECTORY: the link itself is opened, and it is
        // not a directory. Followed, it would have opened the source.
        assert!(error.to_string().contains("Not a directory"), "{error}");
        assert_eq!(fs::read_dir(&dest).unwrap().count(), 0);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn cancellation_stops_the_copy() {
        let (root, source, dest) = tree();
        fs::create_dir(source.join("directory")).unwrap();
        fs::write(source.join("file"), "x").unwrap();
        let cancel = Cancellation::default();
        cancel.cancel();
        assert!(snapshot(&source, &dest, &cancel).is_err());
        assert_eq!(fs::read_dir(&dest).unwrap().count(), 0);
        fs::remove_dir_all(root).unwrap();
    }
}
