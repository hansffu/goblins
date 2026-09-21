//! Same x86_64 payload filter as the PoC. AF_UNIX remains deliberately allowed;
//! pathname mounts and the private network namespace limit reachable sockets.
use crate::Result;
use std::{
    fs::File,
    io::{Seek, Write},
    os::fd::FromRawFd,
};
type Instruction = (u16, u8, u8, u32);

fn header() -> Result<Vec<Instruction>> {
    if std::env::consts::ARCH != "x86_64" {
        return Err("the syscall filter currently supports x86_64 only".into());
    }
    Ok(vec![
        (0x20, 0, 0, 4),
        (0x15, 1, 0, 0xC000003E),
        (0x06, 0, 0, 0x80000000),
        (0x20, 0, 0, 0),
        (0x35, 0, 1, 0x40000000),
        (0x06, 0, 0, 0x50001),
    ])
}

pub fn filter() -> Result<File> {
    let mut code = header()?;
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
    write_filter(code)
}

/// Keep the inherited cgroup namespace owned by the OUTER user namespace.
/// Otherwise privileged OCI containers can mount a fresh writable cgroup2
/// filesystem and reach host-user-delegated controls despite readonly binds.
/// Other namespace and mount operations remain available to the OCI runtime.
pub fn docker_filter() -> Result<File> {
    let mut code = header()?;
    code.extend([
        (0x15, 0, 1, libc::SYS_clone3 as u32),
        (0x06, 0, 0, 0x50026), // ENOSYS: fall back to inspectable clone flags.
    ]);
    for nr in [libc::SYS_clone, libc::SYS_unshare] {
        code.extend([
            (0x15, 0, 4, nr as u32),
            (0x20, 0, 0, 16), // low 32 bits of argument 0
            (0x45, 0, 1, libc::CLONE_NEWCGROUP as u32),
            (0x06, 0, 0, 0x50001), // EPERM
            (0x06, 0, 0, 0x7FFF0000),
        ]);
    }
    code.push((0x06, 0, 0, 0x7FFF0000));
    write_filter(code)
}

fn write_filter(code: Vec<Instruction>) -> Result<File> {
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Read;

    // Evaluate the small classic-BPF instruction subset used by our filters,
    // so architecture checks and flag branches have explicit regression tests.
    fn action(mut filter: File, arch: u32, nr: u32, flags: u32) -> u32 {
        let mut bytes = Vec::new();
        filter.read_to_end(&mut bytes).unwrap();
        let code: Vec<_> = bytes.chunks_exact(8).collect();
        let mut pc = 0;
        let mut accumulator = 0;
        for _ in 0..code.len() {
            let instruction = code[pc];
            let op = u16::from_ne_bytes(instruction[..2].try_into().unwrap());
            let k = u32::from_ne_bytes(instruction[4..].try_into().unwrap());
            let condition = match op {
                0x20 => {
                    accumulator = match k {
                        0 => nr,
                        4 => arch,
                        16 => flags,
                        _ => panic!("unexpected BPF load"),
                    };
                    None
                }
                0x15 => Some(accumulator == k),
                0x35 => Some(accumulator >= k),
                0x45 => Some(accumulator & k != 0),
                0x06 => return k,
                _ => panic!("unexpected BPF instruction"),
            };
            pc += 1 + condition.map_or(0, |yes| instruction[if yes { 2 } else { 3 }] as usize);
        }
        panic!("filter did not return")
    }

    #[test]
    fn docker_cannot_create_cgroup_namespaces_or_bypass_with_clone3() {
        let run = |nr, flags| action(docker_filter().unwrap(), 0xC000003E, nr, flags);
        for nr in [libc::SYS_clone, libc::SYS_unshare] {
            assert_eq!(run(nr as u32, 0), 0x7FFF0000);
            assert_eq!(run(nr as u32, libc::CLONE_NEWUSER as u32), 0x7FFF0000);
            assert_eq!(run(nr as u32, libc::CLONE_NEWCGROUP as u32), 0x50001);
            assert_eq!(
                run(
                    nr as u32,
                    (libc::CLONE_NEWCGROUP | libc::CLONE_NEWUSER) as u32
                ),
                0x50001
            );
        }
        assert_eq!(run(libc::SYS_clone3 as u32, 0), 0x50026);
        assert_eq!(run(libc::SYS_mount as u32, 0), 0x7FFF0000);
        assert_eq!(run(0x40000000 | libc::SYS_clone as u32, 0), 0x50001);
        assert_eq!(
            action(docker_filter().unwrap(), 0x40000003, 120, 0),
            0x80000000
        );
    }

    #[test]
    fn ordinary_payload_keeps_its_stricter_filter() {
        for nr in [libc::SYS_mount, libc::SYS_unshare, libc::SYS_setns] {
            assert_eq!(action(filter().unwrap(), 0xC000003E, nr as u32, 0), 0x50001);
        }
        assert_eq!(
            action(
                filter().unwrap(),
                0xC000003E,
                libc::SYS_socket as u32,
                libc::AF_UNIX as u32
            ),
            0x7FFF0000
        );
    }
}
