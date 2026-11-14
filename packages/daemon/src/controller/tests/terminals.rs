//! `terminals.*` on the host endpoint, against a runtime the test drives.
use super::*;
use goblins_runtime::terminal::{self, Call as TerminalCall, Condition, Source};
use goblins_runtime::{SpawnKind, Stdio};

type Received = std::sync::Arc<std::sync::Mutex<VecDeque<Command>>>;

/// A running goblin `a` with an init channel and a runtime that records its
/// commands.
fn goblin() -> (Controller, std::sync::mpsc::SyncSender<Event>, Received) {
    let mut d = daemon();
    live(&mut d, "a");
    capable(&mut d, "a", Support::linux());
    let (runtime, events, received) = goblins_runtime::Fake::new();
    let a = d.sessions.get_mut("a").unwrap();
    a.runtime = Some(Box::new(runtime));
    a.init = true;
    (d, events, received)
}
fn take(received: &Received) -> Command {
    received
        .lock()
        .unwrap()
        .pop_front()
        .expect("the runtime was sent a command")
}
fn frame(c: &mut Connection) -> Value {
    let data = c.output.pop_front().expect("the connection has an answer");
    let body = data.windows(4).position(|w| w == b"\r\n\r\n").unwrap() + 4;
    rpc::parse(&data[body..]).unwrap()
}
fn call(d: &mut Controller, c: &mut Connection, method: &str, params: Value) {
    d.dispatch(
        c,
        rpc::Call {
            jsonrpc: "2.0".into(),
            id: Some(json!(5)),
            method: method.into(),
            params,
        },
    );
}
/// A host connection with a call made and its answer given back.
fn ask(d: &mut Controller, method: &str, params: Value) -> Value {
    let (mut c, _peer) = connection(Role::Host);
    c.initialized = true;
    call(d, &mut c, method, params);
    frame(&mut c)
}
/// Create terminal `t` with a started program, returning its record.
fn create(d: &mut Controller, received: &Received, params: Value) -> Value {
    let (mut c, _peer) = connection(Role::Host);
    c.id = d.number();
    c.initialized = true;
    call(d, &mut c, "terminals.create", params);
    assert!(c.output.is_empty(), "the answer waits for the program to start");
    d.connections.push(c);
    let Command::Spawn { id, .. } = take(received) else {
        panic!("expected a spawn")
    };
    d.process_event("a", Event::Spawned { id, pid: 42 });
    let c = d.connections.pop().unwrap();
    let mut c = c;
    let reply = frame(&mut c);
    assert!(c.waiting.is_none());
    reply["result"].clone()
}

#[test]
fn create_starts_a_terminal_and_answers_when_it_runs() {
    let _no_forks = unix::no_forks();
    let (mut d, _events, received) = goblin();
    let (mut c, _peer) = connection(Role::Host);
    c.id = d.number();
    c.initialized = true;
    call(
        &mut d,
        &mut c,
        "terminals.create",
        json!({"session":"a","argv":["/bin/fish","-l"],"cwd":"/work","env":[["A","b"]],"name":"app","rows":30,"cols":100}),
    );
    d.connections.push(c);
    let Command::Spawn {
        id,
        kind,
        argv,
        cwd,
        env,
        stdio,
    } = take(&received)
    else {
        panic!("expected a spawn")
    };
    assert_eq!(kind, SpawnKind::Terminal { exit_report: None });
    assert_eq!(argv, Some(vec!["/bin/fish".to_string(), "-l".into()]));
    assert_eq!(cwd.as_deref(), Some("/work"));
    assert_eq!(env, [("A".to_string(), "b".to_string())]);
    assert!(matches!(stdio, Stdio::NewPty { rows: 30, cols: 100 }));
    // Not listed, and no answer, until it runs.
    assert!(d.sessions["a"].record.terminals.is_empty());
    d.process_event("a", Event::Spawned { id, pid: 42 });
    let mut c = d.connections.pop().unwrap();
    let reply = frame(&mut c);
    let record = &reply["result"];
    assert_eq!(record["id"], "t1");
    assert_eq!(record["name"], "app");
    assert_eq!(record["label"], "app");
    assert_eq!(record["origin"], "host");
    assert_eq!((record["rows"].as_u64(), record["cols"].as_u64()), (Some(30), Some(100)));
    assert_eq!(record["attached"], false);
    assert!(record["started_at"].as_u64().unwrap() > 0);
    assert_eq!(d.sessions["a"].record.terminals.len(), 1);
    // Unnamed: the program's name labels it; the default size is 40x120; IDs
    // count up and are never reused.
    let second = create(&mut d, &received, json!({"session":"a","argv":["/usr/bin/gradle","run"]}));
    assert_eq!((second["id"].as_str(), second["label"].as_str()), (Some("t2"), Some("gradle")));
    assert_eq!((second["rows"].as_u64(), second["cols"].as_u64()), (Some(40), Some(120)));
    assert_eq!(second["name"], Value::Null);
    let shell = create(&mut d, &received, json!({"session":"a"}));
    assert_eq!(shell["label"], "shell");
    let list = ask(&mut d, "terminals.list", json!({"session":"a"}));
    assert_eq!(list["result"]["terminals"].as_array().unwrap().len(), 3);
    // The program ends: the terminal leaves the list; its ID is not reused.
    let exec = d.sessions["a"].terminals.entries[0].exec;
    d.process_event("a", Event::Exited { id: exec, code: Some(0), signal: None });
    let list = ask(&mut d, "terminals.list", json!({"session":"a"}));
    assert_eq!(list["result"]["terminals"].as_array().unwrap().len(), 2);
    let again = create(&mut d, &received, json!({"session":"a","name":"app"}));
    assert_eq!(again["id"], "t4");
}

