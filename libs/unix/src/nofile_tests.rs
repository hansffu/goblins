use super::*;
use std::{os::unix::process::CommandExt, process::Command};

fn limits() -> (libc::rlim_t, libc::rlim_t) {
    let mut limit = libc::rlimit {
        rlim_cur: 0,
        rlim_max: 0,
    };
    assert_eq!(
        unsafe { libc::getrlimit(libc::RLIMIT_NOFILE, &mut limit) },
        0
    );
    (limit.rlim_cur, limit.rlim_max)
}

#[test]
fn inherited_payload_metadata_is_checked_without_mutating_kernel_limits() {
    const CHILD: &str = "GOBLINS_UNIX_NOFILE_CHILD";
    if let Ok(mode) = std::env::var(CHILD) {
        // This is a new executable image, with no inherited process-local atomics.
        assert_eq!(saved_payload_descriptor_limit(), None);
        let initial = if mode == "raised" { 128 } else { 512 };
        assert_eq!(limits(), (initial, 512));
        raise_descriptor_limit();
        assert_eq!(limits(), (512, 512));
        assert_eq!(
            saved_payload_descriptor_limit(),
            (initial == 128).then_some(128)
        );
        install_payload_descriptor_limit(Some(128)).unwrap();
        assert_eq!(saved_payload_descriptor_limit(), Some(128));
        let payload = payload_descriptor_limit().unwrap();
        assert_eq!((payload.rlim_cur, payload.rlim_max), (128, 512));
        for invalid in [0, 513, u64::MAX] {
            assert_eq!(
                install_payload_descriptor_limit(Some(invalid))
                    .unwrap_err()
                    .kind(),
                std::io::ErrorKind::InvalidInput
            );
            assert_eq!(saved_payload_descriptor_limit(), Some(128));
            assert_eq!(limits(), (512, 512));
        }
        // Check the current soft bound independently of the hard bound.
        let lower = libc::rlimit {
            rlim_cur: 256,
            rlim_max: 512,
        };
        assert_eq!(unsafe { libc::setrlimit(libc::RLIMIT_NOFILE, &lower) }, 0);
        assert!(install_payload_descriptor_limit(Some(257)).is_err());
        assert_eq!(saved_payload_descriptor_limit(), Some(128));
        assert_eq!(limits(), (256, 512));
        install_payload_descriptor_limit(None).unwrap();
        assert_eq!(saved_payload_descriptor_limit(), None);
        assert!(payload_descriptor_limit().is_none());
        assert_eq!(limits(), (256, 512));
        return;
    }
    let _forking = forking();
    let parent = limits();
    let metadata = saved_payload_descriptor_limit();
    assert!(parent.1 >= 512);
    for (mode, soft) in [("raised", 128), ("unchanged", 512)] {
        let mut command = Command::new(std::env::current_exe().unwrap());
        command
            .args([
                "--exact",
                "nofile_tests::inherited_payload_metadata_is_checked_without_mutating_kernel_limits",
            ])
            .env(CHILD, mode);
        unsafe {
            command.pre_exec(move || {
                let limit = libc::rlimit {
                    rlim_cur: soft,
                    rlim_max: 512,
                };
                if libc::setrlimit(libc::RLIMIT_NOFILE, &limit) == 0 {
                    Ok(())
                } else {
                    Err(std::io::Error::last_os_error())
                }
            });
        }
        assert!(command.status().unwrap().success());
        assert_eq!(limits(), parent);
        assert_eq!(saved_payload_descriptor_limit(), metadata);
    }
}
