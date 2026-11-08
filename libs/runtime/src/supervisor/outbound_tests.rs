use super::outbound::*;
use super::*;
use goblins_unix as unix;
use std::{fs, io::Read, os::fd::OwnedFd, os::unix::net::UnixStream};
fn null() -> Fd {
    OwnedFd::from(fs::File::open("/dev/null").unwrap()).into()
}
fn run(serial: u64) -> Command {
    Command::Run {
        serial,
        app: "app".into(),
    }
}
fn spawn(id: u64, stdio: Stdio) -> Command {
    Command::Spawn {
        id,
        kind: crate::SpawnKind::Exec,
        argv: None,
        cwd: None,
        env: vec![],
        stdio,
    }
}
fn push(commands: &mut Commands, command: Command) {
    assert!(commands.push(command).is_ok());
}

fn record(event: &str, fields: serde_json::Value) -> Command {
    Command::RecordEvent {
        event: event.into(),
        fields,
    }
}

#[test]
fn records_have_separate_entry_admission_fifo_and_lower_work_priority() {
    let mut commands = Commands::default();
    for id in 0..RECORD_EVENTS {
        push(&mut commands, record("r", json!(id)));
    }
    push(&mut commands, record("overflow", json!(null)));
    assert_eq!(commands.dropped_record_events(), 1);
    for id in 0..REQUESTS {
        push(&mut commands, run(id as u64));
    }
    assert!(commands.push(run(99)).is_err());
    assert_eq!(commands.unsent_requests(), REQUESTS);
    push(&mut commands, Command::Cancel(99));
    assert!(matches!(commands.pop(), Some(Command::Cancel(99))));
    for id in 0..REQUESTS {
        assert!(matches!(commands.pop(), Some(Command::Run { serial, .. }) if serial==id as u64));
    }
    assert!(commands.work_is_empty() && !commands.is_empty());
    for id in 0..RECORD_EVENTS {
        assert!(
            matches!(commands.pop(), Some(Command::RecordEvent { fields, .. }) if fields==json!(id))
        );
    }
    assert!(commands.is_empty());
    assert_eq!(commands.dropped_record_events(), 1);
}

#[test]
fn record_byte_boundary_decrements_on_pop_and_stop_closes_and_counts_the_lane() {
    let mut commands = Commands::default();
    // One event byte plus quoted JSON string bytes exactly fills the budget.
    push(
        &mut commands,
        record("r", json!("x".repeat(RECORD_BYTES - 3))),
    );
    push(&mut commands, record("r", json!(null)));
    assert_eq!(commands.dropped_record_events(), 1);
    assert!(matches!(commands.pop(), Some(Command::RecordEvent { .. })));
    push(
        &mut commands,
        record("r", json!("x".repeat(RECORD_BYTES - 2))),
    );
    assert_eq!(
        commands.dropped_record_events(),
        2,
        "one byte over must be discarded"
    );
    push(&mut commands, record("queued", json!(null)));
    push(&mut commands, run(1));
    push(&mut commands, Command::Stop);
    push(&mut commands, record("after-stop", json!(null)));
    assert_eq!(commands.dropped_record_events(), 4);
    assert!(matches!(
        commands.pop(),
        Some(Command::Run { serial: 1, .. })
    ));
    assert!(matches!(commands.pop(), Some(Command::Stop)));
    assert!(commands.is_empty());
    commands.clear();
    push(&mut commands, record("after-teardown", json!(null)));
    assert_eq!(commands.dropped_record_events(), 5);
    assert!(commands.is_empty());
}

#[test]
fn record_encoding_failure_is_counted_without_hiding_semantic_encoding_errors() {
    let _no_forks = unix::no_forks();
    let (a, _b) = UnixStream::pair().unwrap();
    let mut channel = channel::Channel::new(a).unwrap();
    let mut commands = Commands::default();
    let mut writer = Writer::default();
    let mut encoder = Encoder { next: u64::MAX };
    push(&mut commands, record("diagnostic", json!(null)));
    writer
        .pump_commands(&mut commands, &mut encoder, &mut channel)
        .unwrap();
    assert_eq!(commands.dropped_record_events(), 1);
    assert!(commands.is_empty() && writer.idle(&channel) && !channel.closed());
    push(&mut commands, run(1));
    assert!(
        writer
            .pump_commands(&mut commands, &mut encoder, &mut channel)
            .is_err()
    );
}

