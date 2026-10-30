//! Descriptor-relative deletion. Only the controller selects the parent/name;
//! a mapped worker can remove Docker's subordinate-owned files without sudo.
use super::{cvt, fd};
use std::{
    env,
    ffi::{CString, OsStr},
    fs::{self, File},
    io::{self, Read, Write},
    os::{
        fd::AsRawFd,
        unix::{ffi::OsStrExt, fs::MetadataExt},
    },
};

fn remove(parent: &File, name: &OsStr) -> io::Result<()> {
    let name = CString::new(name.as_bytes())?;
    // O_PATH pins even a mode-000 directory without following a symlink.
    // NO_XDEV also refuses bind mounts, including mounts on the same device.
    let how = super::OpenHow {
        flags: (libc::O_PATH | libc::O_NOFOLLOW | libc::O_CLOEXEC) as u64,
        mode: 0,
        resolve: super::RESOLVE_BENEATH | super::RESOLVE_NO_MAGICLINKS | 0x01,
    };
    let entry = File::from(fd(unsafe {
        libc::syscall(
            libc::SYS_openat2,
            parent.as_raw_fd(),
            name.as_ptr(),
            &how,
            size_of::<super::OpenHow>(),
        ) as i32
    })?);
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
            remove(&directory, &child?.file_name())?;
        }
        libc::AT_REMOVEDIR
    } else {
        0
    };
    cvt(unsafe { libc::unlinkat(parent.as_raw_fd(), name.as_ptr(), flags) })?;
    Ok(())
}

pub fn run() -> io::Result<i32> {
    let args: Vec<_> = env::args_os().collect();
    if args.len() != 5 || !matches!(args[4].to_str(), Some("mapped" | "host")) {
        return Err(io::Error::other("invalid storage cleanup arguments"));
    }
    let number: i32 = args[2]
        .to_str()
        .ok_or_else(|| io::Error::other("invalid descriptor"))?
        .parse()
        .map_err(io::Error::other)?;
    let name = args[3].as_bytes();
    if number < 3 || name.is_empty() || name == b"." || name == b".." || name.contains(&b'/') {
        return Err(io::Error::other("invalid storage cleanup target"));
    }
    let parent = File::from(unsafe { std::os::fd::OwnedFd::from_raw_fd(number) });
    if !parent.metadata()?.is_dir() {
        return Err(io::Error::other("storage parent is not a directory"));
    }
    if args[4] == "mapped" {
        cvt(unsafe { libc::unshare(libc::CLONE_NEWUSER) })?;
    }
    println!("ready");
    io::stdout().flush()?;
    io::stdin().read_exact(&mut [0])?;
    if args[4] == "mapped" {
        cvt(unsafe { libc::setresgid(0, 0, 0) })?;
        cvt(unsafe { libc::setresuid(0, 0, 0) })?;
    }
    remove(&parent, &args[3])?;
    Ok(0)
}

use std::os::fd::FromRawFd;

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::{PermissionsExt, symlink};

    #[test]
    fn deletion_preserves_link_targets_and_handles_restricted_directories() {
        let mut template = c"/tmp/goblins-cleanup-test-XXXXXX"
            .to_bytes_with_nul()
            .to_vec();
        assert!(!unsafe { libc::mkdtemp(template.as_mut_ptr().cast()) }.is_null());
        template.pop();
        let root = std::path::PathBuf::from(std::ffi::OsString::from_vec(template));
        fs::create_dir(root.join("selected")).unwrap();
        fs::create_dir(root.join("outside")).unwrap();
        let outside = root.join("outside/payload");
        fs::write(&outside, "host-side effect").unwrap();
        let mode = fs::metadata(&outside).unwrap().mode();
        symlink(root.join("outside"), root.join("selected/link")).unwrap();
        fs::hard_link(&outside, root.join("selected/hardlink")).unwrap();
        fs::create_dir(root.join("selected/restricted")).unwrap();
        fs::write(root.join("selected/restricted/file"), "x").unwrap();
        fs::set_permissions(
            root.join("selected/restricted"),
            fs::Permissions::from_mode(0),
        )
        .unwrap();
        remove(&File::open(&root).unwrap(), OsStr::new("selected")).unwrap();
        assert!(!root.join("selected").exists());
        assert_eq!(fs::read_to_string(&outside).unwrap(), "host-side effect");
        assert_eq!(fs::metadata(&outside).unwrap().mode(), mode);
        symlink(root.join("outside"), root.join("root-link")).unwrap();
        remove(&File::open(&root).unwrap(), OsStr::new("root-link")).unwrap();
        assert!(!root.join("root-link").is_symlink());
        assert!(outside.exists());
        fs::remove_dir_all(root).unwrap();
    }
}

#[cfg(test)]
use std::os::unix::ffi::OsStringExt;
