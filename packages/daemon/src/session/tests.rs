//! Host test adapter only, compiled into the Rust test executable. It lets the
//! existing black-box payload probes exercise Rust Session without installing a
//! debug RPC or fault injection switch in either production executable.
use super::*;
use serde_json::{Value, json};
use std::io::{BufRead, BufReader};

#[test]
fn profiles_preserve_path_precedence_across_duplicate_commands() {
    let fixture = ResourceLease::new(unix::temp_directory().unwrap()).unwrap();
    let package = |name: &str, commands: &[&str]| {
        let path = fixture.directory.join(name);
        fs::create_dir_all(path.join("bin")).unwrap();
        for command in commands {
            fs::write(path.join("bin").join(command), name).unwrap();
        }
        path
    };
    let client = package("client", &["goblins"]);
    let shell = package("shell", &["bash"]);
    let first = package("first", &["bash", "shared"]);
    let second = package("second", &["bash", "shared"]);
    let requested = package("requested", &["bash", "shared", "cowsay", "goblins"]);
    let later = package("later", &["cowsay", "cowthink"]);
    let launch: Launch = serde_json::from_value(json!({
        "shell": shell.join("bin/bash"), "posix_shell": shell.join("bin/bash"),
        "helper": "/unused", "bwrap": "/unused", "flake": "unused",
        "client_package": client, "initial_packages": [first, second]
    }))
    .unwrap();
    let mut session = Session::new(launch, None, Cancel::default()).unwrap();
    let generation = session.profile(&requested).unwrap();
    for (name, target) in [
        ("bash", shell.join("bin/bash")),
        ("shared", first.join("bin/shared")),
        ("goblins", client.join("bin/goblins")),
        ("cowsay", requested.join("bin/cowsay")),
    ] {
        assert_eq!(
            fs::read_link(generation.join("bin").join(name)).unwrap(),
            target
        );
    }
    symlink(&generation, session.directory.join("packages/current")).unwrap();
    session.packages.insert("zzz".into(), requested.clone());
    let next = session.profile(&later).unwrap();
    // A later grant keeps existing winners regardless of package name order.
    session.packages.insert("aaa".into(), later.clone());
    fs::remove_file(session.directory.join("packages/current")).unwrap();
    symlink(&next, session.directory.join("packages/current")).unwrap();
    assert_eq!(
        fs::read_link(next.join("bin/cowsay")).unwrap(),
        requested.join("bin/cowsay")
    );
    assert_eq!(
        fs::read_to_string(next.join("bin/cowsay")).unwrap(),
        "requested"
    );
    assert_eq!(
        fs::read_link(next.join("bin/cowthink")).unwrap(),
        later.join("bin/cowthink")
    );
    // Repeated outputs and subsequent generations never link through `current`.
    let repeated = session.profile(&later).unwrap();
    assert_eq!(
        fs::read_link(repeated.join("bin/cowsay")).unwrap(),
        requested.join("bin/cowsay")
    );
}

