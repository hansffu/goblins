use super::*;
use goblins_protocol::messages::read_body;

const INSTANCE: &str = "0123456789abcdef0123456789abcdef";

#[derive(Default)]
struct MemoryAudit {
    events: Vec<Event>,
    fail: bool,
    full: bool,
}
impl Audit for MemoryAudit {
    fn append(&mut self, event: &Event, _: usize) -> std::result::Result<(), AuditError> {
        if self.fail {
            return Err(AuditError::Unavailable);
        }
        if self.full && event.kind == "message.accepted" {
            return Err(AuditError::Capacity);
        }
        self.events.push(event.clone());
        Ok(())
    }
}

#[test]
fn audit_capacity_rejects_new_work_without_freezing_existing_completion() {
    let mut f = Fixture::new();
    f.send("p", "c", "first", "finish this");
    f.audit.full = true;
    assert_eq!(
        f.call(
            "p",
            "messages.send",
            json!({"key":"second","to":"c","body":"later"})
        )
        .unwrap_err(),
        capacity()
    );
    assert_eq!(f.mailbox.status("c")["audit_failed"], false);
    let claim = f.next("c", "fetch")["message"].clone();
    f.complete("c", &claim, "complete").unwrap();
}

struct Fixture {
    mailbox: Mailbox,
    directory: Directory,
    audit: MemoryAudit,
}
impl Fixture {
    fn new() -> Self {
        let directory = [
            ("p", None, "chief"),
            ("c", Some("p"), "chief/scout"),
            ("g", Some("c"), "chief/scout/helper"),
            ("s", Some("p"), "chief/sibling"),
            ("x", None, "unrelated"),
            ("y", Some("x"), "unrelated/scout"),
        ]
        .into_iter()
        .map(|(id, parent, path)| {
            (
                id.into(),
                Session {
                    id: id.into(),
                    parent: parent.map(Into::into),
                    path: path.into(),
                    running: true,
                },
            )
        })
        .collect();
        Self {
            mailbox: Mailbox::new(INSTANCE.into(), Limits::default()).unwrap(),
            directory,
            audit: MemoryAudit::default(),
        }
    }
    fn call(&mut self, actor: &str, method: &str, params: Value) -> Result<Value> {
        let command = Mutation::parse(method, params).map_err(|e| (-32602, e))?;
        self.mailbox
            .execute(actor, command, &self.directory, 1000, &mut self.audit)
    }
    fn send(&mut self, actor: &str, to: &str, key: &str, body: &str) -> Value {
        self.call(
            actor,
            "messages.send",
            json!({"key":key,"to":to,"body":body}),
        )
        .unwrap()
    }
    fn next(&mut self, actor: &str, key: &str) -> Value {
        self.call(actor, "inbox.next", json!({"key":key})).unwrap()
    }
    fn complete(&mut self, actor: &str, message: &Value, key: &str) -> Result<Value> {
        self.call(
            actor,
            "inbox.complete",
            json!({"key":key,"message":message["id"],
            "claim_generation":message["claim_generation"]}),
        )
    }
}

#[test]
fn authenticated_routing_and_private_inboxes() {
    let mut f = Fixture::new();
    let first = f.send("p", "scout", "one", "pick a number");
    assert_eq!(first["recipient"], "c");
    assert_eq!(first["sender"], "p");
    assert_eq!(f.send("c", "parent", "two", "ready")["recipient"], "p");
    assert_eq!(
        f.send("p", "scout/helper", "three", "nested")["recipient"],
        "g"
    );
    for (actor, to) in [
        ("c", "s"),
        ("c", "x"),
        ("c", "p/scout"),
        ("g", "p"),
        ("c", "nonexistent"),
    ] {
        let error = f
            .call(
                actor,
                "messages.send",
                json!({"key":"denied","to":to,"body":"x"}),
            )
            .unwrap_err();
        assert_eq!(error, absent());
    }
    assert_eq!(
        f.mailbox
            .get("s", first["id"].as_str().unwrap())
            .unwrap_err(),
        absent()
    );
    assert_eq!(
        f.mailbox
            .get("c", first["id"].as_str().unwrap())
            .unwrap()
            .body,
        "pick a number"
    );
    assert_eq!(f.mailbox.status("s")["queued"], 0);
    assert_eq!(
        f.call(
            "host",
            "messages.send",
            json!({"key":"ambiguous","to":"scout","body":"x"})
        )
        .unwrap_err()
        .0,
        -32009
    );
    assert_eq!(
        f.send("host", "chief/scout", "host", "my message")["sender"],
        "host"
    );
}

