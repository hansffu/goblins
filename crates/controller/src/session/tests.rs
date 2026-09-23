//! Host test adapter only, compiled into the Rust test executable. It lets the
//! existing black-box payload probes exercise Rust Session without installing a
//! debug RPC or fault injection switch in either production executable.
use super::*;
use serde_json::{Value, json};
use std::io::{BufRead, BufReader};

#[test]
fn profiles_preserve_path_precedence_across_duplicate_commands() {
    let fixture = SessionDirectory(unix::temp_directory().unwrap());
    let package = |name: &str, commands: &[&str]| {
        let path = fixture.0.join(name);
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