#[test]
fn creation_refuses_bad_names_sizes_programs_and_a_seventeenth_terminal() {
    let _no_forks = unix::no_forks();
    let (mut d, _events, received) = goblin();
    let refused = |d: &mut Controller, params: Value| {
        let reply = ask(d, "terminals.create", params);
        assert!(received.lock().unwrap().is_empty(), "{reply}");
        reply["error"].clone()
    };
    for name in ["t1", "t22", "T", "Name", "1a", "9a", "", "a b", "has space", "x".repeat(33).as_str()] {
        let error = refused(&mut d, json!({"session":"a","name":name}));
        assert_eq!(error["code"], -32602, "{name}");
    }
    for (rows, cols) in [(0, 80), (24, 0), (501, 80), (24, 501)] {
        let error = refused(&mut d, json!({"session":"a","rows":rows,"cols":cols}));
        assert_eq!(error["code"], -32602, "{rows}x{cols}");
    }
    for params in [
        json!({"session":"a","argv":[]}),
        json!({"session":"a","cwd":"relative"}),
        json!({"session":"a","env":[["1BAD","x"]]}),
        json!({"session":"a","unknown":1}),
    ] {
        assert_eq!(refused(&mut d, params)["code"], -32602);
    }
    assert_eq!(refused(&mut d, json!({"session":"nobody"}))["code"], -32004);
}

#[test]
fn names_are_unique_and_a_goblin_has_sixteen_terminals() {
    let _no_forks = unix::no_forks();
    let (mut d, _events, received) = goblin();
    create(&mut d, &received, json!({"session":"a","name":"app"}));
    let error = ask(&mut d, "terminals.create", json!({"session":"a","name":"app"}));
    assert_eq!(error["error"]["code"], -32009);
    assert_eq!(error["error"]["message"], "a terminal named app exists");
    for _ in 1..16 {
        create(&mut d, &received, json!({"session":"a"}));
    }
    let error = ask(&mut d, "terminals.create", json!({"session":"a"}));
    assert_eq!(error["error"]["code"], -32010);
    assert!(error["error"]["message"].as_str().unwrap().contains("16"));
    assert!(received.lock().unwrap().is_empty());
    // One ends: there is room again, and IDs still count up.
    let exec = d.sessions["a"].terminals.entries[0].exec;
    d.process_event("a", Event::Exited { id: exec, code: Some(0), signal: None });
    let next = create(&mut d, &received, json!({"session":"a"}));
    assert_eq!(next["id"], "t17");
}

#[test]
fn every_goblins_terminals_are_its_own() {
    let _no_forks = unix::no_forks();
    let (mut d, _events, received) = goblin();
    live(&mut d, "b");
    capable(&mut d, "b", Support::linux());
    let (runtime, _other_events, other) = goblins_runtime::Fake::new();
    let b = d.sessions.get_mut("b").unwrap();
    b.runtime = Some(Box::new(runtime));
    b.init = true;
    let mine = create(&mut d, &received, json!({"session":"a","name":"mine"}));
    assert_eq!(mine["id"], "t1");
    // Another goblin has none of them, and their names mean nothing there.
    let list = ask(&mut d, "terminals.list", json!({"session":"b"}));
    assert_eq!(list["result"]["terminals"], json!([]));
    let read = ask(&mut d, "terminals.read", json!({"session":"b","terminal":"mine"}));
    assert_eq!(read["error"]["code"], -32004, "{read}");
    assert!(other.lock().unwrap().is_empty());
    // The same name may exist in each, with IDs of its own.
    let (mut c, _peer) = connection(Role::Host);
    c.id = d.number();
    c.initialized = true;
    call(&mut d, &mut c, "terminals.create", json!({"session":"b","name":"mine"}));
    d.connections.push(c);
    let Command::Spawn { id, .. } = take(&other) else {
        panic!("expected a spawn")
    };
    d.process_event("b", Event::Spawned { id, pid: 7 });
    let mut c = d.connections.pop().unwrap();
    let theirs = frame(&mut c)["result"].clone();
    assert_eq!((theirs["id"].as_str(), theirs["name"].as_str()), (Some("t1"), Some("mine")));
    let list = ask(&mut d, "terminals.list", json!({"session":"a"}));
    assert_eq!(list["result"]["terminals"].as_array().unwrap().len(), 1);
}

#[test]
fn a_goblin_that_refuses_exec_refuses_terminals() {
    let _no_forks = unix::no_forks();
    let (mut d, _events, received) = goblin();
    let mut support = Support::linux();
    support.0.remove(&goblins_runtime::policy::Feature::Exec);
    d.sessions.get_mut("a").unwrap().support = support;
    let reply = ask(&mut d, "terminals.create", json!({"session":"a"}));
    assert!(reply.get("error").is_some(), "{reply}");
    assert!(received.lock().unwrap().is_empty());
}

#[test]
fn a_program_that_cannot_start_fails_the_creation_and_leaves_no_terminal() {
    let _no_forks = unix::no_forks();
    let (mut d, _events, received) = goblin();
    let (mut c, _peer) = connection(Role::Host);
    c.id = d.number();
    c.initialized = true;
    call(&mut d, &mut c, "terminals.create", json!({"session":"a","argv":["nothing"]}));
    d.connections.push(c);
    let Command::Spawn { id, .. } = take(&received) else {
        panic!()
    };
    d.process_event(
        "a",
        Event::SpawnFailed { id, code: Some(127), message: "not found".into() },
    );
    let mut c = d.connections.pop().unwrap();
    let reply = frame(&mut c);
    assert_eq!(reply["error"]["message"], "not found");
    assert!(d.sessions["a"].terminals.is_empty());
    assert!(d.sessions["a"].record.terminals.is_empty());
}

