//! Trusted per-session helper. No listener, setuid bit, or host capabilities.
//! The private controller pipes never reach the payload. Only canonical store
//! basenames are accepted after startup; no mount flags, destinations, or PIDs.
use std::{
    env,
    ffi::CString,
    fs::{self, File},
    io::{self, BufRead, BufReader, Write},
    os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd},
    os::unix::{fs::MetadataExt, process::CommandExt},
    process::{Child, Command, Stdio},
    thread,
    time::Duration,
};

const NS_GET_USERNS: libc::c_ulong = 0xb701;
const NS_GET_PARENT: libc::c_ulong = 0xb702;
const CAP_SYS_ADMIN: u32 = 21;
const CAP_SYS_CHROOT: u32 = 18;

// Linux UAPI flags are named here to make the security-sensitive combinations
// reviewable alongside the namespace invariants below.
const RESOLVE_BENEATH: u64 = 0x08;
const RESOLVE_NO_SYMLINKS: u64 = 0x04;
const RESOLVE_NO_MAGICLINKS: u64 = 0x02;
const OPEN_TREE_CLONE: i32 = 1;
const MOUNT_ATTR_RDONLY: u64 = 1;
const MOUNT_ATTR_NOSUID: u64 = 2;
const MOUNT_ATTR_NODEV: u64 = 4;
const MOVE_MOUNT_F_EMPTY_PATH: i32 = 0x04;
const MOVE_MOUNT_T_EMPTY_PATH: i32 = 0x40;
const CLOSE_RANGE_CLOEXEC: u32 = 4;
const MAX_HELPER_COMMAND: u64 = 256;

