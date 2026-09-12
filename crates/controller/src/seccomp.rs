//! Same x86_64 payload filter as the PoC. AF_UNIX remains deliberately allowed;
//! pathname mounts and the private network namespace limit reachable sockets.
use crate::Result;
use std::{
    fs::File,
    io::{Seek, Write},
    os::fd::FromRawFd,
};
pub fn filter() -> Result<File> {
    if std::env::consts::ARCH != "x86_64" {
        return Err("the syscall filter currently supports x86_64 only".into());
    }
    let mut code: Vec<(u16, u8, u8, u32)> = vec![
        (0x20, 0, 0, 4),
        (0x15, 1, 0, 0xC000003E),
        (0x06, 0, 0, 0x80000000),
        (0x20, 0, 0, 0),
        (0x35, 0, 1, 0x40000000),
        (0x06, 0, 0, 0x50001),
    ];
    for nr in [
        165, 166, 155, 272, 308, 428, 429, 430, 431, 432, 442, 101, 304,
    ] {
        code.extend([(0x15, 0, 1, nr), (0x06, 0, 0, 0x50001)]);
    }
    // clone3 -> ENOSYS lets libc use clone, whose namespace flags we can check.
    code.extend([
        (0x15, 0, 1, 435),
        (0x06, 0, 0, 0x50026),
        (0x15, 0, 4, 56),
        (0x20, 0, 0, 16),
        (0x45, 0, 1, 0x7E020000),
        (0x06, 0, 0, 0x50001),
        (0x06, 0, 0, 0x7FFF0000),
        (0x06, 0, 0, 0x7FFF0000),
    ]);
    let raw = crate::unix::cvt(unsafe {
        libc::memfd_create(c"goblins-seccomp".as_ptr(), libc::MFD_CLOEXEC)
    })?;
    let mut file = unsafe { File::from_raw_fd(raw) };
    for (op, jt, jf, k) in code {
        file.write_all(&op.to_ne_bytes())?;
        file.write_all(&[jt, jf])?;
        file.write_all(&k.to_ne_bytes())?;
    }
    file.rewind()?;
    Ok(file)
}