/// Make a call on terminal `t1` and return the supervisor's request and
/// call. The connection waits in the daemon for the answer.
fn pending(
    d: &mut Controller,
    received: &Received,
    method: &str,
    params: Value,
) -> (u64, TerminalCall) {
    let (mut c, _peer) = connection(Role::Host);
    c.id = d.number();
    c.initialized = true;
    call(d, &mut c, method, params);
    assert!(c.output.is_empty(), "the answer waits for the supervisor");
    let Command::TerminalCall { request, terminal, call } = take(received) else {
        panic!("expected a terminal call")
    };
    assert_eq!(terminal, d.sessions["a"].terminals.entries[0].exec);
    d.connections.push(c);
    (request, call)
}
/// The supervisor answers `request`; the frame the waiting connection got.
fn answer(d: &mut Controller, request: u64, result: std::result::Result<Value, String>) -> Value {
    d.process_event("a", Event::TerminalResult { request, result });
    let mut c = d.connections.pop().unwrap();
    assert!(c.waiting.is_none());
    frame(&mut c)
}

#[test]
fn read_send_and_wait_are_answered_by_the_supervisor() {
    let _no_forks = unix::no_forks();
    let (mut d, _events, received) = goblin();
    create(&mut d, &received, json!({"session":"a","name":"app"}));
    // read: visible by default, by ID or name.
    for designator in ["t1", "app"] {
        let (request, call) = pending(&mut d, &received, "terminals.read", json!({"session":"a","terminal":designator}));
        assert_eq!(call, TerminalCall::Read { source: Source::Visible, lines: None });
        let reply = answer(&mut d, request, Ok(json!({"text":"hi","alternate_screen":false,"rows":40,"cols":120})));
        assert_eq!(reply["id"], 5);
        assert_eq!(reply["result"]["text"], "hi");
    }
    let (request, call) = pending(&mut d, &received, "terminals.read", json!({"session":"a","terminal":"t1","source":"recent","lines":20}));
    assert_eq!(call, TerminalCall::Read { source: Source::Recent, lines: Some(20) });
    answer(&mut d, request, Ok(json!({})));
    // send: text and keys.
    let (request, call) = pending(&mut d, &received, "terminals.send", json!({"session":"a","terminal":"t1","text":"ls","keys":["enter"]}));
    assert_eq!(call, TerminalCall::Send { text: Some("ls".into()), keys: vec!["enter".into()] });
    let reply = answer(&mut d, request, Ok(json!({"accepted":true})));
    assert_eq!(reply["result"]["accepted"], true);
    // wait: one condition, the default timeout, and errors from the supervisor.
    let (request, call) = pending(&mut d, &received, "terminals.wait", json!({"session":"a","terminal":"t1","regex":"^done$"}));
    assert_eq!(call, TerminalCall::Wait { condition: Condition::Regex("^done$".into()), timeout: 30_000, lines: None });
    let reply = answer(&mut d, request, Ok(json!({"status":"matched","line":"done"})));
    assert_eq!(reply["result"]["status"], "matched");
    let (request, call) = pending(&mut d, &received, "terminals.wait", json!({"session":"a","terminal":"t1","exit":true,"timeout":5,"lines":7}));
    assert_eq!(call, TerminalCall::Wait { condition: Condition::Exit, timeout: 5, lines: Some(7) });
    let reply = answer(&mut d, request, Err("the terminal's program has ended".into()));
    assert_eq!(reply["error"]["code"], -32009);
    let (request, call) = pending(&mut d, &received, "terminals.wait", json!({"session":"a","terminal":"t1","match":"ok"}));
    assert_eq!(call, TerminalCall::Wait { condition: Condition::Match("ok".into()), timeout: 30_000, lines: None });
    answer(&mut d, request, Ok(json!({"status":"timeout"})));
}

#[test]
fn calls_with_bad_parameters_never_reach_the_supervisor() {
    let _no_forks = unix::no_forks();
    let (mut d, _events, received) = goblin();
    create(&mut d, &received, json!({"session":"a","name":"app"}));
    let long = "x".repeat(terminal::MAX_TEXT + 1);
    let many: Vec<_> = (0..65).map(|_| "enter").collect();
    for (method, params, code) in [
        ("terminals.read", json!({"session":"a","terminal":"t1","source":"all"}), -32602),
        ("terminals.read", json!({"session":"a","terminal":"t1","lines":5001}), -32602),
        ("terminals.read", json!({"session":"a","terminal":"t1","lines":0}), -32602),
        ("terminals.send", json!({"session":"a","terminal":"t1","text":long}), -32602),
        ("terminals.send", json!({"session":"a","terminal":"t1","keys":["enter","nope"]}), -32602),
        ("terminals.send", json!({"session":"a","terminal":"t1","keys":many}), -32602),
        ("terminals.send", json!({"session":"a","terminal":"t1"}), -32602),
        ("terminals.wait", json!({"session":"a","terminal":"t1"}), -32602),
        ("terminals.wait", json!({"session":"a","terminal":"t1","match":"a","exit":true}), -32602),
        ("terminals.wait", json!({"session":"a","terminal":"t1","match":"a","regex":"b"}), -32602),
        ("terminals.wait", json!({"session":"a","terminal":"t1","exit":false}), -32602),
        ("terminals.wait", json!({"session":"a","terminal":"t1","regex":"("}), -32602),
        ("terminals.wait", json!({"session":"a","terminal":"t1","exit":true,"timeout":3_600_001u64}), -32602),
        ("terminals.read", json!({"session":"a","terminal":"t9"}), -32004),
        ("terminals.read", json!({"session":"a","terminal":"app2"}), -32004),
        ("terminals.resize", json!({"session":"a","terminal":"t1","rows":0,"cols":80}), -32602),
        ("terminals.resize", json!({"session":"a","terminal":"t1","rows":24,"cols":501}), -32602),
        ("terminals.nonsense", json!({"session":"a"}), -32601),
    ] {
        let reply = ask(&mut d, method, params.clone());
        assert_eq!(reply["error"]["code"], code, "{method} {params}: {reply}");
        assert!(received.lock().unwrap().is_empty(), "{method} {params}");
    }
}