#[test]
#[ignore = "host integration driver; run through tests/test_integration.py"]
fn host_driver() {
    // The Python harness is the driver owner; its crash must also kill this
    // controller so the real helper's parent-death teardown is exercised.
    unsafe {
        libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL);
    }
    let mut session: Option<Session> = None;
    let mut input = BufReader::new(std::io::stdin());
    loop {
        let mut line = String::new();
        if input.read_line(&mut line).unwrap() == 0 {
            break;
        }
        let req: Value = serde_json::from_str(&line).unwrap();
        let result = (|| -> Result<Value> {
            match req["op"].as_str().ok_or("missing operation")? {
                "new" => {
                    let launch: Launch = serde_json::from_value(req["config"].clone())?;
                    session = Some(Session::new(
                        launch,
                        req["workspace"].as_str().map(Path::new),
                        Cancel::default(),
                    )?);
                    Ok(json!(null))
                }
                "start" => {
                    let s = session.as_mut().unwrap();
                    s.start(None)?;
                    let socket =
                        unix::seqpacket(Path::new(req["socket"].as_str().unwrap()), false)?;
                    for fd in [
                        s.input.as_ref().unwrap().as_raw_fd(),
                        s.output.as_ref().unwrap().as_raw_fd(),
                        s.listener.as_ref().unwrap().as_raw_fd(),
                    ] {
                        unix::send_terminal(socket.as_raw_fd(), fd)?;
                    }
                    Ok(json!(null))
                }
                "grant" => {
                    let s = session.as_mut().unwrap();
                    s.grant_with(
                        req["name"].as_str().unwrap(),
                        req["path"].as_str().map(Path::new),
                        |index, dest| {
                            if req["fail_after"].as_u64() == Some(index as u64) {
                                return Err("injected partial mount failure".into());
                            }
                            if req["fault"] == "symlink" && index == 1 {
                                if dest.is_dir() {
                                    fs::remove_dir(dest)?;
                                } else {
                                    fs::remove_file(dest)?;
                                }
                                symlink("/tmp", dest)?;
                            }
                            Ok(())
                        },
                    )?;
                    Ok(json!(null))
                }
                "closure" => {
                    let paths = req["paths"]
                        .as_array()
                        .unwrap()
                        .iter()
                        .map(|p| PathBuf::from(p.as_str().unwrap()))
                        .collect::<Vec<_>>();
                    Ok(serde_json::to_value(
                        session.as_ref().unwrap().closure(&paths)?,
                    )?)
                }
                "decide" => {
                    let s = session.as_mut().unwrap();
                    let request: goblins_protocol::Request =
                        serde_json::from_value(req["request"].clone())?;
                    let reply = if req["approved"] != true {
                        goblins_protocol::Reply::new(Some(request.id), "denied", None)
                    } else {
                        match s.grant(&request.package, None) {
                            Ok(()) => goblins_protocol::Reply::new(Some(request.id), "ready", None),
                            Err(e) => goblins_protocol::Reply::new(
                                Some(request.id),
                                "error",
                                Some(e.to_string()),
                            ),
                        }
                    };
                    Ok(serde_json::to_value(reply)?)
                }
                "flake" => {
                    session.as_mut().unwrap().launch.flake = req["value"].as_str().unwrap().into();
                    Ok(json!(null))
                }
                "state" => Ok(json!(null)),
                "close" => {
                    session.take();
                    Ok(json!(null))
                }
                _ => Err("unknown test operation".into()),
            }
        })();
        let state = if let Some(s) = session.as_mut() {
            json!({"directory":s.directory,"pid":s.helper_pid(),"identity":s.identity,"mounted":s.mounted,"packages":s.packages,"alive":s.alive()})
        } else {
            json!(null)
        };
        let response = match result {
            Ok(value) => json!({"value":value,"state":state}),
            Err(e) => json!({"error":e.to_string(),"state":state}),
        };
        println!("DRIVER {response}");
        std::io::stdout().flush().unwrap();
    }
}

/// A session directory as the daemon lays it out: `<session>/resources`.
fn resources() -> (PathBuf, PathBuf) {
    let session = unix::temp_directory().unwrap();
    let resources = session.join("resources");
    fs::create_dir(&resources).unwrap();
    fs::write(resources.join("snapshot"), "x").unwrap();
    (session, resources)
}

#[test]
fn the_last_lease_on_a_directory_removes_it_and_its_session() {
    let _no_forks = unix::no_forks();
    let (session, resources) = resources();
    let first = ResourceLease::new(resources.clone()).unwrap();
    let second = first.share().unwrap();
    // Two descriptions of their own: neither is a copy of the other.
    assert_ne!(first.fd(), second.fd());
    drop(first);
    assert!(resources.join("snapshot").exists());
    drop(second);
    assert!(!resources.exists());
    assert!(!session.exists());
}

