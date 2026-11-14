//! A goblin's own attachment: the wait for an attach client's variables, and
//! a sandbox detaching itself.
use super::*;
use goblins_runtime::Stdio;
use goblins_runtime::config::{Handlers, OnAttach};

type Received = std::sync::Arc<std::sync::Mutex<VecDeque<Command>>>;

/// Goblin `a` serving with attach handlers whose client needs `session_id`
/// and `ready`, on a runtime that records its commands.
fn serving(d: &mut Controller, timeout: u64) -> (std::sync::mpsc::SyncSender<Event>, Received) {
    live(d, "a");
    let (runtime, events, received) = goblins_runtime::Fake::new();
    let a = d.sessions.get_mut("a").unwrap();
    a.runtime = Some(Box::new(runtime));
    a.init = true;
    let handlers = Handlers {
        on_attach: OnAttach {
            command: vec!["client".into(), "{session_id}".into()],
            timeout,
            wait_for: vec!["ready".into()],
        },
        on_detach: None,
    };
    a.serve(handlers.clone());
    // As Started leaves it: the policy has the handlers too.
    a.policy.as_mut().unwrap().handlers = Some(handlers);
    (events, received)
}
fn step(d: &mut Controller) {
    let mut a = d.sessions.remove("a").unwrap();
    d.attach_tick(&mut a);
    d.sessions.insert("a".into(), a);
}
fn set(d: &mut Controller, variables: Value) {
    d.sandbox_control_for_test(
        "a",
        "sessions.set-variables",
        json!({ "variables": variables }),
    )
    .unwrap();
}
/// A frontend on goblin `a`'s attach socket that has sent its hello.
fn hello(d: &Controller, version: u64) -> UnixStream {
    let mut frontend = UnixStream::connect(d.sessions["a"].directory.join("attach.sock")).unwrap();
    frontend
        .write_all(
            &rpc::encode(&json!({"jsonrpc":"2.0","method":"attach.hello",
                "params":{"version":version,"rows":24,"cols":80}}))
            .unwrap(),
        )
        .unwrap();
    frontend
}
/// Step until the frontend has a frame to read.
fn answer(d: &mut Controller, frontend: &mut UnixStream) -> Value {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    while std::time::Instant::now() < deadline {
        step(d);
        if unix::readable(frontend.as_raw_fd(), 10).unwrap() {
            return rpc::read(frontend).unwrap();
        }
    }
    panic!("the frontend got no answer");
}

#[test]
fn an_attach_waits_with_a_notice_a_second_then_refuses_naming_what_is_missing() {
    let _no_forks = unix::no_forks();
    let mut d = daemon();
    let path = d.state.clone();
    let (_events, received) = serving(&mut d, 2);
    // A cleared value is not ready, and session_id was never set.
    set(&mut d, json!({"ready":"yes"}));
    set(&mut d, json!({"ready":""}));
    let started = std::time::Instant::now();
    let mut frontend = hello(&d, 1);
    let mut notices = vec![];
    let refused = loop {
        let frame = answer(&mut d, &mut frontend);
        if frame["method"] != "attach.waiting" {
            break frame;
        }
        assert_eq!(frame["params"]["missing"], json!(["ready", "session_id"]));
        notices.push(std::time::Instant::now());
    };
    // One notice at once and one a second later; then the 2 s timeout.
    assert_eq!(notices.len(), 2, "{refused}");
    assert!(notices[1] - notices[0] >= std::time::Duration::from_millis(900));
    assert!(started.elapsed() >= std::time::Duration::from_secs(2));
    let log = d.sessions["a"].directory.join("server.log");
    assert_eq!(
        refused["error"]["message"],
        format!(
            "the goblin did not set ready, session_id in time; its server log is {}; restart the goblin",
            log.display()
        )
    );
    // No client started, and the goblin runs on.
    assert!(received.lock().unwrap().is_empty());
    let a = &d.sessions["a"];
    assert_eq!(a.record.state, "running");
    assert!(a.live());
    assert!(!a.record.terminal_attached);
    drop(d);
    fs::remove_dir_all(path).unwrap();
}