#[test]
fn a_goblin_has_sixteen_waits_and_a_dropped_connection_cancels_its_wait() {
    let _no_forks = unix::no_forks();
    let (mut d, _events, received) = goblin();
    create(&mut d, &received, json!({"session":"a","name":"app"}));
    let mut requests = vec![];
    for n in 0..16 {
        let (mut c, _peer) = connection(Role::Host);
        c.id = 100 + n;
        c.initialized = true;
        call(&mut d, &mut c, "terminals.wait", json!({"session":"a","terminal":"t1","exit":true}));
        let Command::TerminalCall { request, .. } = take(&received) else {
            panic!()
        };
        requests.push(request);
        d.connections.push(c);
    }
    let error = ask(&mut d, "terminals.wait", json!({"session":"a","terminal":"t1","exit":true}));
    assert_eq!(error["error"]["code"], -32010);
    assert!(received.lock().unwrap().is_empty());
    // Reads are not waits: they are not counted.
    let (mut c, _peer) = connection(Role::Host);
    c.id = 200;
    c.initialized = true;
    call(&mut d, &mut c, "terminals.read", json!({"session":"a","terminal":"t1"}));
    assert!(matches!(take(&received), Command::TerminalCall { .. }));
    // The caller of one wait goes away: the supervisor stops it, no more.
    d.withdraw(103);
    assert!(matches!(take(&received), Command::TerminalCancel { request } if request == requests[3]));
    assert!(received.lock().unwrap().is_empty());
    // Its slot is free again once nothing waits on it.
    d.process_event("a", Event::TerminalResult { request: requests[3], result: Ok(json!({})) });
    let (mut c, _peer) = connection(Role::Host);
    c.id = 300;
    c.initialized = true;
    call(&mut d, &mut c, "terminals.wait", json!({"session":"a","terminal":"t1","exit":true}));
    assert!(matches!(take(&received), Command::TerminalCall { .. }));
}

#[test]
fn close_detach_and_resize_act_on_the_terminal() {
    let _no_forks = unix::no_forks();
    let (mut d, _events, received) = goblin();
    create(&mut d, &received, json!({"session":"a","name":"app"}));
    let exec = d.sessions["a"].terminals.entries[0].exec;
    // Close cancels the process group, as a withdrawn exec is cancelled; the
    // terminal stays until its program has ended.
    let reply = ask(&mut d, "terminals.close", json!({"session":"a","terminal":"app"}));
    assert_eq!(reply["result"]["accepted"], true);
    assert!(matches!(take(&received), Command::CancelProcess { id, everything: false } if id == exec));
    assert_eq!(d.sessions["a"].record.terminals.len(), 1);
    // Resize reaches the terminal and the record.
    let reply = ask(&mut d, "terminals.resize", json!({"session":"a","terminal":"t1","rows":30,"cols":90}));
    assert_eq!(reply["result"]["accepted"], true);
    assert!(matches!(take(&received), Command::Resize { terminal: TerminalId::Process(id), rows: 30, cols: 90 } if id == exec));
    assert_eq!((d.sessions["a"].record.terminals[0].rows, d.sessions["a"].record.terminals[0].cols), (30, 90));
    // Detach needs a frontend.
    let reply = ask(&mut d, "terminals.detach", json!({"session":"a","terminal":"t1"}));
    assert_eq!(reply["error"]["message"], "terminal is not attached");
    d.sessions.get_mut("a").unwrap().terminals.entries[0].record.attached = true;
    let reply = ask(&mut d, "terminals.detach", json!({"session":"a","terminal":"t1"}));
    assert_eq!(reply["result"]["accepted"], true);
    assert!(matches!(take(&received), Command::Detach { terminal: TerminalId::Process(id) } if id == exec));
    assert!(!d.sessions["a"].record.terminals[0].attached);
}

#[test]
fn a_stopping_goblin_ends_its_terminals_and_the_calls_waiting_on_them() {
    let _no_forks = unix::no_forks();
    let (mut d, _events, received) = goblin();
    create(&mut d, &received, json!({"session":"a","name":"app"}));
    let (mut c, _peer) = connection(Role::Host);
    c.id = d.number();
    c.initialized = true;
    call(&mut d, &mut c, "terminals.wait", json!({"session":"a","terminal":"t1","exit":true}));
    d.connections.push(c);
    d.process_event("a", Event::InitLost);
    let mut c = d.connections.pop().unwrap();
    assert_eq!(frame(&mut c)["error"]["code"], -32009);
    assert!(d.sessions["a"].record.terminals.is_empty());
    assert!(d.sessions["a"].terminals.is_empty());
    assert!(d.terminal_calls.is_empty());
}

#[test]
fn the_snapshot_follows_attachment_and_size() {
    let _no_forks = unix::no_forks();
    let (mut d, _events, received) = goblin();
    create(&mut d, &received, json!({"session":"a","name":"app"}));
    let exec = d.sessions["a"].terminals.entries[0].exec;
    let status = |attached| TerminalStatus { connected: attached, attached, ..Default::default() };
    d.process_event("a", Event::Terminal { terminal: TerminalId::Process(exec), status: status(true), error: None });
    assert!(d.snapshot().sessions[0].terminals[0].attached);
    d.process_event("a", Event::Terminal { terminal: TerminalId::Process(exec), status: status(false), error: None });
    assert!(!d.snapshot().sessions[0].terminals[0].attached);
    let snapshot = serde_json::to_value(d.snapshot()).unwrap();
    assert_eq!(snapshot["sessions"][0]["terminals"][0]["id"], "t1");
}