fn cvt(n: i32) -> io::Result<i32> {
    if n < 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(n)
    }
}
fn fd(n: i32) -> io::Result<OwnedFd> {
    cvt(n).map(|n| unsafe { OwnedFd::from_raw_fd(n) })
}
fn c(s: &str) -> CString {
    CString::new(s).expect("internal NUL")
}
fn open(path: &str, flags: i32) -> io::Result<OwnedFd> {
    fd(unsafe { libc::open(c(path).as_ptr(), flags | libc::O_CLOEXEC) })
}
fn pipe() -> io::Result<(OwnedFd, OwnedFd)> {
    let mut f = [0; 2];
    cvt(unsafe { libc::pipe2(f.as_mut_ptr(), libc::O_CLOEXEC) })?;
    Ok(unsafe { (OwnedFd::from_raw_fd(f[0]), OwnedFd::from_raw_fd(f[1])) })
}
#[repr(C)]
struct OpenHow {
    flags: u64,
    mode: u64,
    resolve: u64,
}
fn beneath(root: RawFd, path: &str) -> io::Result<OwnedFd> {
    let how = OpenHow {
        flags: (libc::O_PATH | libc::O_CLOEXEC) as u64,
        mode: 0,
        resolve: RESOLVE_BENEATH | RESOLVE_NO_SYMLINKS | RESOLVE_NO_MAGICLINKS,
    }; // BENEATH | NO_SYMLINKS | NO_MAGICLINKS
    fd(unsafe {
        libc::syscall(
            libc::SYS_openat2,
            root,
            c(path).as_ptr(),
            &how,
            size_of::<OpenHow>(),
        ) as i32
    })
}
fn inode(f: &OwnedFd) -> io::Result<u64> {
    Ok(File::from(f.try_clone()?).metadata()?.ino())
}
fn valid_name(name: &str) -> bool {
    (34..=211).contains(&name.len())
        && name.as_bytes()[32] == b'-'
        && name.bytes().enumerate().all(|(i, b)| {
            if i < 32 {
                b"0123456789abcdfghijklmnpqrsvwxyz".contains(&b)
            } else {
                b.is_ascii_alphanumeric() || b"+-._?=".contains(&b)
            }
        })
}
#[repr(C)]
struct MountAttr {
    attr_set: u64,
    attr_clr: u64,
    propagation: u64,
    userns_fd: u64,
}
fn grant(
    store: RawFd,
    root: RawFd,
    source_ns: RawFd,
    target_ns: RawFd,
    name: &str,
) -> io::Result<()> {
    if !valid_name(name) {
        return Err(io::Error::other("invalid canonical store basename"));
    }
    cvt(unsafe { libc::setns(source_ns, libc::CLONE_NEWNS) })?;
    let src = beneath(store, name)?;
    let meta = File::from(src.try_clone()?).metadata()?;
    if !meta.is_dir() && !meta.is_file() {
        return Err(io::Error::other("unsupported store object"));
    }
    let dst = beneath(root, &format!("nix/store/{name}"))?;
    // Harden a detached clone BEFORE publishing it. No recursive submount import.
    let tree = fd(unsafe {
        libc::syscall(
            libc::SYS_open_tree,
            src.as_raw_fd(),
            c("").as_ptr(),
            OPEN_TREE_CLONE | libc::O_CLOEXEC | libc::AT_EMPTY_PATH,
        ) as i32
    })
    .map_err(|e| io::Error::other(format!("open_tree: {e}")))?;
    let attr = MountAttr {
        attr_set: MOUNT_ATTR_RDONLY | MOUNT_ATTR_NOSUID | MOUNT_ATTR_NODEV,
        attr_clr: 0,
        propagation: 0,
        userns_fd: 0,
    };
    cvt(unsafe {
        libc::syscall(
            libc::SYS_mount_setattr,
            tree.as_raw_fd(),
            c("").as_ptr(),
            libc::AT_EMPTY_PATH,
            &attr,
            size_of::<MountAttr>(),
        ) as i32
    })
    .map_err(|e| io::Error::other(format!("mount_setattr: {e}")))?;
    cvt(unsafe { libc::setns(target_ns, libc::CLONE_NEWNS) })?;
    cvt(unsafe {
        libc::syscall(
            libc::SYS_move_mount,
            tree.as_raw_fd(),
            c("").as_ptr(),
            dst.as_raw_fd(),
            c("").as_ptr(),
            MOVE_MOUNT_F_EMPTY_PATH | MOVE_MOUNT_T_EMPTY_PATH,
        ) as i32
    })
    .map_err(|e| io::Error::other(format!("move_mount: {e}")))?;
    Ok(())
}
struct Session(Child);
impl Drop for Session {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}
fn run() -> io::Result<i32> {
    // helper PAYLOAD_STDIN PAYLOAD_STDOUT SECCOMP_FD BIND_FDS BWRAP [arguments...]
    let mut args: Vec<String> = env::args().collect();
    let network = if args.get(1).is_some_and(|arg| arg == "--network-namespaces") {
        if args.len() < 5 {
            return Err(io::Error::other("missing network namespace descriptors"));
        }
        let user: RawFd = args[2].parse().map_err(io::Error::other)?;
        let net: RawFd = args[3].parse().map_err(io::Error::other)?;
        if user < 3 || net < 3 || user == net {
            return Err(io::Error::other("invalid network namespace descriptors"));
        }
        args.drain(1..4);
        Some((unsafe { OwnedFd::from_raw_fd(user) }, unsafe {
            OwnedFd::from_raw_fd(net)
        }))
    } else {
        None
    };
    if args.len() < 7 {
        return Err(io::Error::other("expected host launcher arguments"));
    }
    let parse = |i: usize| args[i].parse::<RawFd>().map_err(io::Error::other);
    let input = parse(1)?;
    let output = parse(2)?;
    let seccomp = parse(3)?;
    // This explicit launcher field avoids interpreting arbitrary bwrap argument
    // values (environment, command arguments, etc.) as descriptor authority.
    let mut sources = Vec::new();
    for value in args[4].split(',').filter(|s| !s.is_empty()) {
        let source: RawFd = value.parse().map_err(io::Error::other)?;
        if source < 3
            || [input, output, seccomp].contains(&source)
            || sources.iter().any(|fd: &OwnedFd| fd.as_raw_fd() == source)
        {
            return Err(io::Error::other("invalid startup source descriptor"));
        }
        cvt(unsafe { libc::fcntl(source, libc::F_GETFD) })?;
        sources.push(unsafe { OwnedFd::from_raw_fd(source) });
    }
    let parent = unsafe { libc::getppid() };
    cvt(unsafe { libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL) })?;
    if unsafe { libc::getppid() } != parent {
        return Err(io::Error::other("controller exited"));
    }
    if unsafe { libc::getuid() } == 0 {
        return Err(io::Error::other("refusing host uid 0"));
    }
    if let Some((user, net)) = network {
        let owner = fd(unsafe { libc::ioctl(net.as_raw_fd(), NS_GET_USERNS) })?;
        if inode(&owner)? != inode(&user)? {
            return Err(io::Error::other("unexpected network namespace owner"));
        }
        cvt(unsafe { libc::setns(user.as_raw_fd(), libc::CLONE_NEWUSER) })?;
        cvt(unsafe { libc::setns(net.as_raw_fd(), libc::CLONE_NEWNET) })?;
    }
    let uid = unsafe { libc::getuid() };
    let gid = unsafe { libc::getgid() };
    cvt(unsafe { libc::unshare(libc::CLONE_NEWUSER) })?;
    fs::write("/proc/self/setgroups", "deny")?;
    fs::write("/proc/self/uid_map", format!("0 {uid} 1\n"))?;
    fs::write("/proc/self/gid_map", format!("0 {gid} 1\n"))?;
    cvt(unsafe { libc::prctl(libc::PR_SET_DUMPABLE, 0) })?;
    // The source mount namespace is also session-owned, never the host's.
    // open_tree requires the source mount to belong to the current namespace.
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
    let source_ns = open("/proc/self/ns/mnt", libc::O_RDONLY)?;
    let own = open("/proc/self/ns/user", libc::O_RDONLY)?;
    let source_owner = fd(unsafe { libc::ioctl(source_ns.as_raw_fd(), NS_GET_USERNS) })?;
    if inode(&source_owner)? != inode(&own)? {
        return Err(io::Error::other("unexpected source mount namespace owner"));
    }
    let store = open("/nix/store", libc::O_PATH | libc::O_DIRECTORY)?;
    let (info_r, info_w) = pipe()?;
    let (block_r, block_w) = pipe()?;
    let mut cmd = Command::new(&args[5]);
    cmd.args([
        "--info-fd",
        &info_w.as_raw_fd().to_string(),
        "--block-fd",
        &block_r.as_raw_fd().to_string(),
    ]);
    cmd.args(&args[6..]);
    // Duplicated stdio is unrelated to the controller's stdin/stdout.
    cmd.stdin(Stdio::from(unsafe { OwnedFd::from_raw_fd(input) }));
    let payload_output = unsafe { OwnedFd::from_raw_fd(output) };
    cmd.stdout(Stdio::from(payload_output.try_clone()?));
    cmd.stderr(Stdio::from(payload_output));
    let mut keep = vec![info_w.as_raw_fd(), block_r.as_raw_fd(), seccomp];
    keep.extend(sources.iter().map(AsRawFd::as_raw_fd));
    unsafe {
        cmd.pre_exec(move || {
            // A shell attachment supplies a fresh PTY slave, never the approval
            // terminal. Make it this payload's controlling terminal before
            // Bubblewrap builds the sandbox. Pipe-based gate tests skip this.
            if libc::isatty(0) == 1 {
                cvt(libc::setsid())?;
                cvt(libc::ioctl(0, libc::TIOCSCTTY, 0))?;
            }
            // Mark all other descriptors close-on-exec, including controller pipes.
            cvt(libc::syscall(libc::SYS_close_range, 3u32, u32::MAX, CLOSE_RANGE_CLOEXEC) as i32)?;
            for &f in &keep {
                cvt(libc::fcntl(f, libc::F_SETFD, 0))?;
            }
            Ok(())
        });
    }
    let mut session = Session(cmd.spawn()?);
    drop(cmd);
    // Bubblewrap consumes and closes bind-fd handles before payload execution.
    // The supervising helper must not retain them either.
    drop(sources);
    drop(info_w);
    drop(block_r);
    // The original inherited seccomp fd belongs solely to this helper now.
    drop(unsafe { OwnedFd::from_raw_fd(seccomp) });
    let mut pid = None;
    for line in BufReader::new(File::from(info_r)).lines() {
        let line = line?;
        if let Some(rest) = line.trim().strip_prefix("\"child-pid\":") {
            pid = rest.trim().trim_end_matches(',').parse::<u32>().ok();
        }
    }
    let pid = pid.ok_or_else(|| io::Error::other("bubblewrap startup failed"))?;
    // info-fd precedes pivot_root. block-fd holds the payload. A trusted mount
    // under the final root identifies completion of the pivot; no agent runs.
    let mut root = None;
    for _ in 0..1000 {
        if let Ok(candidate) = open(
            &format!("/proc/{pid}/root"),
            libc::O_PATH | libc::O_DIRECTORY,
        ) && beneath(candidate.as_raw_fd(), "run/goblins/packages").is_ok()
        {
            root = Some(candidate);
            break;
        }
        if session.0.try_wait()?.is_some() {
            break;
        }
        thread::sleep(Duration::from_millis(10));
    }
    let root = root.ok_or_else(|| io::Error::other("sandbox root readiness timed out"))?;
    let ns = open(&format!("/proc/{pid}/ns/mnt"), libc::O_RDONLY)?;
    let owner = fd(unsafe { libc::ioctl(ns.as_raw_fd(), NS_GET_USERNS) })?; // NS_GET_USERNS
    let ancestor = fd(unsafe { libc::ioctl(owner.as_raw_fd(), NS_GET_PARENT) })?; // NS_GET_PARENT
    if inode(&ancestor)? != inode(&own)? {
        return Err(io::Error::other("unexpected mount namespace owner"));
    }
    cvt(unsafe { libc::setns(ns.as_raw_fd(), libc::CLONE_NEWNS) })?;
    // Keep only the two capabilities required by setns/mount in A and B.
    // These confer no authority in the host user namespace.
    for cap in 0..=40 {
        if cap != CAP_SYS_ADMIN && cap != CAP_SYS_CHROOT {
            cvt(unsafe { libc::prctl(libc::PR_CAPBSET_DROP, cap) })?;
        }
    }
    #[repr(C)]
    struct CapHeader {
        version: u32,
        pid: i32,
    }
    #[repr(C)]
    struct CapData {
        effective: u32,
        permitted: u32,
        inheritable: u32,
    }
    let bits = (1u32 << CAP_SYS_ADMIN) | (1u32 << CAP_SYS_CHROOT);
    let hdr = CapHeader {
        version: 0x20080522,
        pid: 0,
    };
    let caps = [
        CapData {
            effective: bits,
            permitted: bits,
            inheritable: 0,
        },
        CapData {
            effective: 0,
            permitted: 0,
            inheritable: 0,
        },
    ];
    cvt(unsafe { libc::syscall(libc::SYS_capset, &hdr, caps.as_ptr()) as i32 })?;
    writeln!(File::from(block_w), "x")?;
    println!(
        "READY2 {pid} {} {} {} {}",
        inode(&own)?,
        inode(&owner)?,
        inode(&source_ns)?,
        inode(&ns)?
    );
    io::stdout().flush()?;
    // Bound the PRIVATE protocol too; errors terminate the disposable session.
    let mut input = io::stdin().lock();
    loop {
        if let Some(status) = session.0.try_wait()? {
            use std::os::unix::process::ExitStatusExt;
            return Ok(status.code().unwrap_or(128 + status.signal().unwrap_or(0)));
        }
        let mut ready = libc::pollfd {
            fd: 0,
            events: libc::POLLIN,
            revents: 0,
        };
        if cvt(unsafe { libc::poll(&mut ready, 1, 20) })? == 0 {
            continue;
        }
        let mut bytes = Vec::new();
        use std::io::Read;
        let count = input
            .by_ref()
            .take(MAX_HELPER_COMMAND)
            .read_until(b'\n', &mut bytes)?;
        if count == 0 {
            break;
        }
        if bytes.last() != Some(&b'\n') {
            return Err(io::Error::other("oversized helper command"));
        }
        bytes.pop();
        let name = std::str::from_utf8(&bytes).map_err(io::Error::other)?;
        if name == "STOP" {
            break;
        }
        grant(
            store.as_raw_fd(),
            root.as_raw_fd(),
            source_ns.as_raw_fd(),
            ns.as_raw_fd(),
            name,
        )?;
        println!("OK");
        io::stdout().flush()?;
    }
    Ok(0)
}
fn main() {
    std::process::exit(match run() {
        Ok(code) => code,
        Err(error) => {
            eprintln!("goblins helper: {error}");
            1
        }
    });
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn names_are_single_store_objects() {
        assert!(valid_name("00000000000000000000000000000000-jq-1.8.2"));
        for name in [
            "/nix/store/x",
            "../x",
            "00000000000000000000000000000000-x/y",
            "00000000000000000000000000000000-x\nSTOP",
        ] {
            assert!(!valid_name(name));
        }
    }
}