#[test]
fn fifo_claims_and_repeated_fetch_do_not_drain_an_inbox() {
    let mut f = Fixture::new();
    let first = f.send("p", "c", "one", "first");
    let second = f.send("p", "c", "two", "second");
    let claim = f.next("c", "fetch1")["message"].clone();
    assert_eq!(claim["id"], first["id"]);
    assert_eq!(claim["claim_generation"], 1);
    assert_eq!(f.next("c", "fetch2")["message"], claim);
    assert_eq!(f.mailbox.status("c")["queued"], 1);
    f.complete("c", &claim, "complete1").unwrap();
    assert_eq!(f.next("c", "fetch1")["message"], claim); // lost-response replay
    let claim2 = f.next("c", "fetch3")["message"].clone();
    assert_eq!(claim2["id"], second["id"]);
    f.complete("c", &claim2, "complete2").unwrap();
    assert!(f.next("c", "fetch4")["message"].is_null());
}

#[test]
fn empty_fetch_retry_is_a_receipt_not_a_new_claim() {
    let mut f = Fixture::new();
    let empty = f.next("c", "empty");
    let sent = f.send("p", "c", "one", "arrived later");
    assert_eq!(f.next("c", "empty"), empty);
    assert_eq!(f.mailbox.status("c")["queued"], 1);
    assert_eq!(f.next("c", "new")["message"]["id"], sent["id"]);
}

#[test]
fn duplicate_send_survives_name_reuse_and_changed_body_conflicts() {
    let mut f = Fixture::new();
    let original = f.send("p", "scout", "same", "original");
    f.directory.get_mut("c").unwrap().running = false;
    f.directory.insert(
        "replacement".into(),
        Session {
            id: "replacement".into(),
            parent: Some("p".into()),
            path: "chief/scout".into(),
            running: true,
        },
    );
    assert_eq!(f.send("p", "scout", "same", "original"), original);
    assert_eq!(f.audit.events.len(), 1);
    assert_eq!(
        f.call(
            "p",
            "messages.send",
            json!({"key":"same","to":"scout","body":"changed"})
        )
        .unwrap_err(),
        conflict()
    );
    assert_eq!(
        f.send("p", "scout", "new", "next")["recipient"],
        "replacement"
    );
}

#[test]
fn reply_completes_original_and_is_idempotent() {
    let mut f = Fixture::new();
    let original = f.send("p", "c", "one", "50");
    let claim = f.next("c", "fetch")["message"].clone();
    let params =
        json!({"key":"reply","message":claim["id"],"claim_generation":1,"body":"too high"});
    let reply = f.call("c", "messages.reply", params.clone()).unwrap();
    assert_eq!(reply["completed"], original["id"]);
    assert_eq!(reply["reply"]["in_reply_to"], original["id"]);
    assert_eq!(reply["reply"]["conversation"], original["conversation"]);
    assert_eq!(f.mailbox.status("c")["claim"], Value::Null);
    assert_eq!(f.mailbox.status("p")["queued"], 1);
    assert_eq!(f.call("c", "messages.reply", params).unwrap(), reply);
    assert_eq!(f.mailbox.status("p")["queued"], 1);
    assert_eq!(
        f.audit
            .events
            .iter()
            .filter(|e| e.kind == "message.replied")
            .count(),
        1
    );
    assert_eq!(
        f.complete("p", &claim, "wrong-recipient").unwrap_err(),
        absent()
    );
}

#[test]
fn reply_to_host_uses_host_inbox_without_granting_other_inbox_access() {
    let mut f = Fixture::new();
    f.send("host", "c", "one", "my message");
    let claim = f.next("c", "fetch")["message"].clone();
    f.call(
        "c",
        "messages.reply",
        json!({"key":"reply","message":claim["id"],"claim_generation":1,"body":"done"}),
    )
    .unwrap();
    let host_claim = f.next("host", "fetch")["message"].clone();
    assert_eq!(host_claim["body"], "done");
    assert_eq!(host_claim["recipient"], "host");
    f.complete("host", &host_claim, "done").unwrap();
}

#[test]
fn full_reply_inbox_does_not_complete_original() {
    let mut f = Fixture::new();
    f.mailbox.limits.inbox = 1;
    f.send("p", "c", "question", "50");
    f.send("host", "p", "fill", "another task");
    let claim = f.next("c", "fetch")["message"].clone();
    let params =
        json!({"key":"reply","message":claim["id"],"claim_generation":1,"body":"too high"});
    assert_eq!(
        f.call("c", "messages.reply", params.clone()).unwrap_err(),
        capacity()
    );
    assert_eq!(f.mailbox.status("c")["claim"], claim["id"]);
    let parent_claim = f.next("p", "fetch")["message"].clone();
    f.complete("p", &parent_claim, "complete").unwrap();
    f.call("c", "messages.reply", params).unwrap();
    assert_eq!(f.mailbox.status("c")["claim"], Value::Null);
}