#[test]
fn a_lease_another_process_still_holds_leaves_the_directory_for_the_last_holder() {
    let _no_forks = unix::no_forks();
    let (session, resources) = resources();
    let lease = ResourceLease::new(resources.clone()).unwrap();
    // The helper holds the lock through a copy of the descriptor.
    let copy = File::from(unix::owned(unsafe { libc::dup(lease.fd()) }).unwrap());
    drop(lease);
    assert!(resources.join("snapshot").exists());
    // Collecting garbage needs the exclusive lock, which the copy denies.
    assert!(
        crate::cleanup::resource_exclusive(&resources)
            .unwrap()
            .is_none()
    );
    drop(copy);
    assert!(
        crate::cleanup::resource_exclusive(&resources)
            .unwrap()
            .is_some()
    );
    fs::remove_dir_all(session).unwrap();
}

#[test]
fn a_directory_collected_meanwhile_is_not_a_problem() {
    let _no_forks = unix::no_forks();
    let (session, resources) = resources();
    let lease = ResourceLease::new(resources.clone()).unwrap();
    fs::remove_dir_all(&session).unwrap();
    drop(lease);
    assert!(!session.exists());
}

#[test]
fn a_directory_that_is_being_collected_cannot_be_leased() {
    let _no_forks = unix::no_forks();
    let (session, resources) = resources();
    let collecting = crate::cleanup::resource_exclusive(&resources)
        .unwrap()
        .unwrap();
    assert!(ResourceLease::new(resources.clone()).is_err());
    drop(collecting);
    drop(ResourceLease::new(resources).unwrap());
    assert!(!session.exists());
}

#[test]
fn a_handoff_holds_every_resources_directory_the_owner_holds_after_the_owner_is_gone() {
    let _no_forks = unix::no_forks();
    // A runs B, which runs C: C's workspace lives in A's resources.
    let (a_session, a_resources) = resources();
    let (b_session, b_resources) = resources();
    let owner =
        Inheritance::fixture_leases("shell", &[], vec![b_resources.clone(), a_resources.clone()]);
    let handoff = owner.handoff().unwrap();
    // Fresh leases of their own, never copies of the owner's.
    assert_eq!(handoff.leases.len(), 2);
    for (lease, own) in handoff.leases.iter().zip(owner.leases.iter()) {
        assert_ne!(lease.fd(), own.fd());
    }
    assert!(handoff.workspace.as_raw_fd() >= 0);
    assert!(handoff.working_directory.is_none());
    assert!(handoff.dev_shell.is_none());
    assert!(!handoff.docker);
    // The owners stop: what they held stays while the handoff holds it.
    drop(owner);
    assert!(a_resources.join("snapshot").exists());
    assert!(b_resources.join("snapshot").exists());
    drop(handoff);
    assert!(!a_session.exists());
    assert!(!b_session.exists());
}

#[test]
fn a_handoff_carries_the_docker_flag_as_it_was_when_it_was_made() {
    let _no_forks = unix::no_forks();
    let (session, resources) = resources();
    let owner = Inheritance::fixture_in("shell", &[], resources);
    assert!(!owner.handoff().unwrap().docker);
    owner.docker.store(true, Ordering::SeqCst);
    assert!(owner.handoff().unwrap().docker);
    // Read once: the flag later changing does not reach a handoff already made.
    let made = owner.handoff().unwrap();
    owner.docker.store(false, Ordering::SeqCst);
    assert!(made.docker);
    drop((made, owner));
    assert!(!session.exists());
}

#[test]
fn a_capability_held_elsewhere_keeps_every_directory_of_its_chain_for_the_engine_that_uses_them() {
    let _no_forks = unix::no_forks();
    // A scope keeps the capability of a Docker user, so that the directories
    // its engine's grants live in stay until the engine stops.
    let (a_session, a_resources) = resources();
    let (b_session, b_resources) = resources();
    let owner = Inheritance::fixture_leases("shell", &[], vec![b_resources, a_resources.clone()]);
    let engine = owner.clone();
    drop(owner);
    assert!(a_resources.join("snapshot").exists());
    drop(engine);
    assert!(!a_session.exists());
    assert!(!b_session.exists());
}
