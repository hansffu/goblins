//! App Server contracts exercised over the adapter's real websocket transport.
use super::*;
use std::os::unix::net::UnixListener;

struct Call(&'static str, Value, std::result::Result<Value, Value>);
fn call(method: &'static str, params: Value, result: Value) -> Call {
    Call(method, params, Ok(result))
}
fn scripted(calls: Vec<Call>, test: impl FnOnce(&mut Server)) {
    let path = std::env::temp_dir().join(format!(
        "gcs-{}.sock",
        goblins_protocol::messages::operation_key(None).unwrap()
    ));
    struct Socket(PathBuf);
    impl Drop for Socket {
        fn drop(&mut self) {
            let _ = fs::remove_file(&self.0);
        }
    }
    let _socket = Socket(path.clone());
    let listener = UnixListener::bind(&path).unwrap();
    listener.set_nonblocking(true).unwrap();
    std::thread::scope(|scope| {
        let peer = scope.spawn(move || {
            let deadline = Instant::now() + Duration::from_secs(3);
            let stream = loop {
                match listener.accept() {
                    Ok((stream, _)) => break stream,
                    Err(e)
                        if e.kind() == io::ErrorKind::WouldBlock && Instant::now() < deadline =>
                    {
                        std::thread::sleep(Duration::from_millis(1))
                    }
                    Err(e) => panic!("accept: {e}"),
                }
            };
            stream
                .set_read_timeout(Some(Duration::from_secs(3)))
                .unwrap();
            stream
                .set_write_timeout(Some(Duration::from_secs(3)))
                .unwrap();
            let mut ws = tungstenite::accept(stream).unwrap();
            fn read(ws: &mut WebSocket<UnixStream>) -> Value {
                serde_json::from_str(ws.read().unwrap().to_text().unwrap()).unwrap()
            }
            let init = read(&mut ws);
            assert_eq!(init["method"], "initialize");
            assert_eq!(init["params"]["capabilities"]["experimentalApi"], true);
            ws.send(Message::text(
                json!({"id":init["id"],"result":{}}).to_string(),
            ))
            .unwrap();
            assert_eq!(read(&mut ws)["method"], "initialized");
            for Call(method, params, reply) in calls {
                let request = read(&mut ws);
                assert_eq!(request["method"], method);
                assert_eq!(request["params"], params, "{method}");
                let mut response = json!({"id":request["id"]});
                match reply {
                    Ok(result) => response["result"] = result,
                    Err(error) => response["error"] = error,
                }
                ws.send(Message::text(response.to_string())).unwrap();
            }
        });
        let (tx, _rx) = mpsc::channel();
        let mut server = Server::connect(&path, tx).unwrap();
        test(&mut server);
        peer.join().unwrap();
    });
}
fn thread(policy: &str, sandbox: &str, state: &str) -> Value {
    json!({"thread":{"id":"recorded","cwd":"/work","status":{"type":state}},
           "approvalPolicy":policy,"sandbox":{"type":sandbox}})
}
#[test]
fn new_threads_use_native_approval_full_access_and_are_named_before_joining() {
    for (config, expected, bypass) in [
        (json!({}), "on-request", false),
        (json!({"approval_policy":"untrusted"}), "untrusted", false),
        (json!({}), "never", true),
    ] {
        let mut calls = vec![];
        if !bypass {
            calls.push(call("config/read", json!({}), json!({"config":config})));
        }
        calls.extend([
            call(
                "thread/start",
                json!({"approvalPolicy":expected,"sandbox":"danger-full-access"}),
                thread(expected, "dangerFullAccess", "idle"),
            ),
            call(
                "thread/name/set",
                json!({"threadId":"recorded","name":"chief"}),
                json!({}),
            ),
        ]);
        scripted(calls, |s| {
            let opened = open(s, None, bypass, "chief").unwrap();
            assert_eq!(opened["id"], "recorded");
            assert_eq!(opened["name"], "chief");
        });
    }
}
#[test]
fn resume_reads_policy_in_the_recorded_directory_and_verifies_repairs() {
    for repaired in [true, false] {
        scripted(
            vec![
                call(
                    "thread/read",
                    json!({"threadId":"recorded","includeTurns":false}),
                    thread("old", "readOnly", "idle"),
                ),
                call(
                    "config/read",
                    json!({"cwd":"/work"}),
                    json!({"config":{"approval_policy":"on-failure"}}),
                ),
                call(
                    "thread/resume",
                    json!({"threadId":"recorded","excludeTurns":true,"approvalPolicy":"on-failure","sandbox":"danger-full-access"}),
                    thread("old", "readOnly", "idle"),
                ),
                call(
                    "thread/settings/update",
                    json!({"threadId":"recorded","approvalPolicy":"on-failure","sandboxPolicy":{"type":"dangerFullAccess"}}),
                    json!({}),
                ),
                call(
                    "thread/resume",
                    json!({"threadId":"recorded","excludeTurns":true}),
                    thread(
                        "on-failure",
                        if repaired {
                            "dangerFullAccess"
                        } else {
                            "readOnly"
                        },
                        "active",
                    ),
                ),
            ],
            |s| {
                let opened = open(s, Some("recorded"), false, "ignored");
                if repaired {
                    assert_eq!(opened.unwrap()["status"]["type"], "active");
                } else {
                    assert!(opened.unwrap_err().to_string().contains("did not apply"));
                }
            },
        );
    }
}
#[test]
fn joining_never_changes_policy_and_only_unsaved_rollouts_are_absent() {
    scripted(
        vec![call(
            "thread/resume",
            json!({"threadId":"recorded","excludeTurns":true}),
            thread("old", "readOnly", "idle"),
        )],
        |s| {
            assert_eq!(join(s, "recorded").unwrap().unwrap()["id"], "recorded");
        },
    );
    for (message, absent) in [
        ("no rollout found for thread", true),
        ("permission denied", false),
    ] {
        scripted(
            vec![Call(
                "thread/resume",
                json!({"threadId":"recorded","excludeTurns":true}),
                Err(json!({"code":-1,"message":message})),
            )],
            |s| {
                let result = join(s, "recorded");
                if absent {
                    assert!(result.unwrap().is_none());
                } else {
                    assert!(result.unwrap_err().to_string().contains(message));
                }
            },
        );
    }
}
#[test]
fn loaded_threads_follow_cursors_validate_lists_and_bound_pagination() {
    scripted(
        vec![
            call(
                "thread/loaded/list",
                json!({"cursor":null}),
                json!({"data":["a",17],"nextCursor":"next"}),
            ),
            call(
                "thread/loaded/list",
                json!({"cursor":"next"}),
                json!({"data":["b"],"nextCursor":null}),
            ),
        ],
        |s| {
            assert_eq!(loaded(s).unwrap(), ["a", "b"]);
        },
    );
    scripted(
        vec![call(
            "thread/loaded/list",
            json!({"cursor":null}),
            json!({"data":{}}),
        )],
        |s| {
            assert!(
                loaded(s)
                    .unwrap_err()
                    .to_string()
                    .contains("invalid loaded")
            );
        },
    );
    let calls = (0..32)
        .map(|n| {
            call(
                "thread/loaded/list",
                json!({"cursor":if n == 0 {Value::Null} else {json!(n)}}),
                json!({"data":[],"nextCursor":n+1}),
            )
        })
        .collect();
    scripted(calls, |s| {
        assert!(
            loaded(s)
                .unwrap_err()
                .to_string()
                .contains("pagination limit")
        );
    });
}
#[test]
fn catch_up_uses_current_attachment_excludes_reminders_and_stops_at_old_history() {
    let params =
        |cursor| json!({"threadId":"recorded","sortDirection":"desc","limit":100,"cursor":cursor});
    let item = |id, at, client| json!({"startedAtMs":at,"item":{"id":id,"type":"userMessage","clientId":client}});
    let mut follow = Follow::default();
    let mut ledger = Ledger::default();
    ledger
        .threads
        .entry("recorded".into())
        .or_default()
        .reminders
        .push_back("reminder".into());
    scripted(vec![], |s| {
        catch_up(s, &mut follow, &ledger, "recorded").unwrap();
    });
    follow.attach(Some(100));
    scripted(
        vec![
            call(
                "thread/items/list",
                params(Value::Null),
                json!({"data":[item("r",300,"reminder"),item("human",200,"human")],"nextCursor":"page2"}),
            ),
            call(
                "thread/items/list",
                params(json!("page2")),
                json!({"data":[item("human",250,"human"),item("older",99,"old")],"nextCursor":"must-not-read"}),
            ),
        ],
        |s| {
            catch_up(s, &mut follow, &ledger, "recorded").unwrap();
        },
    );
    assert_eq!(follow.winner, Some((200, "recorded".into())));
    assert_eq!(follow.seen["recorded"].len(), 2);
    scripted(
        vec![call(
            "thread/items/list",
            params(Value::Null),
            json!({"data":null}),
        )],
        |s| {
            assert!(
                catch_up(s, &mut follow, &ledger, "recorded")
                    .unwrap_err()
                    .to_string()
                    .contains("invalid item")
            );
        },
    );
    scripted(
        (0..8)
            .map(|n| {
                call(
                    "thread/items/list",
                    params(if n == 0 { Value::Null } else { json!(n) }),
                    json!({"data":[],"nextCursor":n+1}),
                )
            })
            .collect(),
        |s| {
            catch_up(s, &mut follow, &ledger, "recorded").unwrap();
        },
    );
}