#[test]
fn a_frontend_that_leaves_while_waiting_starts_nothing_and_frees_the_slot() {
    let _no_forks = unix::no_forks();
    let mut d = daemon();
    let path = d.state.clone();
    let (_events, received) = serving(&mut d, 30);
    set(&mut d, json!({"session_id":"c819"}));
    let mut leaving = hello(&d, 1);
    let notice = answer(&mut d, &mut leaving);
    assert_eq!(notice["method"], "attach.waiting");
    assert_eq!(notice["params"]["missing"], json!(["ready"]));
    leaving.shutdown(std::net::Shutdown::Both).unwrap();
    step(&mut d);
    // Ready now, but nobody is waiting for the client.
    set(&mut d, json!({"ready":"yes"}));
    for _ in 0..3 {
        step(&mut d);
    }
    assert!(received.lock().unwrap().is_empty());
    // A frontend of another version is refused.
    let mut older = hello(&d, 99);
    assert_eq!(
        answer(&mut d, &mut older)["error"]["message"],
        "unsupported attach.hello version"
    );
    // The slot is free: the next frontend gets a client at its size.
    let _next = hello(&d, 1);
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    let spawned = loop {
        step(&mut d);
        if let Some(command) = received.lock().unwrap().pop_front() {
            break command;
        }
        assert!(std::time::Instant::now() < deadline, "no client started");
    };
    let Command::Spawn {
        argv,
        stdio: Stdio::NewPty { rows: 24, cols: 80 },
        ..
    } = spawned
    else {
        panic!("expected the client's spawn: {spawned:?}");
    };
    assert_eq!(argv, Some(vec!["client".to_string(), "c819".into()]));
    drop(d);
    fs::remove_dir_all(path).unwrap();
}

#[test]
fn a_goblin_detaches_only_itself_and_keeps_its_pending_approval() {
    let _no_forks = unix::no_forks();
    let mut d = daemon();
    let path = d.state.clone();
    live(&mut d, "a");
    // Kept, so the tick does not see b's runtime end.
    let _b = live(&mut d, "b");
    capable(&mut d, "a", Support::linux());
    let (runtime, events, received) = goblins_runtime::Fake::new();
    let a = d.sessions.get_mut("a").unwrap();
    a.runtime = Some(Box::new(runtime));
    a.terminal.connected = true;
    a.terminal.attached = true;
    a.record.terminal_attached = true;
    // A package request from a's sandbox waits for its approval.
    events.send(Event::SandboxOpened { connection: 1 }).unwrap();
    events
        .send(sandbox_event(1, 1, "initialize", json!({"api":1})))
        .unwrap();
    d.tick().unwrap();
    events
        .send(sandbox_event(
            1,
            2,
            "permissions.request",
            json!({"kind":"package","package":"hello","reason":"t"}),
        ))
        .unwrap();
    d.tick().unwrap();
    let request = d.permissions.back().unwrap().clone();
    assert_eq!(
        (
            request.session.as_str(),
            request.kind.as_str(),
            request.state.as_str()
        ),
        ("a", "package", "pending")
    );
    received.lock().unwrap().clear();
    // Goblin b names no goblin to detach, and is not attached itself.
    let refused = sandbox_call(&mut d, "b", "sessions.detach", json!({"session":"a"}));
    assert_eq!(refused["error"]["code"], -32602, "{refused}");
    let refused = sandbox_call(&mut d, "b", "sessions.detach", json!({}));
    assert_eq!(
        refused["error"],
        json!({"code":-32009,"message":"terminal is not attached"})
    );
    assert!(received.lock().unwrap().is_empty());
    assert!(d.sessions["a"].record.terminal_attached);
    // Goblin a detaches itself; its request still waits.
    let accepted = sandbox_call(&mut d, "a", "sessions.detach", json!({}));
    assert_eq!(accepted["result"], json!({"accepted":true}));
    assert!(matches!(
        received.lock().unwrap().pop_front(),
        Some(Command::Detach {
            terminal: TerminalId::Main
        })
    ));
    assert!(received.lock().unwrap().is_empty(), "nothing was cancelled");
    d.tick().unwrap();
    let state = &d
        .permissions
        .iter()
        .find(|r| r.id == request.id)
        .unwrap()
        .state;
    assert_eq!(state, "pending");
    assert!(!d.sessions["a"].record.terminal_attached);
    drop(d);
    fs::remove_dir_all(path).unwrap();
}
