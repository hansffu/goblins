use super::*;
use serde_json::json;

fn record(id: usize) -> Command {
    Command::RecordEvent {
        event: "exec-started".into(),
        fields: json!({"exec":id,"$fd":0}),
    }
}

#[test]
fn diagnostic_work_drains_boundedly_with_full_semantic_output_and_stop_drops_the_rest() {
    let _no_forks = unix::no_forks();
    let directory = unix::temp_directory().unwrap();
    let (worker, _work, results) = worker();
    let mut engine = Engine::new(worker, directory.join("host-jobs"));
    for _ in 0..EVENTS {
        engine.report(Event::Mode { server: false });
    }
    engine.send(record(1)).unwrap();
    engine.send(record(2)).unwrap();
    let path = directory.join("events.jsonl");
    assert!(!path.exists());
    engine.collect();
    let lines: Vec<serde_json::Value> = fs::read_to_string(&path)
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    assert_eq!(lines.len(), 1, "one record per bounded collection round");
    assert_eq!(lines[0]["fields"], json!({"exec":1,"$fd":0}));
    assert_eq!(
        engine.events.len(),
        EVENTS,
        "logging consumes no semantic output slot"
    );
    engine.collect();
    let lines: Vec<serde_json::Value> = fs::read_to_string(&path)
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    assert_eq!(lines.len(), 2);
    assert_eq!(lines[1]["fields"]["exec"], 2);
    for id in 0..16 {
        engine.send(record(id)).unwrap();
    }
    engine.send(Command::Stop).unwrap();
    engine.send(record(99)).unwrap();
    assert!(engine.stopping);
    assert_eq!(engine.deferred.dropped_record_events(), 17);
    results.send(Event::Stopped { exit_code: Some(0) }).unwrap();
    let mut events = vec![];
    for _ in 0..4 {
        events.extend(std::iter::from_fn(|| engine.poll()));
        if engine.released() {
            break;
        }
    }
    assert_eq!(
        events
            .iter()
            .filter(|event| matches!(event, Event::Mode { .. }))
            .count(),
        EVENTS
    );
    assert_eq!(
        events
            .iter()
            .filter(|event| matches!(event, Event::Stopped { .. }))
            .count(),
        1
    );
    assert!(engine.released());
    assert_eq!(fs::read_to_string(&path).unwrap().lines().count(), 2);
    drop(engine);
    fs::remove_dir_all(directory).unwrap();
}

#[test]
fn a_full_log_lane_does_not_defer_other_dispatcher_work() {
    let _no_forks = unix::no_forks();
    let directory = unix::temp_directory().unwrap();
    let (worker, _work, _results) = worker();
    let mut engine = Engine::new(worker, directory.join("host-jobs"));
    for id in 0..16 {
        engine.send(record(id)).unwrap();
    }
    engine
        .send(Command::Export {
            member: "probe".into(),
        })
        .unwrap();
    assert_eq!(
        engine.deferred.unsent_requests(),
        0,
        "logs do not occupy dispatcher admission"
    );
    assert!(
        matches!(engine.events.front(), Some(Event::Exported { member, result:Err(_) }) if member=="probe")
    );
    engine.send(Command::Stop).unwrap();
    assert_eq!(engine.deferred.dropped_record_events(), 16);
    drop(engine);
    fs::remove_dir_all(directory).unwrap();
}

#[test]
fn resource_release_disables_logging_even_while_another_lease_keeps_the_directory() {
    let _no_forks = unix::no_forks();
    let root = unix::temp_directory().unwrap();
    fs::write(root.join("anchor"), "keep fixture parent").unwrap();
    let directory = root.join("resources");
    unix::private_directory(&directory).unwrap();
    let held = goblins_runtime::ResourceLease::new(directory.clone()).unwrap();
    let (worker, _work, results) = worker();
    let mut engine = Engine::new(worker, directory.join("host-jobs"));
    engine.send(record(1)).unwrap();
    assert!(engine.poll().is_none());
    let path = directory.join("events.jsonl");
    assert!(path.exists());
    engine.send(record(2)).unwrap();
    results.send(Event::Stopped { exit_code: Some(0) }).unwrap();
    let events: Vec<_> = std::iter::from_fn(|| engine.poll()).collect();
    assert!(
        events
            .iter()
            .any(|event| matches!(event, Event::Stopped { .. }))
    );
    assert!(engine.released() && engine.log_directory.is_none());
    assert_eq!(engine.deferred.dropped_record_events(), 1);
    fs::remove_file(&path).unwrap();
    engine.send(record(3)).unwrap();
    while engine.poll().is_some() {}
    assert!(
        directory.exists() && !path.exists(),
        "retained lease cannot re-enable a released writer"
    );
    fs::remove_dir_all(&directory).unwrap();
    engine.send(record(4)).unwrap();
    while engine.poll().is_some() {}
    assert!(
        !directory.exists(),
        "neither resource directory nor log may be recreated"
    );
    assert_eq!(engine.deferred.dropped_record_events(), 3);
    drop(engine);
    drop(held);
    fs::remove_dir_all(root).unwrap();
}