fn hello(socket: &Path, terminal: Option<&str>, rows: u16, cols: u16) -> UnixStream {
    let mut frontend = UnixStream::connect(socket).unwrap();
    let mut params = json!({"version":1,"rows":rows,"cols":cols});
    if let Some(terminal) = terminal {
        params["terminal"] = json!(terminal);
    }
    frontend
        .write_all(&rpc::encode(&json!({"jsonrpc":"2.0","method":"attach.hello","params":params})).unwrap())
        .unwrap();
    frontend
}
fn step(d: &mut Controller) {
    d.tick().unwrap();
    std::thread::sleep(std::time::Duration::from_millis(5));
}

#[test]
fn a_hello_naming_a_terminal_attaches_it_beside_the_goblins_own_attachment() {
    let _no_forks = unix::no_forks();
    let (mut d, _events, received) = goblin();
    create(&mut d, &received, json!({"session":"a","name":"app"}));
    let exec = d.sessions["a"].terminals.entries[0].exec;
    let socket = d.sessions["a"].directory.join("attach.sock");
    // The goblin has attach handlers (`capable`), yet a terminal attaches.
    let mut frontend = hello(&socket, Some("app"), 30, 100);
    for _ in 0..50 {
        step(&mut d);
        if !received.lock().unwrap().is_empty() {
            break;
        }
    }
    let Command::Attach { terminal, rows, cols, .. } = take(&received) else {
        panic!("expected an attach")
    };
    assert_eq!((terminal, rows, cols), (TerminalId::Process(exec), 30, 100));
    assert!(d.sessions["a"].record.terminals[0].attached);
    assert_eq!((d.sessions["a"].record.terminals[0].rows, d.sessions["a"].record.terminals[0].cols), (30, 100));
    assert!(!d.sessions["a"].record.terminal_attached, "the goblin's own attachment is untouched");
    // A second frontend for it is refused; one for t1 by ID too.
    for designator in ["app", "t1"] {
        let mut second = hello(&socket, Some(designator), 30, 100);
        for _ in 0..50 {
            step(&mut d);
            if unix::readable(second.as_raw_fd(), 0).unwrap() {
                break;
            }
        }
        assert_eq!(rpc::read(&mut second).unwrap()["error"]["message"], "terminal is already attached");
    }
    assert!(received.lock().unwrap().is_empty());
    // The supervisor reports it released.
    d.process_event(
        "a",
        Event::Terminal { terminal: TerminalId::Process(exec), status: TerminalStatus::default(), error: None },
    );
    assert!(!d.sessions["a"].record.terminals[0].attached);
    drop(frontend.try_clone());
    frontend.shutdown(std::net::Shutdown::Both).unwrap();
}

#[test]
fn a_hello_for_a_terminal_that_cannot_be_attached_is_refused_at_once() {
    let _no_forks = unix::no_forks();
    let (mut d, _events, received) = goblin();
    create(&mut d, &received, json!({"session":"a","name":"app"}));
    let socket = d.sessions["a"].directory.join("attach.sock");
    for (designator, rows, cols, message) in [
        ("t9", 24, 80, "no terminal t9"),
        ("other", 24, 80, "no terminal other"),
        ("app", 24, 501, "terminal sizes are 1 to 500 rows and columns"),
        ("app", 501, 80, "terminal sizes are 1 to 500 rows and columns"),
    ] {
        let mut frontend = hello(&socket, Some(designator), rows, cols);
        for _ in 0..50 {
            step(&mut d);
            if unix::readable(frontend.as_raw_fd(), 0).unwrap() {
                break;
            }
        }
        assert_eq!(rpc::read(&mut frontend).unwrap()["error"]["message"], message);
    }
    // A goblin that refuses exec refuses the attachment too.
    let mut support = Support::linux();
    support.0.remove(&goblins_runtime::policy::Feature::Exec);
    d.sessions.get_mut("a").unwrap().support = support;
    let mut frontend = hello(&socket, Some("app"), 24, 80);
    for _ in 0..50 {
        step(&mut d);
        if unix::readable(frontend.as_raw_fd(), 0).unwrap() {
            break;
        }
    }
    assert!(rpc::read(&mut frontend).unwrap()["error"]["message"].is_string());
    assert!(received.lock().unwrap().is_empty());
}

// The sandbox endpoint: the same methods on the caller's own goblin.

/// A sandbox connection of goblin `a` with a call made and its answer given back.
fn sandbox_ask(d: &mut Controller, method: &str, params: Value) -> Value {
    let (mut c, _peer) = connection(Role::Sandbox("a".into()));
    c.initialized = true;
    call(d, &mut c, method, params);
    frame(&mut c)
}
/// Create a terminal on the sandbox endpoint, with a started program.
fn agent_create(d: &mut Controller, received: &Received, params: Value) -> Value {
    let (mut c, _peer) = connection(Role::Sandbox("a".into()));
    c.id = d.number();
    c.initialized = true;
    call(d, &mut c, "terminals.create", params);
    assert!(c.output.is_empty(), "the answer waits for the program to start");
    d.connections.push(c);
    let Command::Spawn { id, .. } = take(received) else {
        panic!("expected a spawn")
    };
    d.process_event("a", Event::Spawned { id, pid: 42 });
    let mut c = d.connections.pop().unwrap();
    frame(&mut c)["result"].clone()
}