#[test]
fn a_committed_record_frame_finishes_before_cancel_work_and_stop_drop_other_records() {
    let _no_forks = unix::no_forks();
    let (a, b) = UnixStream::pair().unwrap();
    let size: libc::c_int = 4096;
    assert_eq!(
        unsafe {
            libc::setsockopt(
                a.as_raw_fd(),
                libc::SOL_SOCKET,
                libc::SO_SNDBUF,
                (&size as *const libc::c_int).cast(),
                std::mem::size_of_val(&size) as _,
            )
        },
        0
    );
    let mut sender = channel::Channel::new(a).unwrap();
    let mut receiver = channel::Channel::new(b).unwrap();
    let mut commands = Commands::default();
    let mut writer = Writer::default();
    let mut encoder = Encoder::default();
    let payload = json!("x".repeat(512 * 1024));
    push(&mut commands, record("committed", payload.clone()));
    writer
        .pump_commands(&mut commands, &mut encoder, &mut sender)
        .unwrap();
    assert!(sender.wants_write());
    push(&mut commands, record("uncommitted", json!(null)));
    push(&mut commands, Command::Cancel(99));
    push(
        &mut commands,
        Command::Export {
            member: "child".into(),
        },
    );
    push(&mut commands, Command::Stop);
    assert_eq!(commands.dropped_record_events(), 1);
    let mut methods = vec![];
    let mut decoded_payload = None;
    for _ in 0..2048 {
        writer
            .pump_commands(&mut commands, &mut encoder, &mut sender)
            .unwrap();
        if let Some(frame) = receiver.receive().unwrap() {
            methods.push(frame.value["method"].as_str().unwrap().to_owned());
            if let Message::Command(Command::RecordEvent { fields, .. }) =
                Decoder::default().receive(frame).unwrap().unwrap().message
            {
                decoded_payload = Some(fields);
            }
        }
        if methods.len() == 4 && commands.is_empty() && writer.idle(&sender) {
            break;
        }
    }
    assert_eq!(
        methods,
        [
            "Command.RecordEvent",
            "Command.Cancel",
            "Command.Export",
            "Command.Stop"
        ]
    );
    assert_eq!(decoded_payload, Some(payload));
    assert!(commands.is_empty() && writer.drained(&sender));
}

