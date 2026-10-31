//! Controller lifecycle authority with real Supervised children, never Local.
use super::*;
use crate::supervised::tests as child;

#[test]
fn owner_death_during_export_fails_member_naming_owner_and_releases_every_lease() {
    let _forking = unix::forking();
    let mut d = daemon();
    let state = d.state.clone();
    let (owner, _events, _received) = raid_owner(&mut d);
    let process_root = state.join("owner-process");
    unix::private_directory(&process_root).unwrap();
    let (scope, mut scope_observer) = UnixStream::pair().unwrap();
    scope_observer.set_nonblocking(true).unwrap();
    let runtime = child::unacked_member(&process_root, Some(OwnedFd::from(scope).into()));
    d.sessions.get_mut(&owner).unwrap().runtime = Some(Box::new(runtime));
    let root = state.join("manifest-root");
    fs::write(&root, "owned").unwrap();
    d.current_manifest = Some(std::sync::Arc::new(
        goblins_runtime::config::ManifestLease::adopt(state.join("manifest"), Some(root.clone())),
    ));
    d.backend = RuntimeBackend::Supervised;
    d.executable = child::executable(&state, "engine");
    let member = run_member(&mut d, &owner, "owner-death");
    assert_eq!(d.sessions[&member].record.state, "starting");
    assert!(
        !state.join("ready").exists(),
        "member placeholder must not spawn before export"
    );
    d.current_manifest = None;
    assert!(root.exists(), "accepted member owns its rooted manifest");
    let (mut c, mut peer) = connection(Role::Sandbox(owner.clone()));
    c.waiting = Some((call_key(42), json!(7)));
    d.connections.push(c);
    d.sessions.get_mut(&owner).unwrap().call = Some(Call {
        serial: 42,
        connection: 1,
    });
    tick_until(&mut d, |_| process_root.join("export").exists());
    d.sessions
        .get_mut(&owner)
        .unwrap()
        .runtime
        .as_mut()
        .unwrap()
        .abort();
    tick_until(&mut d, |d| {
        d.sessions[&owner].runtime.is_none() && d.sessions[&member].runtime.is_none()
    });
    assert_eq!(d.sessions[&owner].record.state, "failed");
    assert_eq!(d.sessions[&member].record.state, "failed");
    assert_eq!(
        d.sessions[&member].record.detail.as_deref(),
        Some("goblin boss stopped before it could hand over its resources")
    );
    assert!(d.sessions[&member].exporting.is_none());
    assert!(d.sessions[&member].launch_manifest.is_none());
    assert!(!root.exists());
    assert!(!process_root.join("resources").exists());
    assert_eq!(scope_observer.read(&mut [0]).unwrap(), 0);
    assert!(unix::readable(peer.as_raw_fd(), 100).unwrap());
    assert_eq!(
        rpc::read(&mut peer).unwrap(),
        json!({"jsonrpc":"2.0","id":7,"error":{"code":-32009,"message":"payload stopped"}})
    );
    assert_eq!(
        peer.read(&mut [0]).unwrap(),
        0,
        "the failure frame is followed by connection EOF"
    );
    assert!(d.sessions[&owner].call.is_none());
    assert!(
        !d.connections
            .iter()
            .any(|c| matches!(&c.role, Role::Sandbox(s) if s == &owner))
    );
    assert!(!d.retiring.contains(&owner) && !d.retiring.contains(&member));
    drop(d);
    fs::remove_dir_all(state).unwrap();
}

#[test]
fn death_of_stopped_terminal_supervisor_never_revives_name_history_or_record() {
    let _forking = unix::forking();
    let mut d = daemon();
    let state = d.state.clone();
    let id = session_with_results_id(&mut d, "retained", vec![]);
    let process_root = state.join("terminal-process");
    unix::private_directory(&process_root).unwrap();
    let runtime = child::launch(&process_root, "stopped");
    let a = d.sessions.get_mut(&id).unwrap();
    a.runtime = Some(Box::new(runtime));
    a.record.agent_name = "kid".into();
    a.pending = None;
    let record = line_record(a, None);
    tick_until(&mut d, |d| !d.sessions[&id].live());
    assert!(d.sessions[&id].runtime.is_some());
    assert_eq!(d.sessions[&id].record.state, "stopped");
    assert_eq!(d.allocate_name(Some("kid")).unwrap(), "kid");
    assert!(d.resolve_session("kid").is_err());
    assert!(d.recreate_conflict(&record).is_none());
    assert!(!d.live_lines().contains(&record.id));
    d.sessions
        .get_mut(&id)
        .unwrap()
        .runtime
        .as_mut()
        .unwrap()
        .abort();
    tick_until(&mut d, |d| d.sessions[&id].runtime.is_none());
    let a = &d.sessions[&id];
    assert_eq!(a.record.state, "stopped");
    assert!(
        a.terminal.interrupted
            && !a.terminal.connected
            && !a.terminal.attached
            && !a.terminal.detached
    );
    assert_eq!(d.allocate_name(Some("kid")).unwrap(), "kid");
    assert!(d.recreate_conflict(&record).is_none());
    assert!(!d.live_lines().contains(&record.id));
    assert!(!process_root.join("spawn").exists());
    drop(d);
    fs::remove_dir_all(state).unwrap();
}

#[test]
fn shutdown_stops_and_reaps_stalled_supervisors_in_parallel() {
    let _forking = unix::forking();
    let mut d = daemon();
    let state = d.state.clone();
    let mut pids = vec![];
    for n in 0..4 {
        let id = session_with_results_id(&mut d, &format!("stalled-{n}"), vec![]);
        let root = state.join(format!("child-{n}"));
        unix::private_directory(&root).unwrap();
        let mut runtime = child::launch(&root, "hold");
        child::ready(&mut runtime, &root);
        child::pause(&runtime);
        pids.push(fs::read_to_string(root.join("pid")).unwrap());
        d.sessions.get_mut(&id).unwrap().runtime = Some(Box::new(runtime));
    }
    d.backend = RuntimeBackend::Supervised;
    let start = Instant::now();
    drop(d);
    assert!(
        start.elapsed() < std::time::Duration::from_secs(6),
        "four 2-second acknowledgement budgets must run concurrently"
    );
    for pid in pids {
        assert!(
            !Path::new(&format!("/proc/{pid}")).exists(),
            "supervisor was not reaped"
        );
    }
    fs::remove_dir_all(state).unwrap();
}