#[test]
fn a_goblin_creates_and_lists_its_own_terminals_with_origin_agent() {
    let _no_forks = unix::no_forks();
    let (mut d, _events, received) = goblin();
    live(&mut d, "b");
    let host = create(&mut d, &received, json!({"session":"a","name":"mine"}));
    let agent = agent_create(&mut d, &received, json!({"argv":["/bin/sleep","9"],"name":"srv"}));
    assert_eq!((host["origin"].as_str(), agent["origin"].as_str()), (Some("host"), Some("agent")));
    assert_eq!((agent["id"].as_str(), agent["label"].as_str()), (Some("t2"), Some("srv")));
    // The sandbox lists all of its goblin's terminals, of both origins.
    let list = sandbox_ask(&mut d, "terminals.list", json!({}));
    let origins: Vec<_> = list["result"]["terminals"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["origin"].as_str().unwrap().to_string())
        .collect();
    assert_eq!(origins, ["host", "agent"]);
    // Absent params are no params.
    let list = sandbox_ask(&mut d, "terminals.list", Value::Null);
    assert_eq!(list["result"]["terminals"].as_array().unwrap().len(), 2);
    // The host sees the same, and the snapshot shows the origin.
    assert_eq!(d.sessions["a"].record.terminals[1].origin, "agent");
    // A goblin that is not the caller has none of the caller's.
    assert!(d.sessions["b"].record.terminals.is_empty());
}

#[test]
fn the_sandbox_endpoint_takes_no_session_and_reaches_no_other_goblin() {
    let _no_forks = unix::no_forks();
    let (mut d, _events, received) = goblin();
    live(&mut d, "b");
    let b = d.sessions.get_mut("b").unwrap();
    b.init = true;
    let (runtime, _events, other) = goblins_runtime::Fake::new();
    b.runtime = Some(Box::new(runtime));
    for (method, params) in [
        ("terminals.create", json!({"session":"b"})),
        ("terminals.list", json!({"session":"a"})),
        ("terminals.read", json!({"session":"b","terminal":"t1"})),
        ("terminals.send", json!({"session":"b","terminal":"t1","text":"x"})),
        ("terminals.wait", json!({"session":"b","terminal":"t1","exit":true})),
        ("terminals.close", json!({"session":"b","terminal":"t1"})),
    ] {
        let reply = sandbox_ask(&mut d, method, params);
        assert_eq!(reply["error"]["code"], -32602, "{method}: {reply}");
        assert!(
            reply["error"]["message"].as_str().unwrap().contains("no session"),
            "{reply}"
        );
    }
    // Names of another goblin's terminals mean nothing here.
    let (mut c, _peer) = connection(Role::Host);
    c.initialized = true;
    c.id = d.number();
    call(&mut d, &mut c, "terminals.create", json!({"session":"b","name":"theirs"}));
    d.connections.push(c);
    let Command::Spawn { id, .. } = take(&other) else {
        panic!()
    };
    d.process_event("b", Event::Spawned { id, pid: 7 });
    assert_eq!(d.sessions["b"].record.terminals.len(), 1);
    let reply = sandbox_ask(&mut d, "terminals.read", json!({"terminal":"theirs"}));
    assert_eq!(reply["error"]["code"], -32004, "{reply}");
    let reply = sandbox_ask(&mut d, "terminals.read", json!({"terminal":"t1"}));
    assert_eq!(reply["error"]["code"], -32004, "{reply}");
    assert!(received.lock().unwrap().is_empty());
}

#[test]
fn a_goblin_closes_only_the_terminals_its_agent_created() {
    let _no_forks = unix::no_forks();
    let (mut d, _events, received) = goblin();
    create(&mut d, &received, json!({"session":"a","name":"human"}));
    agent_create(&mut d, &received, json!({"name":"mine"}));
    let refused = sandbox_ask(&mut d, "terminals.close", json!({"terminal":"human"}));
    assert_eq!(refused["error"]["code"], -32009, "{refused}");
    let message = refused["error"]["message"].as_str().unwrap();
    assert!(message.contains("only the human can close it"), "{message}");
    assert!(received.lock().unwrap().is_empty(), "nothing was cancelled");
    let accepted = sandbox_ask(&mut d, "terminals.close", json!({"terminal":"mine"}));
    assert_eq!(accepted["result"], json!({"accepted":true}));
    let Command::CancelProcess { id, everything } = take(&received) else {
        panic!("expected a cancel")
    };
    assert_eq!(id, d.sessions["a"].terminals.entries[1].exec);
    assert!(!everything);
    // The human can close either.
    let reply = ask(&mut d, "terminals.close", json!({"session":"a","terminal":"human"}));
    assert_eq!(reply["result"], json!({"accepted":true}));
    assert!(matches!(take(&received), Command::CancelProcess { .. }));
}

#[test]
fn a_goblin_cannot_attach_detach_or_resize_a_terminal() {
    let _no_forks = unix::no_forks();
    let (mut d, _events, received) = goblin();
    create(&mut d, &received, json!({"session":"a","name":"app"}));
    for (method, params) in [
        ("terminals.attach", json!({"terminal":"app"})),
        ("terminals.detach", json!({"terminal":"app"})),
        ("terminals.resize", json!({"terminal":"app","rows":30,"cols":100})),
    ] {
        let reply = sandbox_ask(&mut d, method, params);
        assert_eq!(reply["error"]["code"], -32601, "{method}: {reply}");
    }
    assert!(received.lock().unwrap().is_empty());
}

#[test]
fn a_goblin_reads_sends_and_waits_on_all_of_its_terminals() {
    let _no_forks = unix::no_forks();
    let (mut d, _events, received) = goblin();
    create(&mut d, &received, json!({"session":"a","name":"human"}));
    let (mut c, _peer) = connection(Role::Sandbox("a".into()));
    c.id = d.number();
    c.initialized = true;
    call(&mut d, &mut c, "terminals.read", json!({"terminal":"human","source":"recent","lines":3}));
    d.connections.push(c);
    let Command::TerminalCall { request, call: TerminalCall::Read { source, lines }, .. } = take(&received) else {
        panic!("expected a read")
    };
    assert_eq!((source, lines), (Source::Recent, Some(3)));
    d.process_event(
        "a",
        Event::TerminalResult {
            request,
            result: Ok(json!({"text":"hello","alternate_screen":false,"rows":40,"cols":120})),
        },
    );
    let mut c = d.connections.pop().unwrap();
    assert_eq!(frame(&mut c)["result"]["text"], "hello");
    // Keys are typed into a terminal of either origin; a wait is limited like the host's.
    let (mut c, _peer) = connection(Role::Sandbox("a".into()));
    c.id = d.number();
    c.initialized = true;
    call(&mut d, &mut c, "terminals.send", json!({"terminal":"t1","text":"ls","keys":["enter"]}));
    d.connections.push(c);
    assert!(matches!(
        take(&received),
        Command::TerminalCall { call: TerminalCall::Send { .. }, .. }
    ));
    let bad = sandbox_ask(&mut d, "terminals.wait", json!({"terminal":"human"}));
    assert_eq!(bad["error"]["code"], -32602, "{bad}");
}