#[test]
fn unsent_capacity_is_four_and_a_removed_target_releases_its_slot() {
    let mut commands = Commands::default();
    for id in 0..4 {
        push(&mut commands, run(id));
    }
    assert!(matches!(
        commands.push(run(4)),
        Err(Command::Run { serial: 4, .. })
    ));
    push(&mut commands, Command::Cancel(1));
    assert_eq!(commands.unsent_requests(), 3);
    push(&mut commands, run(4));
    let ids: Vec<_> = std::iter::from_fn(|| commands.pop())
        .map(|c| match c {
            Command::Run { serial, .. } => serial,
            _ => panic!("only uncancelled requests"),
        })
        .collect();
    assert_eq!(ids, [0, 2, 3, 4]);
}
#[test]
fn cancellation_priority_coalescing_and_host_preparation_cleanup_are_preserved() {
    let mut commands = Commands::default();
    push(&mut commands, run(1));
    push(&mut commands, Command::Cancel(9));
    push(&mut commands, Command::Cancel(9));
    push(
        &mut commands,
        Command::CancelProcess {
            id: 7,
            everything: false,
        },
    );
    push(
        &mut commands,
        Command::CancelProcess {
            id: 7,
            everything: true,
        },
    );
    push(
        &mut commands,
        Command::CancelProcess {
            id: 7,
            everything: false,
        },
    );
    push(
        &mut commands,
        Command::RunHostJob {
            request: "prepared".into(),
            argv: vec![],
            env: vec![],
            timeout: None,
        },
    );
    push(
        &mut commands,
        Command::CancelHostJob {
            request: "prepared".into(),
        },
    );
    push(
        &mut commands,
        Command::CancelHostJob {
            request: "prepared".into(),
        },
    );
    assert!(matches!(commands.pop(), Some(Command::Cancel(9))));
    assert!(matches!(
        commands.pop(),
        Some(Command::CancelProcess {
            id: 7,
            everything: true
        })
    ));
    assert!(
        matches!(commands.pop(),Some(Command::CancelHostJob { request }) if request=="prepared")
    );
    assert!(matches!(
        commands.pop(),
        Some(Command::Run { serial: 1, .. })
    ));
    assert!(
        matches!(commands.pop_cancelled(),Some(Event::HostJobFinished {request,outcome:Ok(outcome)}) if request=="prepared" && outcome.cancelled)
    );
    assert!(commands.is_empty());
    push(
        &mut commands,
        Command::PrepareHostJob {
            request: "new".into(),
            cwd: "/tmp".into(),
        },
    );
    push(
        &mut commands,
        Command::RunHostJob {
            request: "new".into(),
            argv: vec![],
            env: vec![],
            timeout: None,
        },
    );
    push(
        &mut commands,
        Command::CancelHostJob {
            request: "new".into(),
        },
    );
    assert!(commands.pop().is_none());
    assert!(
        matches!(commands.pop_cancelled(),Some(Event::HostJobPrepared {request,result:Err(_)}) if request=="new")
    );
    assert!(
        matches!(commands.pop_cancelled(),Some(Event::HostJobFinished {request,outcome:Ok(outcome)}) if request=="new" && outcome.cancelled)
    );
    assert!(commands.is_empty());
}
#[test]
fn cancelled_unsent_spawn_closes_all_capabilities_and_removes_its_signals() {
    let _no_forks = unix::no_forks();
    let (read, write) = unix::pipe().unwrap();
    let mut reader = fs::File::from(read);
    let mut commands = Commands::default();
    push(
        &mut commands,
        spawn(1, Stdio::Fds([write.into(), null(), null()])),
    );
    push(
        &mut commands,
        Command::Signal {
            id: 1,
            signal: libc::SIGINT,
        },
    );
    push(
        &mut commands,
        Command::CancelProcess {
            id: 1,
            everything: true,
        },
    );
    assert!(commands.pop().is_none());
    let mut byte = [0];
    assert_eq!(reader.read(&mut byte).unwrap(), 0);
    assert!(matches!(
        commands.pop_cancelled(),
        Some(Event::SpawnFailed { id: 1, .. })
    ));
    assert!(commands.is_empty());
}
#[test]
fn distinct_signals_and_kill_all_follow_spawn_with_sixteen_unsent_signals_per_process() {
    let mut commands = Commands::default();
    push(
        &mut commands,
        spawn(1, Stdio::NewPty { rows: 24, cols: 80 }),
    );
    for signal in 1..=18 {
        push(&mut commands, Command::Signal { id: 1, signal });
    }
    push(&mut commands, Command::KillAll);
    assert!(matches!(commands.pop(), Some(Command::Spawn { id: 1, .. })));
    for expected in 1..=16 {
        assert!(matches!(commands.pop(),Some(Command::Signal { id:1,signal }) if signal==expected));
    }
    assert!(matches!(commands.pop(), Some(Command::KillAll)));
    assert!(commands.is_empty());
}
#[test]
fn resize_stays_in_place_and_does_not_cross_attach_or_detach() {
    let mut commands = Commands::default();
    push(
        &mut commands,
        Command::Resize {
            terminal: TerminalId::Main,
            rows: 10,
            cols: 10,
        },
    );
    push(&mut commands, run(1));
    push(
        &mut commands,
        Command::Resize {
            terminal: TerminalId::Main,
            rows: 20,
            cols: 20,
        },
    );
    push(
        &mut commands,
        Command::Attach {
            terminal: TerminalId::Main,
            stream: null(),
            rows: 24,
            cols: 80,
        },
    );
    push(
        &mut commands,
        Command::Resize {
            terminal: TerminalId::Main,
            rows: 30,
            cols: 30,
        },
    );
    push(
        &mut commands,
        Command::Detach {
            terminal: TerminalId::Main,
        },
    );
    push(
        &mut commands,
        Command::Resize {
            terminal: TerminalId::Main,
            rows: 40,
            cols: 40,
        },
    );
    assert!(matches!(
        commands.pop(),
        Some(Command::Resize { rows: 20, .. })
    ));
    assert!(matches!(
        commands.pop(),
        Some(Command::Run { serial: 1, .. })
    ));
    assert!(matches!(commands.pop(), Some(Command::Attach { .. })));
    assert!(matches!(
        commands.pop(),
        Some(Command::Resize { rows: 30, .. })
    ));
    assert!(matches!(commands.pop(), Some(Command::Detach { .. })));
    assert!(matches!(
        commands.pop(),
        Some(Command::Resize { rows: 40, .. })
    ));
    assert!(commands.is_empty());
}
#[test]
fn sandbox_output_overflow_closes_only_that_connection_and_stop_stays_last() {
    let mut commands = Commands::default();
    push(
        &mut commands,
        Command::SandboxSend {
            connection: 1,
            message: json!({"id":1,"result":"a"}),
        },
    );
    push(
        &mut commands,
        Command::SandboxSend {
            connection: 2,
            message: json!({"id":2,"result":"b"}),
        },
    );
    push(&mut commands, Command::Stop);
    push(&mut commands, Command::Stop);
    push(
        &mut commands,
        Command::SandboxSend {
            connection: 1,
            message: json!("x".repeat(2 * 1024 * 1024)),
        },
    );
    push(
        &mut commands,
        Command::SandboxSend {
            connection: 1,
            message: json!("ignored closed output"),
        },
    );
    push(
        &mut commands,
        Command::Export {
            member: "member".into(),
        },
    );
    assert!(matches!(
        commands.pop(),
        Some(Command::SandboxSend { connection: 2, .. })
    ));
    assert!(matches!(
        commands.pop(),
        Some(Command::SandboxClose { connection: 1 })
    ));
    assert!(matches!(commands.pop(), Some(Command::Export { .. })));
    assert!(matches!(commands.pop(), Some(Command::Stop)));
    assert!(commands.is_empty());
}
#[test]
fn committed_large_frame_is_indivisible_and_stop_follows_earlier_export() {
    let _no_forks = unix::no_forks();
    let (a, b) = UnixStream::pair().unwrap();
    let size: libc::c_int = 4096;
    assert_eq!(
        unsafe {
            libc::setsockopt(
                a.as_raw_fd(),
                libc::SOL_SOCKET,
                libc::SO_SNDBUF,
                (&size as *const libc::c_int).cast(),
                std::mem::size_of_val(&size) as _,
            )
        },
        0
    );
    let mut sender = channel::Channel::new(a).unwrap();
    let mut receiver = channel::Channel::new(b).unwrap();
    let mut commands = Commands::default();
    let mut encoder = Encoder::default();
    let mut writer = Writer::default();
    let payload = goblins_jsonrpc::result(json!(1), json!("x".repeat(512 * 1024)));
    let encoded = goblins_jsonrpc::encode(&payload).unwrap();
    assert!(
        encoded.len() > 4096
            && serde_json::to_vec(&payload).unwrap().len() <= goblins_jsonrpc::MAX_BODY
    );
    push(
        &mut commands,
        Command::SandboxSend {
            connection: 1,
            message: payload.clone(),
        },
    );
    writer
        .pump_commands(&mut commands, &mut encoder, &mut sender)
        .unwrap();
    assert!(sender.wants_write());
    push(
        &mut commands,
        Command::Export {
            member: "member".into(),
        },
    );
    push(&mut commands, Command::Stop);
    let mut methods = vec![];
    let mut payloads = vec![];
    for _ in 0..2048 {
        writer
            .pump_commands(&mut commands, &mut encoder, &mut sender)
            .unwrap();
        if let Some(frame) = receiver.receive().unwrap() {
            methods.push(frame.value["method"].as_str().unwrap().to_string());
            let received = Decoder::default().receive(frame).unwrap().unwrap();
            if let Message::Command(Command::SandboxSend { message, .. }) = received.message {
                payloads.push(message);
            }
        }
        if commands.is_empty() && writer.idle(&sender) && methods.len() == 3 {
            break;
        }
    }
    assert_eq!(
        methods,
        ["Command.SandboxSend", "Command.Export", "Command.Stop"]
    );
    assert_eq!(payloads, vec![payload]);
    assert!(commands.is_empty());
    assert!(writer.idle(&sender));
}

