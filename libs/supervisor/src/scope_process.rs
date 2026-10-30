//! Scope-mode child. No daemon, CLI or sandbox client startup runs here.
use crate::{Result, process::Cancellation, scope_keeper::Namespaces, scope_storage::Storage};
use goblins_runtime::{
    channel::{Channel, Descriptor, Frame},
    scope::{self, Join, Message},
};
use goblins_unix as unix;
use std::{
    collections::BTreeMap,
    fs::{self, File},
    os::{
        fd::{AsRawFd, FromRawFd, OwnedFd},
        unix::net::UnixStream,
    },
    path::PathBuf,
};

/// Must be called directly by the minimal scope executable before other setup.
pub fn main() -> Result<()> {
    // The socketpair was created by the daemon before fork; its peer credentials
    // identify that original daemon even if it died before this process ran.
    unix::cvt(unsafe { libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL) })?;
    let mut peer: libc::ucred = unsafe { std::mem::zeroed() };
    let mut size = std::mem::size_of_val(&peer) as libc::socklen_t;
    unix::cvt(unsafe {
        libc::getsockopt(
            scope::CONTROL_FD,
            libc::SOL_SOCKET,
            libc::SO_PEERCRED,
            (&mut peer as *mut libc::ucred).cast(),
            &mut size,
        )
    })?;
    if size as usize != std::mem::size_of_val(&peer) || unsafe { libc::getppid() } != peer.pid {
        return Err("scope daemon exited before startup".into());
    }
    unix::cvt(unsafe { libc::syscall(libc::SYS_close_range, 4u32, u32::MAX, 0u32) as i32 })?;
    unix::cvt(unsafe { libc::syscall(libc::SYS_close_range, 0u32, 2u32, 0u32) as i32 })?;
    // Replace closed inherited stdio with new null handles. Otherwise namespace
    // or lease capabilities may occupy 0..2 and be overwritten by subprocess
    // stdio setup (the helper deliberately rejects capability fds below 3).
    for expected in 0..3 {
        let fd = unix::cvt(unsafe {
            libc::open(c"/dev/null".as_ptr(), libc::O_RDWR | libc::O_CLOEXEC)
        })?;
        if fd != expected {
            return Err("scope descriptor allowlist setup failed".into());
        }
    }
    unix::cvt(unsafe { libc::fcntl(scope::CONTROL_FD, libc::F_SETFD, libc::FD_CLOEXEC) })?;
    let stream = unsafe { UnixStream::from_raw_fd(scope::CONTROL_FD) };
    let mut control = Channel::new(stream)?;
    let setup = loop {
        if let Some(frame) = control.receive()? {
            break scope::decode(frame.value)?;
        }
        if control.closed() {
            return Ok(());
        }
        wait(&control);
    };
    let Message::Setup {
        api,
        name,
        directory,
    } = setup
    else {
        return Err("scope setup must be first".into());
    };
    if api != scope::API || !goblins_protocol::scope_name(&name) || !directory.is_absolute() {
        return Err("invalid scope setup".into());
    }
    unix::private_directory(&directory)?;
    // The keeper opens its own shared lifetime lease on this inode. Cleanup
    // and relaunch require an exclusive lock, as for session resources.
    let _runtime_lock = goblins_runtime::lease::flock(&directory.join("lock"), false)?
        .ok_or("scope runtime is being collected")?;
    let result = run(&mut control, &name, directory);
    if let Err(error) = &result {
        let _ = send(
            &mut control,
            &Message::Error {
                message: error.to_string(),
            },
            vec![],
        );
        let _ = control.flush();
    }
    result
}
fn wait(channel: &Channel) {
    if let Some(fd) = channel.fd() {
        let mut poll = libc::pollfd {
            fd,
            events: libc::POLLIN
                | if channel.wants_write() {
                    libc::POLLOUT
                } else {
                    0
                },
            revents: 0,
        };
        unsafe {
            libc::poll(&mut poll, 1, 20);
        }
    }
}
fn send(channel: &mut Channel, message: &Message, mut fds: Vec<OwnedFd>) -> Result<()> {
    channel.queue(&scope::encode(message), &mut fds)?;
    Ok(())
}
struct State {
    join: Join,
    // Drop pasta before the keeper; drop storage last.
    _network: Option<crate::network::Network>,
    namespaces: Namespaces,
    storage: Storage,
}
impl State {
    fn new(name: &str, directory: &std::path::Path, join: Join) -> Result<Self> {
        let cancel = Cancellation::default();
        let storage =
            Storage::open(name, &join.definition)?.ok_or("scope storage still stopping")?;
        let namespaces = Namespaces::start(
            &join.bwrap,
            &join.helper,
            directory,
            &directory.join("lock"),
            join.definition.pid,
            storage.temporary_lease(),
            &cancel,
        )?;
        let network = join
            .pasta
            .as_ref()
            .map(|pasta| {
                crate::network::Network::attach(
                    pasta,
                    namespaces.user.try_clone()?,
                    namespaces.net.try_clone()?,
                    &namespaces.net_path,
                    directory,
                    &cancel,
                )
            })
            .transpose()?;
        Ok(Self {
            join,
            _network: network,
            namespaces,
            storage,
        })
    }
    fn compatible(&self, name: &str, join: &Join) -> Result<()> {
        if self.join.pasta.is_some() != join.pasta.is_some() {
            return Err(
                format!("scope '{name}' has a different network policy (online/offline)").into(),
            );
        }
        if self.join.definition.docker && self.join.allow_nix != join.allow_nix {
            return Err(
                format!("scope '{name}' has Docker and a different allowNix setting").into(),
            );
        }
        if self.join.definition != join.definition {
            return Err(format!(
                "scope '{name}' is running with a different definition; stop its members first"
            )
            .into());
        }
        Ok(())
    }
    fn joined(&self, directory: &std::path::Path) -> Result<(Message, Vec<OwnedFd>)> {
        let mut fds = vec![];
        let mut add = |file: &File| -> Result<Descriptor> {
            let index = fds.len();
            fds.push(file.try_clone()?.into());
            Ok(Descriptor::new(index))
        };
        let outer = add(&self.namespaces.outer)?;
        let user = add(&self.namespaces.user)?;
        let net = add(&self.namespaces.net)?;
        let pid = self.namespaces.pid.as_ref().map(&mut add).transpose()?;
        Ok((
            Message::Joined {
                outer,
                user,
                net,
                pid,
                directory: directory.to_path_buf(),
            },
            fds,
        ))
    }
}
struct Member {
    channel: Channel,
    joined: bool,
    leaving: bool,
    left_sent: bool,
    mountns: Option<File>,
    pidns: Option<File>,
}
impl Member {
    fn cleanup(&mut self) {
        if let Some(mountns) = self.mountns.take() {
            kill_member(self.pidns.as_ref(), &mountns);
        }
    }
}
impl Drop for Member {
    fn drop(&mut self) {
        self.cleanup();
    }
}
// Acknowledgement is a lifetime boundary, not just a notification. Last
// leavers remain connected until the scope's resources have been dropped.
fn acknowledge_leavers(members: &mut [Member]) -> Result<()> {
    for member in members
        .iter_mut()
        .filter(|m| m.leaving && !m.channel.closed())
    {
        if !member.left_sent {
            send(&mut member.channel, &Message::Left {}, vec![])?;
            member.left_sent = true;
        }
        member.channel.flush()?;
        if !member.channel.wants_write() {
            member.channel.close();
        }
    }
    Ok(())
}
fn acknowledge_if_members_remain(members: &mut [Member]) -> Result<()> {
    if members.iter().any(|m| !m.leaving && !m.channel.closed()) {
        acknowledge_leavers(members)?;
    }
    Ok(())
}
fn finish_stop<T>(state: &mut Option<T>, members: &mut [Member]) -> Result<()> {
    for member in members.iter_mut() {
        member.cleanup();
    }
    drop(state.take());
    acknowledge_leavers(members)
}
fn take_fd(frame: &mut Frame, descriptor: Descriptor) -> Result<OwnedFd> {
    // Descriptor references were checked by Channel. Move, never duplicate, so
    // every refusal/EOF/error drops exactly the capabilities received.
    if descriptor.index >= frame.fds.len() {
        return Err("invalid scope descriptor".into());
    }
    Ok(frame.fds.remove(descriptor.index))
}
fn run(control: &mut Channel, name: &str, directory: PathBuf) -> Result<()> {
    let mut state: Option<State> = None;
    let mut members: Vec<Member> = vec![];
    let mut added = 0u64;
    let mut idle_reported = None;
    loop {
        control.flush()?;
        for _ in 0..32 {
            let Some(mut frame) = control.receive()? else {
                break;
            };
            match scope::decode(frame.value.clone())? {
                Message::AddMember { fd } => {
                    let stream = UnixStream::from(take_fd(&mut frame, fd)?);
                    members.push(Member {
                        channel: Channel::new(stream)?,
                        joined: false,
                        leaving: false,
                        left_sent: false,
                        mountns: None,
                        pidns: None,
                    });
                    added = added
                        .checked_add(1)
                        .ok_or("scope membership counter exhausted")?;
                }
                Message::Stop {} => {
                    send(control, &Message::Stopping {}, vec![])?;
                    control.flush()?;
                    finish_stop(&mut state, &mut members)?;
                    return Ok(());
                }
                _ => return Err("invalid scope control request".into()),
            }
        }
        if control.closed() {
            break;
        }
        for member in &mut members {
            if member.channel.flush().is_err() {
                continue;
            }
            let request = match member.channel.receive() {
                Ok(Some(frame)) => frame,
                Ok(None) => continue,
                Err(_) => continue,
            };
            let mut frame = request;
            let result = (|| -> Result<()> {
                match scope::decode(frame.value.clone())? {
                    Message::Join(join) if !member.joined => {
                        if let Some(state) = &state {
                            state.compatible(name, &join)?;
                        } else {
                            state = Some(State::new(name, &directory, join)?);
                        }
                        let (message, fds) = state.as_ref().unwrap().joined(&directory)?;
                        send(&mut member.channel, &message, fds)?;
                        member.pidns = state
                            .as_ref()
                            .unwrap()
                            .namespaces
                            .pid
                            .as_ref()
                            .map(File::try_clone)
                            .transpose()?;
                        member.joined = true;
                    }
                    Message::Leave {} if member.joined && !member.leaving => {
                        member.cleanup();
                        member.leaving = true;
                    }
                    Message::OpenStorage { names } if member.joined && !member.leaving => {
                        if names.len() > goblins_runtime::channel::MAX_FDS {
                            return Err("storage request exceeds frame budget".into());
                        }
                        let mut fds = vec![];
                        let mut directories = BTreeMap::new();
                        for name in names {
                            if directories.contains_key(&name) {
                                return Err("duplicate scope storage request".into());
                            }
                            let index = fds.len();
                            fds.push(
                                state
                                    .as_ref()
                                    .unwrap()
                                    .storage
                                    .directory(&name)?
                                    .try_clone()?
                                    .into(),
                            );
                            directories.insert(name, Descriptor::new(index));
                        }
                        send(&mut member.channel, &Message::Storage { directories }, fds)?;
                    }
                    Message::RegisterMember { mountns }
                        if member.joined && !member.leaving && member.mountns.is_none() =>
                    {
                        let namespace = File::from(take_fd(&mut frame, mountns)?);
                        if unsafe { libc::ioctl(namespace.as_raw_fd(), 0xb703) }
                            != libc::CLONE_NEWNS
                        {
                            return Err("invalid member mount namespace".into());
                        }
                        member.mountns = Some(namespace);
                        send(&mut member.channel, &Message::Registered {}, vec![])?;
                    }
                    _ => return Err("invalid scope member request".into()),
                }
                Ok(())
            })();
            if let Err(error) = result {
                let _ = send(
                    &mut member.channel,
                    &Message::Error {
                        message: error.to_string(),
                    },
                    vec![],
                );
                let _ = member.channel.flush();
                member.channel.close();
            }
        }
        members.retain(|member| !member.channel.closed());
        acknowledge_if_members_remain(&mut members)?;
        if added > 0
            && members
                .iter()
                .all(|member| member.leaving || member.channel.closed())
            && idle_reported != Some(added)
        {
            send(control, &Message::Idle { added }, vec![])?;
            control.flush()?;
            idle_reported = Some(added);
        }
        if state.as_mut().is_some_and(|s| !s.namespaces.alive()) {
            return Err("scope keeper exited".into());
        }
        // Member sockets have bounded work per tick too. A short poll on the
        // control socket keeps idle scopes quiet without missing member EOF.
        wait(control);
    }
    // EOF also cleans every registered namespace before dropping storage.
    finish_stop(&mut state, &mut members)
}
fn kill_member(pid: Option<&File>, mountns: &File) {
    use std::os::unix::fs::MetadataExt;
    let (Some(pid), Ok(mountns)) = (
        pid.and_then(|f| f.metadata().ok()).map(|m| m.ino()),
        mountns.metadata().map(|m| m.ino()),
    ) else {
        return;
    };
    for _ in 0..100 {
        let mut found = false;
        for entry in fs::read_dir("/proc").into_iter().flatten().flatten() {
            let Some(id) = entry
                .file_name()
                .to_str()
                .and_then(|n| n.parse::<i32>().ok())
            else {
                continue;
            };
            let Ok(pidfd) =
                unix::owned(unsafe { libc::syscall(libc::SYS_pidfd_open, id, 0) as i32 })
            else {
                continue;
            };
            let matches = |kind: &str, inode| {
                fs::metadata(format!("/proc/{id}/ns/{kind}")).is_ok_and(|m| m.ino() == inode)
            };
            if matches("pid", pid) && matches("mnt", mountns) {
                found = true;
                unsafe {
                    libc::syscall(
                        libc::SYS_pidfd_send_signal,
                        pidfd.as_raw_fd(),
                        libc::SIGKILL,
                        std::ptr::null::<libc::siginfo_t>(),
                        0,
                    );
                }
            }
        }
        if !found {
            return;
        }
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn member(leaving: bool) -> (Member, Channel) {
        let (server, client) = UnixStream::pair().unwrap();
        (
            Member {
                channel: Channel::new(server).unwrap(),
                joined: true,
                leaving,
                left_sent: false,
                mountns: None,
                pidns: None,
            },
            Channel::new(client).unwrap(),
        )
    }
    #[test]
    fn last_leaver_is_held_until_storage_teardown() {
        let directory = unix::temp_directory().unwrap();
        let path = directory.join("storage.lock");
        let mut storage = Some(
            goblins_runtime::lease::flock(&path, false)
                .unwrap()
                .unwrap(),
        );
        let (last, mut peer) = member(true);
        let mut members = vec![last];
        acknowledge_if_members_remain(&mut members).unwrap();
        assert!(peer.receive().unwrap().is_none());
        assert!(
            goblins_runtime::lease::flock(&path, true)
                .unwrap()
                .is_none()
        );
        finish_stop(&mut storage, &mut members).unwrap();
        assert!(matches!(
            scope::decode(peer.receive().unwrap().unwrap().value).unwrap(),
            Message::Left {}
        ));
        assert!(
            goblins_runtime::lease::flock(&path, true)
                .unwrap()
                .is_some()
        );
        fs::remove_dir_all(directory).unwrap();
    }
    #[test]
    fn another_member_allows_leave_without_releasing_storage() {
        let directory = unix::temp_directory().unwrap();
        let path = directory.join("storage.lock");
        let mut storage = Some(
            goblins_runtime::lease::flock(&path, false)
                .unwrap()
                .unwrap(),
        );
        let (leaver, mut peer) = member(true);
        let (active, _active_peer) = member(false);
        let mut members = vec![leaver, active];
        acknowledge_if_members_remain(&mut members).unwrap();
        assert!(matches!(
            scope::decode(peer.receive().unwrap().unwrap().value).unwrap(),
            Message::Left {}
        ));
        assert!(
            goblins_runtime::lease::flock(&path, true)
                .unwrap()
                .is_none()
        );
        finish_stop(&mut storage, &mut members).unwrap();
        assert!(
            goblins_runtime::lease::flock(&path, true)
                .unwrap()
                .is_some()
        );
        fs::remove_dir_all(directory).unwrap();
    }
}
