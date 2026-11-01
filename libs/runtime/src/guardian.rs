//! Private scope-to-Docker-guardian resource retention. The guardian cannot
//! resolve host resource paths inside its confined filesystem; it holds the
//! received fresh lock capabilities through cleanup and leaves collection to
//! the scope's ResourceLeases or GC after those descriptors close.
use crate::{
    ResourceLease, Result,
    channel::{Channel, Descriptor},
};
use serde::{Deserialize, Serialize};
use std::{
    fs::File,
    os::{
        fd::{AsRawFd, OwnedFd},
        unix::net::UnixStream,
    },
    path::PathBuf,
};

/// Keep the endpoint raw across fork. A Channel destructor calls shutdown on
/// the socket endpoint shared by forked copies; a raw UnixStream drop only
/// closes that process's descriptor. Only the guardian parent enters Channel.
pub struct Bootstrap {
    stream: Option<UnixStream>,
    leases: Vec<OwnedFd>,
}
impl Bootstrap {
    /// Initial fresh locks are already held from exec, before readiness and
    /// before any channel acknowledgement. Never release these on early EOF.
    pub fn with_locks(stream: UnixStream, leases: Vec<OwnedFd>) -> Result<Self> {
        if leases.is_empty() {
            return Err("missing bootstrap lease descriptors".into());
        }
        for fd in &leases {
            if !File::from(fd.try_clone()?).metadata()?.is_file() {
                return Err("invalid bootstrap lease descriptor".into());
            }
            goblins_unix::cvt(unsafe {
                libc::fcntl(fd.as_raw_fd(), libc::F_SETFD, libc::FD_CLOEXEC)
            })?;
        }
        goblins_unix::cvt(unsafe {
            libc::fcntl(stream.as_raw_fd(), libc::F_SETFD, libc::FD_CLOEXEC)
        })?;
        Ok(Self {
            stream: Some(stream),
            leases,
        })
    }
    pub fn enter(&mut self) -> Result<Retainer> {
        let channel = Channel::new(
            self.stream
                .take()
                .ok_or("guardian channel already entered")?,
        )?;
        Ok(Retainer {
            channel,
            leases: std::mem::take(&mut self.leases),
        })
    }
    /// Own every initial lock across ALL post-fork errors. Failed Channel
    /// construction leaves the locks in Bootstrap; successful construction
    /// moves them into Retainer. Neither owner drops before cleanup returns.
    pub fn supervise(
        mut self,
        ready: impl FnOnce() -> std::io::Result<()>,
        running: impl FnOnce(&mut Retainer) -> std::io::Result<i32>,
        cleanup: impl FnOnce() -> std::io::Result<()>,
    ) -> std::io::Result<i32> {
        let mut guardian = None;
        let result = (|| {
            guardian = Some(
                self.enter()
                    .map_err(|e| std::io::Error::other(e.to_string()))?,
            );
            ready()?;
            running(guardian.as_mut().unwrap())
        })();
        cleanup()?;
        result
    }
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Lease {
    pub directory: PathBuf,
    pub lock: Descriptor,
}
#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "method", content = "params", deny_unknown_fields)]
enum Message {
    Retain { leases: Vec<Lease> },
    Retained {},
}
fn encode(message: &Message) -> serde_json::Value {
    let mut value = serde_json::to_value(message).unwrap();
    value
        .as_object_mut()
        .unwrap()
        .insert("jsonrpc".into(), "2.0".into());
    value
}
fn decode(mut value: serde_json::Value) -> Result<Message> {
    if value.as_object_mut().and_then(|v| v.remove("jsonrpc")) != Some(serde_json::json!("2.0")) {
        return Err("invalid guardian envelope".into());
    }
    Ok(serde_json::from_value(value)?)
}
pub fn queue_retain(channel: &mut Channel, leases: &[ResourceLease]) -> Result<()> {
    let mut locks = vec![];
    for lease in leases {
        locks.push(lease.share()?.into_parts());
    }
    queue_locks(channel, locks)
}
/// Also used once for the scope runtime's shared lock, whose drop must never
/// acquire ResourceLease cleanup semantics while non-Docker members remain.
pub fn queue_locks(channel: &mut Channel, locks: Vec<(PathBuf, OwnedFd)>) -> Result<()> {
    let mut fds = vec![];
    let mut records = vec![];
    for (directory, lock) in locks {
        records.push(Lease {
            directory,
            lock: Descriptor::new(fds.len()),
        });
        fds.push(lock);
    }
    channel.queue(&encode(&Message::Retain { leases: records }), &mut fds)?;
    Ok(())
}
pub fn retained(value: serde_json::Value) -> Result<()> {
    if !matches!(decode(value)?, Message::Retained {}) {
        return Err("invalid guardian acknowledgement".into());
    }
    Ok(())
}
pub struct Retainer {
    pub channel: Channel,
    // Never drain these on EOF. supervise() owns Retainer until disk cleanup
    // has completed, even if parsing or acknowledgement fails.
    leases: Vec<OwnedFd>,
}
impl Retainer {
    pub fn new(channel: Channel) -> Self {
        Self {
            channel,
            leases: vec![],
        }
    }
    pub fn receive(&mut self) -> Result<()> {
        self.channel.flush()?;
        for _ in 0..32 {
            let Some(frame) = self.channel.receive()? else {
                break;
            };
            let Message::Retain { leases } = decode(frame.value)? else {
                return Err("invalid guardian request".into());
            };
            let mut fds: Vec<_> = frame.fds.into_iter().map(Some).collect();
            for lease in leases {
                let fd = fds
                    .get_mut(lease.lock.index)
                    .and_then(Option::take)
                    .ok_or("invalid guardian lease descriptor")?;
                if !lease.directory.is_absolute()
                    || !File::from(fd.try_clone()?).metadata()?.is_file()
                {
                    return Err("invalid guardian resource lease".into());
                }
                self.leases.push(fd);
            }
            self.channel
                .queue(&encode(&Message::Retained {}), &mut vec![])?;
            self.channel.flush()?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lease::resource_exclusive;
    use std::{
        fs,
        os::{fd::AsRawFd, unix::net::UnixStream},
    };
    fn post_fork_failure_keeps_locks_through_cleanup(channel_failure: bool) {
        use std::{
            cell::Cell,
            io::Write,
            os::unix::fs::OpenOptionsExt,
            process::{Child, Command},
        };
        let _forking = goblins_unix::forking();
        let root = goblins_unix::temp_directory().unwrap();
        let runtime = root.join("runtime");
        goblins_unix::private_directory(&runtime).unwrap();
        fs::create_dir(runtime.join("store")).unwrap();
        let mut locks = vec![
            (crate::lease::flock(&runtime.join("lock"), false)
                .unwrap()
                .unwrap())
            .into(),
        ];
        let mut owners = vec![];
        let paths: Vec<_> = ["ancestor", "donor"]
            .into_iter()
            .map(|name| {
                let path = root.join(name);
                fs::create_dir(&path).unwrap();
                fs::write(path.join("root"), "retained").unwrap();
                let owner = ResourceLease::new(path.clone()).unwrap();
                locks.push(owner.share().unwrap().into_parts().1);
                owners.push(owner);
                path
            })
            .collect();
        let (_peer, stream) = UnixStream::pair().unwrap();
        let stream = if channel_failure {
            drop(stream);
            // A valid O_PATH descriptor makes actual Channel construction fail
            // when set_nonblocking runs, without closing an invalid OwnedFd.
            UnixStream::from(OwnedFd::from(
                File::options()
                    .read(true)
                    .custom_flags(libc::O_PATH)
                    .open("/dev/null")
                    .unwrap(),
            ))
        } else {
            stream
        };
        let bootstrap = Bootstrap::with_locks(stream, locks).unwrap();
        drop(owners);
        struct Reap(Child);
        impl Drop for Reap {
            fn drop(&mut self) {
                let _ = self.0.kill();
                let _ = self.0.wait();
            }
        }
        let mut child = Reap(Command::new("sleep").arg("60").spawn().unwrap());
        let child_pid = child.0.id();
        let disk = root.join("disk");
        fs::create_dir(&disk).unwrap();
        fs::write(disk.join("marker"), "cleanup required").unwrap();
        let (read, write) = goblins_unix::pipe().unwrap();
        drop(read);
        let ready_called = Cell::new(false);
        let cleaned = Cell::new(false);
        let retained = || {
            for path in &paths {
                assert!(path.join("root").exists());
                assert!(resource_exclusive(path).unwrap().is_none());
            }
            assert!(!crate::scope::remove_stale(&runtime, false).unwrap());
            assert!(runtime.join("store").exists());
        };
        let error = bootstrap
            .supervise(
                || {
                    ready_called.set(true);
                    writeln!(File::from(write), "{child_pid}")
                },
                |_| panic!("watch must not run after setup/readiness failure"),
                || {
                    retained();
                    let pid = child.0.id();
                    child.0.kill().unwrap();
                    assert!(!child.0.wait().unwrap().success());
                    assert_eq!(
                        unsafe { libc::waitpid(pid as i32, std::ptr::null_mut(), libc::WNOHANG) },
                        -1
                    );
                    assert_eq!(
                        std::io::Error::last_os_error().raw_os_error(),
                        Some(libc::ECHILD)
                    );
                    retained();
                    fs::remove_dir_all(&disk).unwrap();
                    retained();
                    cleaned.set(true);
                    Ok(())
                },
            )
            .unwrap_err();
        assert!(cleaned.get());
        assert_eq!(ready_called.get(), !channel_failure);
        if channel_failure {
            assert!(error.to_string().contains("os error 9"), "{error}");
        } else {
            assert_eq!(error.kind(), std::io::ErrorKind::BrokenPipe);
        }
        assert!(!disk.exists());
        for path in &paths {
            assert!(resource_exclusive(path).unwrap().is_some());
        }
        assert!(crate::scope::remove_stale(&runtime, false).unwrap());
        fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn post_fork_closed_readiness_pipe_reaps_and_cleans_before_releasing_locks() {
        post_fork_failure_keeps_locks_through_cleanup(false);
    }
    #[test]
    fn post_fork_channel_setup_failure_reaps_and_cleans_before_releasing_locks() {
        post_fork_failure_keeps_locks_through_cleanup(true);
    }
    #[test]
    fn closing_the_forked_child_bootstrap_preserves_the_guardian_endpoint() {
        let _forking = goblins_unix::forking();
        let (scope, stream) = UnixStream::pair().unwrap();
        let root = goblins_unix::temp_directory().unwrap();
        let owner = ResourceLease::new(root.clone()).unwrap();
        let initial = owner.share().unwrap().into_parts().1;
        let descriptor = initial.as_raw_fd();
        let mut bootstrap = Bootstrap::with_locks(stream, vec![initial]).unwrap();
        assert_ne!(
            unsafe { libc::fcntl(descriptor, libc::F_GETFD) } & libc::FD_CLOEXEC,
            0
        );
        let child = unsafe { libc::fork() };
        assert!(child >= 0);
        if child == 0 {
            drop((scope, bootstrap));
            unsafe {
                // The engine child closes the capability rather than keeping
                // cleanup authority through a later exec.
                libc::_exit(if libc::fcntl(descriptor, libc::F_GETFD) == -1 {
                    0
                } else {
                    1
                });
            }
        }
        let mut status = 0;
        assert_eq!(unsafe { libc::waitpid(child, &mut status, 0) }, child);
        assert!(libc::WIFEXITED(status));
        assert_eq!(libc::WEXITSTATUS(status), 0);
        assert_ne!(
            unsafe { libc::flock(owner.fd(), libc::LOCK_EX | libc::LOCK_NB) },
            0
        );
        let mut scope = Channel::new(scope).unwrap();
        let mut guardian = bootstrap.enter().unwrap();
        queue_retain(&mut scope, std::slice::from_ref(&owner)).unwrap();
        scope.flush().unwrap();
        guardian.receive().unwrap();
        retained(scope.receive().unwrap().unwrap().value).unwrap();
        drop((scope, owner));
        guardian.receive().unwrap();
        assert!(guardian.channel.closed());
        assert!(resource_exclusive(&root).unwrap().is_none());
        drop(guardian);
        assert!(resource_exclusive(&root).unwrap().is_some());
        fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn guardian_retains_all_batches_after_scope_eof_until_cleanup_finishes() {
        let _no_forks = goblins_unix::no_forks();
        let root = goblins_unix::temp_directory().unwrap();
        let count = crate::channel::MAX_FDS + 1;
        let leases: Vec<_> = (0..count)
            .map(|i| {
                let path = root.join(format!("donor{i}"));
                fs::create_dir(&path).unwrap();
                fs::write(path.join("root"), "retained").unwrap();
                ResourceLease::new(path).unwrap()
            })
            .collect();
        let paths: Vec<_> = leases.iter().map(|l| l.directory().to_path_buf()).collect();
        let (scope, guardian) = UnixStream::pair().unwrap();
        let mut scope = Channel::new(scope).unwrap();
        let mut guardian = Retainer::new(Channel::new(guardian).unwrap());
        let runtime = root.join("runtime");
        goblins_unix::private_directory(&runtime).unwrap();
        fs::create_dir(runtime.join("store")).unwrap();
        let runtime_lock = crate::lease::flock(&runtime.join("lock"), false)
            .unwrap()
            .unwrap();
        let guardian_lock = crate::lease::flock(&runtime.join("lock"), false)
            .unwrap()
            .unwrap();
        queue_locks(&mut scope, vec![(runtime.clone(), guardian_lock.into())]).unwrap();
        scope.flush().unwrap();
        guardian.receive().unwrap();
        retained(scope.receive().unwrap().unwrap().value).unwrap();
        for batch in leases.chunks(crate::channel::MAX_FDS) {
            queue_retain(&mut scope, batch).unwrap();
            scope.flush().unwrap();
            guardian.receive().unwrap();
            retained(scope.receive().unwrap().unwrap().value).unwrap();
        }
        // Every guardian lease is an independent OFD, including the last
        // batch: upgrading any donor's own lock must still fail.
        for lease in &leases {
            assert_ne!(
                unsafe { libc::flock(lease.fd(), libc::LOCK_EX | libc::LOCK_NB) },
                0
            );
        }
        drop(scope);
        guardian.receive().unwrap();
        assert!(guardian.channel.closed());
        drop(runtime_lock);
        assert!(!crate::scope::remove_stale(&runtime, false).unwrap());
        assert!(runtime.join("store").exists());
        drop(leases);
        for path in &paths {
            assert!(path.join("root").exists());
            assert!(resource_exclusive(path).unwrap().is_none());
        }
        assert_eq!(guardian.leases.len(), count + 1);
        assert!(guardian.leases.iter().all(|fd| fd.as_raw_fd() >= 0));
        // This is the cleanup-completed boundary, not the EOF boundary.
        drop(guardian);
        assert!(crate::scope::remove_stale(&runtime, false).unwrap());
        assert!(!runtime.exists());
        for path in &paths {
            assert!(resource_exclusive(path).unwrap().is_some());
        }
        fs::remove_dir_all(root).unwrap();
    }
}