#[test]
fn cancellation_passes_between_handoff_batches_while_stop_waits_for_the_complete_export() {
    let _no_forks = unix::no_forks();
    let root = unix::temp_directory().unwrap();
    let h = super::tests::handoff(&root, channel::MAX_FDS + 1);
    let (a, b) = UnixStream::pair().unwrap();
    let mut sender = channel::Channel::new(a).unwrap();
    let mut receiver = channel::Channel::new(b).unwrap();
    let mut encoder = Encoder::default();
    let mut writer = Writer::default();
    let mut commands = Commands::default();
    let mut decoder = Decoder::default();
    let prepared = encoder
        .prepare(Message::Command(Command::Start(Box::new(h))))
        .unwrap();
    assert!(writer.begin(prepared).is_ok());
    writer.pump(&mut sender).unwrap();
    assert!(!sender.wants_write());
    assert!(decoder.receive_channel(&mut receiver).unwrap().is_none());
    push(&mut commands, Command::Cancel(9));
    push(&mut commands, Command::Stop);
    writer
        .pump_commands(&mut commands, &mut encoder, &mut sender)
        .unwrap();
    assert!(matches!(
        decoder
            .receive_channel(&mut receiver)
            .unwrap()
            .unwrap()
            .message,
        Message::Command(Command::Cancel(9))
    ));
    writer
        .pump_commands(&mut commands, &mut encoder, &mut sender)
        .unwrap();
    let received = decoder.receive_channel(&mut receiver).unwrap().unwrap();
    assert!(matches!(
        received.message,
        Message::Command(Command::Start(_))
    ));
    let transfer = received.adopted.unwrap();
    writer.adopted(transfer).unwrap();
    writer
        .pump_commands(&mut commands, &mut encoder, &mut sender)
        .unwrap();
    assert!(matches!(
        decoder
            .receive_channel(&mut receiver)
            .unwrap()
            .unwrap()
            .message,
        Message::Command(Command::Stop)
    ));
    assert!(root.join("r0/held").exists());
    drop(received);
    for n in 0..=channel::MAX_FDS {
        assert!(!root.join(format!("r{n}")).exists());
    }
    fs::remove_dir_all(root).unwrap();
}
#[test]
fn sandbox_output_budget_includes_the_committed_partially_written_frame() {
    let _no_forks = unix::no_forks();
    let (a, b) = UnixStream::pair().unwrap();
    let size: libc::c_int = 4096;
    assert_eq!(
        unsafe {
            libc::setsockopt(
                a.as_raw_fd(),
                libc::SOL_SOCKET,
                libc::SO_SNDBUF,
                (&size as *const libc::c_int).cast(),
                std::mem::size_of_val(&size) as _,
            )
        },
        0
    );
    let mut sender = channel::Channel::new(a).unwrap();
    let mut receiver = channel::Channel::new(b).unwrap();
    let mut commands = Commands::default();
    let mut encoder = Encoder::default();
    let mut writer = Writer::default();
    let first = goblins_jsonrpc::result(json!(1), json!("x".repeat(700_000)));
    let second = goblins_jsonrpc::result(json!(2), json!("y".repeat(700_000)));
    let third = goblins_jsonrpc::result(json!(3), json!("z".repeat(700_000)));
    let sizes: Vec<_> = [&first, &second, &third]
        .into_iter()
        .map(|value| {
            assert!(serde_json::to_vec(value).unwrap().len() <= goblins_jsonrpc::MAX_BODY);
            goblins_jsonrpc::encode(value).unwrap().len()
        })
        .collect();
    assert!(sizes[0] + sizes[1] <= 2 * 1024 * 1024);
    assert!(sizes.iter().sum::<usize>() > 2 * 1024 * 1024);
    push(
        &mut commands,
        Command::SandboxSend {
            connection: 7,
            message: first.clone(),
        },
    );
    writer
        .pump_commands(&mut commands, &mut encoder, &mut sender)
        .unwrap();
    assert!(sender.wants_write());
    push(
        &mut commands,
        Command::SandboxSend {
            connection: 7,
            message: second,
        },
    );
    push(
        &mut commands,
        Command::SandboxSend {
            connection: 7,
            message: third,
        },
    );
    let mut methods = vec![];
    let mut payloads = vec![];
    for _ in 0..2048 {
        writer
            .pump_commands(&mut commands, &mut encoder, &mut sender)
            .unwrap();
        if let Some(frame) = receiver.receive().unwrap() {
            methods.push(frame.value["method"].as_str().unwrap().to_string());
            let received = Decoder::default().receive(frame).unwrap().unwrap();
            if let Message::Command(Command::SandboxSend { message, .. }) = received.message {
                payloads.push(message);
            }
        }
        if methods.len() == 2 && writer.idle(&sender) && commands.is_empty() {
            break;
        }
    }
    assert_eq!(methods, ["Command.SandboxSend", "Command.SandboxClose"]);
    assert_eq!(payloads, vec![first]);
}