#[test]
fn requeue_invalidates_old_claim_and_replay_does_not_upgrade_it() {
    let mut f = Fixture::new();
    f.send("p", "c", "one", "work");
    let claim = f.next("c", "fetch1")["message"].clone();
    f.call(
        "host",
        "inbox.requeue",
        json!({"key":"recover","message":claim["id"],"claim_generation":1,"reason":"interrupted"}),
    )
    .unwrap();
    assert_eq!(f.complete("c", &claim, "stale").unwrap_err(), conflict());
    assert_eq!(f.next("c", "fetch1")["message"]["claim_generation"], 1);
    let recovered = f.next("c", "fetch2")["message"].clone();
    assert_eq!(recovered["claim_generation"], 2);
    f.complete("c", &recovered, "done").unwrap();
}

#[test]
fn recipient_exit_fails_pending_items_and_preserves_send_receipt() {
    let mut f = Fixture::new();
    let first = f.send("p", "c", "one", "first");
    let second = f.send("p", "c", "two", "second");
    f.next("c", "fetch");
    f.directory.get_mut("c").unwrap().running = false;
    f.mailbox.stop_recipient("c", 2000, &mut f.audit).unwrap();
    for id in [&first["id"], &second["id"]] {
        assert_eq!(
            f.mailbox.get("p", id.as_str().unwrap()).unwrap().state,
            State::Failed
        );
    }
    assert_eq!(f.send("p", "c", "one", "first"), first);
    assert_eq!(
        f.call(
            "p",
            "messages.send",
            json!({"key":"new","to":"c","body":"x"})
        )
        .unwrap_err(),
        absent()
    );
    let count = f.audit.events.len();
    f.mailbox.stop_recipient("c", 3000, &mut f.audit).unwrap();
    assert_eq!(f.audit.events.len(), count);
}

#[test]
fn audit_failure_never_publishes_a_partial_reply_and_freezes_mutations() {
    let mut f = Fixture::new();
    f.send("p", "c", "one", "50");
    let claim = f.next("c", "fetch")["message"].clone();
    f.audit.fail = true;
    let params = json!({"key":"reply","message":claim["id"],"claim_generation":1,"body":"correct"});
    assert!(f.call("c", "messages.reply", params.clone()).is_err());
    assert_eq!(f.mailbox.status("c")["claim"], claim["id"]);
    assert_eq!(f.mailbox.status("p")["queued"], 0);
    assert_eq!(f.mailbox.status("c")["audit_failed"], true);
    f.audit.fail = false;
    assert!(f.call("c", "messages.reply", params).is_err());
    assert_eq!(f.next("c", "fetch")["message"], claim); // recover committed receipt
    assert_eq!(f.audit.events.len(), 2);
}

#[test]
fn capacity_reserves_fetch_and_completion_but_rejects_unbounded_empty_reads() {
    let mut f = Fixture::new();
    f.mailbox.limits.operations = 3;
    let sent = f.send("p", "c", "one", "work"); // reserves fetch + complete
    assert_eq!(
        f.call("p", "inbox.next", json!({"key":"empty"}))
            .unwrap_err(),
        capacity()
    );
    let claim = f.next("c", "fetch")["message"].clone();
    f.complete("c", &claim, "done").unwrap();
    assert_eq!(f.send("p", "c", "one", "work"), sent);
    assert_eq!(
        f.call("c", "inbox.next", json!({"key":"later"}))
            .unwrap_err(),
        capacity()
    );
}

#[test]
fn conversation_ids_do_not_confer_routing_or_visibility() {
    let mut f = Fixture::new();
    let message = f.send("p", "c", "one", "setup");
    assert_eq!(
        f.call(
            "x",
            "messages.send",
            json!({"key":"foreign","to":"y","body":"x","conversation":message["conversation"]})
        )
        .unwrap_err(),
        absent()
    );
    assert_eq!(
        f.call(
            "c",
            "messages.send",
            json!({"key":"sibling","to":"s","body":"x","conversation":message["conversation"]})
        )
        .unwrap_err(),
        absent()
    );
    let result = f.call("c","messages.send",json!({"key":"allowed","to":"parent","body":"ready","conversation":message["conversation"]})).unwrap();
    assert_eq!(result["conversation"], message["conversation"]);
}

#[test]
fn sender_file_contents_survive_source_deletion_and_are_audited_once() {
    let mut f = Fixture::new();
    let path = std::env::temp_dir().join(format!("goblins-mailbox-input-{}", std::process::id()));
    let source = "literal $(false)\n\u{001b}[2J\n🐟";
    std::fs::write(&path, source).unwrap();
    let body = read_body(std::fs::File::open(&path).unwrap()).unwrap();
    f.send("p", "c", "file", &body);
    std::fs::remove_file(path).unwrap();
    let claim = f.next("c", "fetch")["message"].clone();
    assert_eq!(claim["body"], source);
    f.complete("c", &claim, "done").unwrap();
    assert_eq!(f.audit.events[0].data["change"]["message"]["body"], source);
    for event in &f.audit.events[1..] {
        assert!(!serde_json::to_string(event).unwrap().contains("body"));
    }
    let encoded = serde_json::to_string(&f.audit.events[0]).unwrap();
    assert!(!encoded.contains('\u{001b}'));
    assert!(!encoded.contains('\n'));
}