#[test]
fn only_a_goblins_own_agent_can_ask_for_an_exit_notice() {
    let _no_forks = unix::no_forks();
    let (mut d, _events, received) = goblin();
    let reply = ask(&mut d, "terminals.create", json!({"session":"a","notify":true}));
    assert_eq!(reply["error"]["code"], -32602, "{reply}");
    assert!(received.lock().unwrap().is_empty());
}

/// Create an agent terminal that asked for a notice; returns the request
/// the supervisor is to report its exit under.
fn notified(d: &mut Controller, received: &Received, params: Value) -> (Value, u64) {
    let (mut c, _peer) = connection(Role::Sandbox("a".into()));
    c.id = d.number();
    c.initialized = true;
    call(d, &mut c, "terminals.create", params);
    d.connections.push(c);
    let Command::Spawn { id, kind: SpawnKind::Terminal { exit_report: Some(request) }, .. } =
        take(received)
    else {
        panic!("expected a spawn that asks for the exit report")
    };
    d.process_event("a", Event::Spawned { id, pid: 42 });
    let mut c = d.connections.pop().unwrap();
    (frame(&mut c)["result"].clone(), request)
}

#[test]
fn a_terminal_that_asked_for_a_notice_reports_its_exit_once_with_the_last_lines() {
    let _no_forks = unix::no_forks();
    let (mut d, _events, received) = goblin();
    // Without notify the supervisor is asked for no report.
    let (mut c, _peer) = connection(Role::Sandbox("a".into()));
    c.id = d.number();
    c.initialized = true;
    call(&mut d, &mut c, "terminals.create", json!({"name":"quiet"}));
    let Command::Spawn { kind, .. } = take(&received) else {
        panic!()
    };
    assert_eq!(kind, SpawnKind::Terminal { exit_report: None });
    let (record, request) = notified(&mut d, &received, json!({"name":"build","notify":true}));
    assert_eq!(record["origin"], "agent");
    // Nothing is asked of the supervisor later, and nothing is a call anyone waits for.
    assert!(received.lock().unwrap().is_empty());
    assert_eq!(d.terminal_calls.len(), 1);
    assert!(d.operation_outbox.is_empty());
    // The program exits.
    d.process_event(
        "a",
        Event::TerminalResult {
            request,
            result: Ok(json!({"status":"exited","code":3,"signal":null,"text":"compiling\nerror: boom"})),
        },
    );
    assert!(d.terminal_calls.is_empty());
    let exec = d.sessions["a"].terminals.entries.last().unwrap().exec;
    d.process_event("a", Event::Exited { id: exec, code: Some(3), signal: None });
    assert_eq!(d.operation_outbox.len(), 1);
    let event = d.operation_outbox.pop_front().unwrap();
    assert_eq!((event.session.as_str(), event.actor.as_str()), ("a", "goblins"));
    assert_eq!(event.request, "a-t2", "the goblin and the terminal");
    let crate::operation::Change::Completed { kind, subject, status, message, notify } = event.change
    else {
        panic!("expected an outcome")
    };
    assert_eq!((kind.as_str(), subject.as_str(), status.as_str()), ("terminal", "build", "exited"));
    assert_eq!(message.as_deref(), Some("exit code 3\ncompiling\nerror: boom"));
    assert!(notify);
    assert!(received.lock().unwrap().is_empty());
    assert!(d.operation_outbox.is_empty());
    // The notice text names the terminal and the status, and ends with the screen.
    let text = crate::operation::notice("r", "terminal", "build", "exited", Some("exit code 3\nboom"));
    assert!(text.starts_with("Goblins operation r: terminal build exited: exit code 3\nboom\n\nThis notice"), "{text}");
}

#[test]
fn closing_a_terminal_or_stopping_its_goblin_is_no_exit_notice() {
    let _no_forks = unix::no_forks();
    let (mut d, _events, received) = goblin();
    let (_, first) = notified(&mut d, &received, json!({"name":"one","notify":true}));
    let (_, second) = notified(&mut d, &received, json!({"name":"two","notify":true}));
    // Closed: the program exits because it was asked to.
    let reply = sandbox_ask(&mut d, "terminals.close", json!({"terminal":"one"}));
    assert_eq!(reply["result"], json!({"accepted":true}));
    assert!(matches!(take(&received), Command::CancelProcess { .. }));
    d.process_event(
        "a",
        Event::TerminalResult {
            request: first,
            result: Ok(json!({"status":"exited","code":null,"signal":15,"text":""})),
        },
    );
    assert!(d.operation_outbox.is_empty());
    // The goblin stops: its terminals end with it, and the report is not awaited.
    d.stop("a", None).unwrap();
    assert!(d.terminal_calls.is_empty());
    assert!(d.sessions["a"].record.terminals.is_empty());
    assert!(d.sessions["a"].terminals.is_empty());
    d.process_event(
        "a",
        Event::TerminalResult {
            request: second,
            result: Ok(json!({"status":"exited","code":0,"signal":null,"text":"bye"})),
        },
    );
    assert!(d.operation_outbox.is_empty());
}