#[test]
fn cancellation_outcomes_take_the_removed_request_slots_until_reported() {
    let mut commands = Commands::default();
    for id in 0..4 {
        push(
            &mut commands,
            spawn(id, Stdio::NewPty { rows: 24, cols: 80 }),
        );
        push(
            &mut commands,
            Command::CancelProcess {
                id,
                everything: true,
            },
        );
    }
    assert_eq!(commands.unsent_requests(), 0);
    assert!(matches!(
        commands.push(spawn(4, Stdio::NewPty { rows: 24, cols: 80 })),
        Err(Command::Spawn { id: 4, .. })
    ));
    assert!(matches!(
        commands.pop_cancelled(),
        Some(Event::SpawnFailed { id: 0, .. })
    ));
    push(
        &mut commands,
        spawn(4, Stdio::NewPty { rows: 24, cols: 80 }),
    );
    for id in 1..4 {
        assert!(
            matches!(commands.pop_cancelled(),Some(Event::SpawnFailed {id:actual,..}) if actual==id)
        );
    }
    assert!(commands.pop_cancelled().is_none());
    assert!(matches!(commands.pop(), Some(Command::Spawn { id: 4, .. })));
    assert!(commands.is_empty());
}

fn terminal_call(request: u64) -> Command {
    Command::TerminalCall {
        request,
        terminal: 7,
        call: crate::terminal::Call::Wait {
            condition: crate::terminal::Condition::Exit,
            timeout: 1000,
            lines: None,
        },
    }
}

