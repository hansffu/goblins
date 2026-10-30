//! Host-only mount injection into an already confined trust-group engine.
//! Inputs are pinned descriptors, never namespace IDs or sandbox RPC paths.
use super::*;
use std::os::unix::fs::MetadataExt;

pub fn run() -> io::Result<i32> {
    let args: Vec<_> = env::args().collect();
    if args.len() != 8 {
        return Err(io::Error::other("invalid Docker bind arguments"));
    }
    let mut handles = Vec::new();
    for arg in &args[2..6] {
        let n: i32 = arg.parse().map_err(io::Error::other)?;
        if n < 3 || handles.iter().any(|f: &OwnedFd| f.as_raw_fd() == n) {
            return Err(io::Error::other("invalid bind descriptor"));
        }
        handles.push(unsafe { OwnedFd::from_raw_fd(n) });
    }
    let [user, target, root, source] = handles.as_slice() else {
        unreachable!()
    };
    let destination = args[6]
        .strip_prefix('/')
        .ok_or_else(|| io::Error::other("absolute destination required"))?;
    if destination.is_empty()
        || destination
            .split('/')
            .any(|p| p.is_empty() || p == "." || p == "..")
        || destination.contains('\0')
    {
        return Err(io::Error::other("invalid destination"));
    }
    let readonly = match args[7].as_str() {
        "ro" => true,
        "rw" => false,
        _ => return Err(io::Error::other("invalid mode")),
    };
    let owner = fd(unsafe { libc::ioctl(target.as_raw_fd(), NS_GET_USERNS) })?;
    let parent = fd(unsafe { libc::ioctl(owner.as_raw_fd(), NS_GET_PARENT) })?;
    if inode(&parent)? != inode(user)? {
        return Err(io::Error::other("engine mount ancestry mismatch"));
    }
    cvt(unsafe { libc::setns(user.as_raw_fd(), libc::CLONE_NEWUSER) })?;
    cvt(unsafe { libc::unshare(libc::CLONE_NEWNS) })?;
    cvt(unsafe {
        libc::mount(
            std::ptr::null(),
            c("/").as_ptr(),
            std::ptr::null(),
            libc::MS_REC | libc::MS_PRIVATE,
            std::ptr::null(),
        )
    })?;
    // Mount operations require a source from this mount namespace. Resolve the
    // pinned object's current name, reopen it in our private clone, and verify
    // identity before using the new descriptor. A rename/replacement fails
    // closed; subsequent pathname changes cannot redirect the pinned mount.
    let source_name = fs::read_link(format!("/proc/self/fd/{}", source.as_raw_fd()))?;
    let source_local = open(
        source_name
            .to_str()
            .ok_or_else(|| io::Error::other("non-UTF8 source"))?,
        libc::O_PATH,
    )?;
    let original = File::from(source.try_clone()?).metadata()?;
    let reopened = File::from(source_local.try_clone()?).metadata()?;
    if (original.dev(), original.ino()) != (reopened.dev(), reopened.ino()) {
        return Err(io::Error::other("Docker bind source changed"));
    }
    let imported = fd(unsafe {
        libc::syscall(
            libc::SYS_open_tree,
            source_local.as_raw_fd(),
            c("").as_ptr(),
            OPEN_TREE_CLONE | libc::AT_EMPTY_PATH | libc::O_CLOEXEC,
        ) as i32
    })
    .map_err(|e| io::Error::other(format!("clone source: {e}")))?;
    cvt(unsafe {
        libc::mount(
            c("tmpfs").as_ptr(),
            c("/tmp").as_ptr(),
            c("tmpfs").as_ptr(),
            libc::MS_NOSUID | libc::MS_NODEV,
            std::ptr::null(),
        )
    })?;
    let directory = File::from(source.try_clone()?).metadata()?.is_dir();
    if directory {
        fs::create_dir("/tmp/source")?;
    } else {
        File::create("/tmp/source")?;
    }
    let landing = open("/tmp/source", libc::O_PATH)?;
    cvt(unsafe {
        libc::syscall(
            libc::SYS_move_mount,
            imported.as_raw_fd(),
            c("").as_ptr(),
            landing.as_raw_fd(),
            c("").as_ptr(),
            MOVE_MOUNT_F_EMPTY_PATH | MOVE_MOUNT_T_EMPTY_PATH,
        ) as i32
    })?;
    drop((imported, landing));
    let local = open("/tmp/source", libc::O_PATH)?;
    let attr = MountAttr {
        attr_set: MOUNT_ATTR_NOSUID
            | MOUNT_ATTR_NODEV
            | if readonly { MOUNT_ATTR_RDONLY } else { 0 },
        attr_clr: 0,
        propagation: 0,
        userns_fd: 0,
    };
    cvt(unsafe {
        libc::syscall(
            libc::SYS_mount_setattr,
            local.as_raw_fd(),
            c("").as_ptr(),
            libc::AT_EMPTY_PATH,
            &attr,
            size_of::<MountAttr>(),
        ) as i32
    })?;
    drop(local);
    // Cross the same privilege boundary as initial engine mounts BEFORE cloning
    // the grant. Merely setting RDONLY on a new mount in the engine would allow
    // a privileged container to remount it writable.
    cvt(unsafe { libc::setns(owner.as_raw_fd(), libc::CLONE_NEWUSER) })?;
    cvt(unsafe { libc::unshare(libc::CLONE_NEWNS) })?;
    let local = open("/tmp/source", libc::O_PATH)?;
    let tree = fd(unsafe {
        libc::syscall(
            libc::SYS_open_tree,
            local.as_raw_fd(),
            c("").as_ptr(),
            OPEN_TREE_CLONE | libc::AT_EMPTY_PATH | libc::O_CLOEXEC,
        ) as i32
    })?;
    cvt(unsafe { libc::setns(target.as_raw_fd(), libc::CLONE_NEWNS) })?;
    let mut parent = root.try_clone()?;
    let parts: Vec<_> = destination.split('/').collect();
    for (i, part) in parts.iter().enumerate() {
        if i + 1 != parts.len() || directory {
            let n = unsafe { libc::mkdirat(parent.as_raw_fd(), c(part).as_ptr(), 0o755) };
            if n < 0 && io::Error::last_os_error().raw_os_error() != Some(libc::EEXIST) {
                return Err(io::Error::last_os_error());
            }
        } else if beneath(parent.as_raw_fd(), part).is_err() {
            drop(fd(unsafe {
                libc::openat(
                    parent.as_raw_fd(),
                    c(part).as_ptr(),
                    libc::O_CREAT
                        | libc::O_EXCL
                        | libc::O_WRONLY
                        | libc::O_NOFOLLOW
                        | libc::O_CLOEXEC,
                    0o644,
                )
            })?);
        }
        parent = beneath(parent.as_raw_fd(), part)?;
    }
    cvt(unsafe {
        libc::syscall(
            libc::SYS_move_mount,
            tree.as_raw_fd(),
            c("").as_ptr(),
            parent.as_raw_fd(),
            c("").as_ptr(),
            MOVE_MOUNT_F_EMPTY_PATH | MOVE_MOUNT_T_EMPTY_PATH,
        ) as i32
    })?;
    Ok(0)
}