#[test]
fn a_program_that_cannot_start_leaves_no_report_to_wait_for() {
    let _no_forks = unix::no_forks();
    let (mut d, _events, received) = goblin();
    let (mut c, _peer) = connection(Role::Sandbox("a".into()));
    c.id = d.number();
    c.initialized = true;
    call(&mut d, &mut c, "terminals.create", json!({"argv":["nothing"],"notify":true}));
    d.connections.push(c);
    let Command::Spawn { id, .. } = take(&received) else {
        panic!()
    };
    assert_eq!(d.terminal_calls.len(), 1);
    d.process_event(
        "a",
        Event::SpawnFailed { id, code: Some(2), message: "no such file".into() },
    );
    assert!(d.terminal_calls.is_empty());
    assert!(d.sessions["a"].record.terminals.is_empty());
}

#[test]
fn an_exit_notice_says_how_the_program_ended_and_cuts_its_lines_to_4_kib() {
    assert_eq!(super::super::terminals::notice_message(Some(0), None, ""), "exit code 0");
    assert_eq!(
        super::super::terminals::notice_message(None, Some(9), "a\nb\n\n"),
        "killed by signal 9\na\nb"
    );
    assert_eq!(super::super::terminals::notice_message(None, None, "x"), "no exit status\nx");
    // The newest lines stay, whole characters only, within 4 KiB.
    let text = (0..2000).map(|n| format!("line {n} é")).collect::<Vec<_>>().join("\n");
    let message = super::super::terminals::notice_message(Some(1), None, &text);
    assert!(message.len() <= 4096, "{}", message.len());
    assert!(message.starts_with("exit code 1\n…"));
    assert!(message.ends_with("line 1999 é"));
}

#[test]
fn generated_lifecycle_contracts_keep_host_positional_and_sandbox_object_forms() {
    let (mut d, _events, received) = goblin();
    let record = create(
        &mut d,
        &received,
        json!(["a", ["sh"], "/work", [["A", "b"]], "app", 30, 90, false]),
    );
    assert_eq!(record["name"], "app");
    assert_eq!((record["rows"].as_u64(), record["cols"].as_u64()), (Some(30), Some(90)));
    assert_eq!(record["origin"], "host");
    let list = ask(&mut d, "terminals.list", json!(["a"]));
    assert_eq!(list["result"]["terminals"], json!([record.clone()]));
    let member = sandbox_ask(&mut d, "terminals.list", Value::Null);
    assert_eq!(member["result"], list["result"]);
    for method in ["terminals.create", "terminals.list", "terminals.close"] {
        let refused = sandbox_ask(&mut d, method, json!([]));
        assert_eq!(refused["error"]["code"], -32602);
        assert_eq!(refused["error"]["message"], "params must be an object");
    }
    let closed = ask(&mut d, "terminals.close", json!(["a", "app"]));
    assert_eq!(closed["result"], json!({"accepted": true}));
    assert!(matches!(take(&received), Command::CancelProcess { everything: false, .. }));
    // Close acceptance precedes process exit, so the terminal remains listed.
    assert_eq!(ask(&mut d, "terminals.list", json!(["a"]))["result"], list["result"]);
}

#[test]
fn terminal_list_keeps_retained_ids_and_releases_agent_name_selectors() {
    let (mut d, _events, _) = goblin();
    let (runtime, _events, received) = goblins_runtime::Fake::new();
    let released = runtime.release_flag();
    let a = d.sessions.get_mut("a").unwrap();
    a.runtime = Some(Box::new(runtime));
    a.record.agent_name = "friendly".into();
    create(&mut d, &received, json!({"session":"a"}));
    d.end_terminals("a", "stopped");
    d.sessions.get_mut("a").unwrap().record.state = "stopped".into();
    // Name lookup tracks retained runtime ownership, rather than state text.
    assert_eq!(ask(&mut d, "terminals.list", json!(["friendly"]))["result"], json!({"terminals":[]}));
    released.store(true, std::sync::atomic::Ordering::SeqCst);
    assert_eq!(ask(&mut d, "terminals.list", json!(["a"]))["result"], json!({"terminals":[]}));
    assert_eq!(ask(&mut d, "terminals.list", json!(["friendly"]))["error"]["code"], -32004);
    assert_eq!(ask(&mut d, "terminals.list", json!(["absent"]))["error"]["code"], -32004);
    live(&mut d, "b");
    d.sessions.get_mut("b").unwrap().record.agent_name = "friendly".into();
    assert_eq!(d.resolve_session("friendly").unwrap(), "b");
    assert_eq!(ask(&mut d, "terminals.list", json!(["friendly"]))["result"], json!({"terminals":[]}));
}

#[test]
fn generated_io_contract_corpus_keeps_runtime_validation_and_refusal_behavior() {
    let _no_forks = unix::no_forks();
    let fixtures: Value = serde_json::from_str(include_str!("../../../../../libs/api/tests/fixtures.json")).unwrap();
    for case in fixtures["terminal_io_parameter_cases"].as_array().unwrap() {
        let (mut d, _events, received) = goblin();
        create(&mut d, &received, json!({"session":"a"}));
        let method = case["method"].as_str().unwrap();
        if method == "terminals.detach" {
            d.sessions.get_mut("a").unwrap().terminals.entries[0].record.attached = true;
        }
        let role = if case["endpoint"] == "host" { Role::Host } else { Role::Sandbox("a".into()) };
        let (mut c, _peer) = connection(role);
        c.id = d.number();
        c.initialized = true;
        call(&mut d, &mut c, method, case["params"].clone());
        let accepted = c.waiting.is_some() || frame(&mut c).get("result").is_some();
        assert_eq!(accepted, case["runtime"].as_bool().unwrap(), "{case}");
        if !accepted {
            assert!(received.lock().unwrap().is_empty(), "refused input reached runtime: {case}");
        }
    }
}