#[test]
fn terminal_calls_are_bounded_requests_and_their_cancellation_overtakes_blocked_work() {
    let mut commands = Commands::default();
    for request in 0..REQUESTS as u64 {
        push(&mut commands, terminal_call(request));
    }
    assert_eq!(commands.unsent_requests(), REQUESTS);
    // Admission is shared with every other request: a full queue refuses.
    assert!(matches!(
        commands.push(terminal_call(99)),
        Err(Command::TerminalCall { request: 99, .. })
    ));
    assert!(commands.push(run(1)).is_err());
    // Cancelling an unsent call removes it, with no answer owed and no
    // cancellation left to send.
    push(&mut commands, Command::TerminalCancel { request: 1 });
    assert_eq!(commands.unsent_requests(), REQUESTS - 1);
    assert!(commands.pop_cancellation().is_none());
    assert!(commands.pop_cancelled().is_none());
    push(&mut commands, terminal_call(100));
    // A call that was sent is cancelled ahead of the work still queued, once.
    push(&mut commands, Command::TerminalCancel { request: 55 });
    push(&mut commands, Command::TerminalCancel { request: 55 });
    assert!(matches!(
        commands.pop_cancellation(),
        Some(Command::TerminalCancel { request: 55 })
    ));
    assert!(commands.pop_cancellation().is_none());
    let order: Vec<_> = std::iter::from_fn(|| commands.pop())
        .map(|c| match c {
            Command::TerminalCall { request, .. } => request,
            other => panic!("{other:?}"),
        })
        .collect();
    assert_eq!(order, [0, 2, 3, 100]);
}
