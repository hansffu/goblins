use super::*;
use goblins_protocol::messages::read_body;

const INSTANCE: &str = "0123456789abcdef0123456789abcdef";

#[derive(Default)]
struct MemoryAudit {
    events: Vec<Event>,
    fail: bool,
    full: bool,
    /// Reject raid events for lack of capacity.
    raid_full: bool,
    /// Reject operation outcomes that carry a notice for lack of capacity.
    notice_full: bool,
    /// A byte limit enforced like the journal's, with the bytes written.
    limit: Option<usize>,
    bytes: usize,
}
impl Audit for MemoryAudit {
    fn append(&mut self, event: &Event, reserved: usize) -> std::result::Result<(), AuditError> {
        if self.fail {
            return Err(AuditError::Unavailable);
        }
        if self.full && event.kind == "message.accepted"
            || self.raid_full && event.kind.starts_with("raid.")
            || self.notice_full
                && event.kind == "operation.completed"
                && !event.data["notice"].is_null()
        {
            return Err(AuditError::Capacity);
        }
        let line = serde_json::to_vec(event).unwrap().len() + 1;
        if self
            .limit
            .is_some_and(|limit| self.bytes + line + reserved > limit)
        {
            return Err(AuditError::Capacity);
        }
        self.bytes += line;
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
            ("p", "chief"),
            ("c", "scout"),
            ("g", "helper"),
            ("s", "sibling"),
            ("x", "unrelated"),
            ("y", "lookout"),
        ]
        .into_iter()
        .map(|(id, name)| {
            (
                id.into(),
                Session {
                    id: id.into(),
                    agent_name: name.into(),
                    stop_reason: None,
                    running: true,
                },
            )
        })
        .collect();
        let mut f = Self {
            mailbox: Mailbox::new(INSTANCE.into(), Limits::default()).unwrap(),
            directory,
            audit: MemoryAudit::default(),
        };
        // chief's crew shares a raid, as logged; unrelated and lookout do not.
        for id in ["p", "c", "g", "s"] {
            f.mailbox.raids.insert(
                id.into(),
                RaidView {
                    id: "base".into(),
                    name: "crew".into(),
                    role: None,
                    owner: Some("p".into()),
                    paused: false,
                },
            );
        }
        f
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
    fn record(&mut self, events: Vec<crate::raid::Event>) {
        self.mailbox
            .record_raid(&events, &self.directory, 1000, &mut self.audit);
    }
    fn reply(&mut self, actor: &str, claim: &Value, key: &str) -> Result<Value> {
        self.call(
            actor,
            "messages.reply",
            json!({"key":key,"message":claim["id"],
            "claim_generation":claim["claim_generation"],"body":"answer"}),
        )
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
    assert_eq!(
        (&first["sender_name"], &first["recipient_name"]),
        (&json!("chief"), &json!("scout"))
    );
    assert_eq!(f.send("c", "chief", "two", "ready")["recipient"], "p");
    assert_eq!(f.send("g", "s", "three", "by id")["recipient"], "s");
    // Only the sender's raid: no tree, no parent alias, never itself.
    for (actor, to) in [
        ("c", "x"),
        ("c", "lookout"),
        ("x", "p"),
        ("c", "parent"),
        ("c", "scout"),
        ("c", "nonexistent"),
    ] {
        let error = f
            .call(
                actor,
                "messages.send",
                json!({"key":"denied","to":to,"body":"x"}),
            )
            .unwrap_err();
        assert_eq!(error, absent(), "{actor} -> {to}");
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
    assert_eq!(f.mailbox.status("s")["queued"], 1);
    // The host reaches every goblin.
    let sent = f.send("host", "lookout", "host", "my message");
    assert_eq!(
        (&sent["sender"], &sent["recipient"]),
        (&json!("host"), &json!("y"))
    );
    assert_eq!(
        f.audit.events.last().unwrap().data["change"]["route"],
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
            agent_name: "scout".into(),
            stop_reason: None,
            running: true,
        },
    );
    let crew = f.mailbox.raids["c"].clone();
    f.mailbox.raids.insert("replacement".into(), crew);
    assert_eq!(f.send("p", "scout", "same", "original"), original);
    assert_eq!(f.audit.events.len(), 2);
    assert_eq!(f.audit.events[1].kind, "operation.retried");
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
            json!({"key":"outside","to":"x","body":"x","conversation":message["conversation"]})
        )
        .unwrap_err(),
        absent()
    );
    let result = f.call("c","messages.send",json!({"key":"allowed","to":"chief","body":"ready","conversation":message["conversation"]})).unwrap();
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

fn raid_of(members: &[&str]) -> (crate::raid::Raids, Vec<crate::raid::Event>) {
    let mut raids = crate::raid::Raids::default();
    let members: Vec<String> = members.iter().map(|m| m.to_string()).collect();
    let events = raids
        .invite("r1".into(), "review", &members, "host")
        .unwrap();
    (raids, events)
}

#[test]
fn logged_raid_membership_permits_sends_until_removed() {
    use crate::raid::Reason;
    let mut f = Fixture::new();
    let (mut raids, events) = raid_of(&["c", "y"]);
    // The controller's copy authorizes nothing: only recorded events do.
    assert_eq!(
        f.call(
            "c",
            "messages.send",
            json!({"key":"early","to":"y","body":"x"})
        )
        .unwrap_err(),
        absent()
    );
    f.record(events);
    let sent = f.send("c", "lookout", "name", "by name");
    assert_eq!(sent["recipient"], "y");
    let accepted = f.audit.events.last().unwrap();
    assert_eq!(accepted.data["change"]["route"], "raid");
    assert_eq!(accepted.data["change"]["raid"], "r1");
    assert_eq!(f.send("y", "c", "id", "by id")["recipient"], "c");
    // The host's route needs no raid.
    f.send("host", "c", "host", "from the host");
    let accepted = f.audit.events.last().unwrap();
    assert_eq!(accepted.data["change"]["route"], "host");
    assert_eq!(accepted.data["change"]["raid"], Value::Null);
    // Membership never lets one member read another's inbox or messages.
    assert_eq!(
        f.mailbox
            .get("x", sent["id"].as_str().unwrap())
            .unwrap_err(),
        absent()
    );
    assert_eq!(f.mailbox.status("c")["queued"], 2);
    f.record(raids.remove("y", Reason::Left));
    assert!(f.mailbox.raids().get("y").is_none());
    assert_eq!(
        f.call(
            "c",
            "messages.send",
            json!({"key":"late","to":"y","body":"x"})
        )
        .unwrap_err(),
        absent()
    );
    // Delivered messages stay with their recipient.
    let claim = f.next("y", "fetch")["message"].clone();
    assert_eq!(claim["id"], sent["id"]);
}

#[test]
fn a_paused_raid_refuses_with_a_warning_and_notifies_on_resume() {
    use crate::raid::Reason;
    let mut f = Fixture::new();
    let (mut raids, events) = raid_of(&["g", "y", "s"]);
    f.record(events);
    f.send("y", "s", "before", "question");
    let claim = f.next("s", "fetch")["message"].clone();
    // The owner leaving pauses the raid: sends and replies between members
    // are refused with a warning, and the claim stays outstanding.
    f.record(raids.remove("g", Reason::Stopped));
    let paused = (
        -32009,
        "raid 'review' is paused until the host selects a new owner; you will get a notice when it resumes"
            .to_string(),
    );
    assert_eq!(
        f.call("y", "messages.send", json!({"key":"k","to":"s","body":"x"}))
            .unwrap_err(),
        paused
    );
    assert_eq!(f.reply("s", &claim, "reply").unwrap_err(), paused);
    assert_eq!(f.mailbox.status("s")["claim"], claim["id"]);
    // A refused send stores no receipt, so it can be retried with its key.
    // A new owner authorizes nothing until it is recorded.
    f.audit.raid_full = true;
    let mut failed = raids.clone();
    f.record(failed.set_owner("review", "y", "host").unwrap());
    assert!(f.mailbox.raids()["y"].paused);
    assert_eq!(f.mailbox.status("y")["queued"], 0);
    f.audit.raid_full = false;
    let mut f2 = Fixture::new();
    let (mut raids2, events) = raid_of(&["g", "y", "s"]);
    f2.record(events);
    f2.record(raids2.remove("g", Reason::Left));
    let _ = f2.call("y", "messages.send", json!({"key":"k","to":"s","body":"x"}));
    let _ = f2.call(
        "y",
        "messages.send",
        json!({"key":"k2","to":"s","body":"x"}),
    );
    f2.record(raids2.set_owner("review", "s", "host").unwrap());
    let view = &f2.mailbox.raids()["y"];
    assert_eq!((view.owner.as_deref(), view.paused), (Some("s"), false));
    // One notice for the warned sender, none for the others.
    assert_eq!(f2.mailbox.status("y")["queued"], 1);
    assert_eq!(f2.mailbox.status("s")["queued"], 0);
    let notice = f2.next("y", "notice")["message"].clone();
    assert_eq!(notice["sender"], "goblins");
    assert_eq!(
        notice["raid"],
        json!({"id":"r1","name":"review","event":"resumed","owner":"s"})
    );
    assert_eq!(
        notice["body"],
        "Raid review has resumed with owner sibling; messages to its members are allowed again."
    );
    assert_eq!(
        f2.reply("y", &notice, "r").unwrap_err().1,
        "notices take no reply; complete the claim with goblin inbox complete"
    );
    f2.complete("y", &notice, "done").unwrap();
    assert_eq!(f2.send("y", "s", "k3", "again")["recipient"], "s");
    // A later resume notifies nobody new.
    f2.record(raids2.set_owner("review", "y", "host").unwrap());
    assert_eq!(f2.mailbox.status("y")["queued"], 0);
}

#[test]
fn raid_names_and_ids_resolve_and_ambiguous_names_are_rejected() {
    let mut f = Fixture::new();
    f.record(raid_of(&["p", "y"]).1);
    assert_eq!(f.send("p", "lookout", "a", "x")["recipient"], "y");
    assert_eq!(f.send("p", "y", "b", "x")["recipient"], "y");
    // Live names are unique; should two match anyway, neither is chosen.
    f.directory.insert(
        "z".into(),
        Session {
            id: "z".into(),
            agent_name: "lookout".into(),
            stop_reason: None,
            running: true,
        },
    );
    let view = f.mailbox.raids["y"].clone();
    f.mailbox.raids.insert("z".into(), view);
    assert_eq!(
        f.call(
            "p",
            "messages.send",
            json!({"key":"c","to":"lookout","body":"x"})
        )
        .unwrap_err(),
        (-32009, "ambiguous recipient; use its ID".into())
    );
    // Roles are never matched, and non-members gain nothing.
    assert_eq!(
        f.call("x", "messages.send", json!({"key":"e","to":"p","body":"x"}))
            .unwrap_err(),
        absent()
    );
}

#[test]
fn replies_need_a_shared_raid() {
    use crate::raid::Reason;
    let mut f = Fixture::new();
    let (mut raids, events) = raid_of(&["x", "c"]);
    f.record(events);
    f.send("c", "unrelated", "ask", "question");
    let claim = f.next("x", "fetch")["message"].clone();
    f.record(raids.remove("c", Reason::Left));
    assert_eq!(
        f.reply("x", &claim, "reply").unwrap_err(),
        (
            -32009,
            "not allowed: 'scout' is no longer in this goblin's raid".into()
        )
    );
    // The claim stays outstanding and can still be completed.
    assert_eq!(f.mailbox.status("x")["claim"], claim["id"]);
    assert_eq!(f.mailbox.status("c")["queued"], 0);
    f.complete("x", &claim, "complete").unwrap();
    // Members of a shared raid, and the host, are unaffected.
    f.send("p", "helper", "down", "to a member");
    let claim = f.next("g", "g-fetch")["message"].clone();
    f.reply("g", &claim, "g-reply").unwrap();
    f.send("host", "unrelated", "host", "from host");
    let claim = f.next("x", "x-fetch")["message"].clone();
    f.reply("x", &claim, "x-reply").unwrap();
}

#[test]
fn raid_capacity_failure_closes_raid_routes_only_and_still_applies_removals() {
    use crate::raid::Reason;
    let mut f = Fixture::new();
    let (mut raids, events) = raid_of(&["c", "y"]);
    f.record(events);
    f.send("c", "y", "before", "question");
    let claim = f.next("y", "fetch")["message"].clone();
    f.audit.raid_full = true;
    let mut joined = raids.clone();
    let join = joined
        .invite("r1".into(), "review", &["x".to_string()], "host")
        .unwrap();
    f.record(join);
    // A join whose event could not be written authorizes nothing.
    assert!(f.mailbox.raids().get("x").is_none());
    assert_eq!(f.mailbox.status("c")["raid_audit_failed"], true);
    assert_eq!(f.mailbox.status("c")["audit_failed"], false);
    let unrecorded = (-32009, "raid membership could not be recorded".to_string());
    assert_eq!(
        f.call(
            "c",
            "messages.send",
            json!({"key":"raid","to":"y","body":"x"})
        )
        .unwrap_err(),
        unrecorded
    );
    assert_eq!(f.reply("y", &claim, "reply").unwrap_err(), unrecorded);
    f.send("host", "c", "host", "still works");
    // Removals apply although their event cannot be written.
    let count = f.audit.events.len();
    f.record(raids.remove("y", Reason::Stopped));
    assert_eq!(f.audit.events.len(), count);
    assert!(f.mailbox.raids().get("y").is_none());
    assert!(f.mailbox.raids().get("c").is_some());
    f.complete("y", &claim, "complete").unwrap();
}

#[test]
fn raid_write_failure_freezes_messaging() {
    use crate::raid::Reason;
    let mut f = Fixture::new();
    let (mut raids, events) = raid_of(&["c", "y"]);
    f.audit.fail = true;
    f.record(events);
    assert!(f.mailbox.raids().get("y").is_none());
    assert_eq!(f.mailbox.raids()["c"].id, "base");
    assert_eq!(f.mailbox.status("c")["audit_failed"], true);
    f.audit.fail = false;
    assert_eq!(
        f.call(
            "c",
            "messages.send",
            json!({"key":"crew","to":"chief","body":"x"})
        )
        .unwrap_err(),
        (-32009, "mailbox audit unavailable".into())
    );
    // Later additions stay unapplied; removals still apply.
    let mut f2 = Fixture::new();
    f2.record(raid_of(&["c", "y"]).1);
    f2.audit.fail = true;
    f2.record(raids.remove("c", Reason::Left));
    assert!(f2.mailbox.raids().get("c").is_none());
    assert!(f2.mailbox.raids()["y"].paused);
    assert_eq!(f2.mailbox.raids()["y"].owner, None);
}

#[test]
fn raid_events_carry_actor_and_affected_sessions_in_order() {
    use crate::raid::Reason;
    let mut f = Fixture::new();
    let mut raids = crate::raid::Raids::default();
    let mut events = raids.create("r1".into(), "review", "c", "c").unwrap();
    events.extend(raids.join("r1".into(), "review", "g", "c").unwrap());
    events.extend(
        raids
            .invite("r1".into(), "review", &["y".to_string()], "host")
            .unwrap(),
    );
    events.extend(raids.set_role("y", "builder", "host").unwrap());
    events.extend(raids.remove("g", Reason::LaunchFailed));
    events.extend(raids.remove("c", Reason::Left));
    events.extend(raids.remove("y", Reason::Stopped));
    f.record(events);
    let logged: Vec<_> = f
        .audit
        .events
        .iter()
        .map(|e| (e.kind.as_str(), e.actor.as_str(), e.sessions.clone()))
        .collect();
    let s = |v: &[&str]| v.iter().map(|x| x.to_string()).collect::<Vec<_>>();
    assert_eq!(
        logged,
        [
            ("raid.created", "c", s(&["c"])),
            ("raid.joined", "c", s(&["g"])),
            ("raid.joined", "host", s(&["y"])),
            ("raid.role", "host", s(&["y"])),
            ("raid.left", "g", s(&["g"])),
            ("raid.left", "c", s(&["c"])),
            ("raid.paused", "c", s(&["c", "y"])),
            ("raid.left", "y", s(&["y"])),
            ("raid.dissolved", "y", s(&["y"])),
        ]
    );
    assert_eq!(f.audit.events[4].data["reason"], "launch-failed");
    assert_eq!(f.audit.events[6].data["reason"], "left");
    assert_eq!(f.audit.events[8].data["reason"], "empty");
    assert_eq!(
        f.audit
            .events
            .iter()
            .map(|e| e.sequence)
            .collect::<Vec<_>>(),
        (1..=9).collect::<Vec<_>>()
    );
    assert!(f.mailbox.raids().values().all(|v| v.id == "base"));
}

fn operation(
    request: &str,
    session: &str,
    change: crate::operation::Change,
) -> crate::operation::Event {
    crate::operation::Event {
        request: request.into(),
        session: session.into(),
        actor: session.into(),
        change,
    }
}
fn requested(request: &str, session: &str) -> crate::operation::Event {
    operation(
        request,
        session,
        crate::operation::Change::Requested {
            kind: "docker".into(),
            subject: "Docker scope: work".into(),
            reason: "integration tests".into(),
            automatic: false,
        },
    )
}
fn completed(request: &str, session: &str, status: &str, notify: bool) -> crate::operation::Event {
    crate::operation::Event {
        actor: NOTICE_SENDER.into(),
        ..operation(
            request,
            session,
            crate::operation::Change::Completed {
                kind: "docker".into(),
                subject: "Docker scope: work".into(),
                status: status.into(),
                message: (status == "failed").then(|| "engine unavailable".into()),
                notify,
            },
        )
    }
}
impl Fixture {
    fn operations(&mut self, events: Vec<crate::operation::Event>) {
        self.mailbox
            .record_operations(&events, &self.directory, 1000, &mut self.audit);
    }
    fn kinds(&self) -> Vec<&str> {
        self.audit.events.iter().map(|e| e.kind.as_str()).collect()
    }
}

#[test]
fn an_operation_outcome_queues_one_correlated_notice() {
    let mut f = Fixture::new();
    f.operations(vec![requested("r1", "c")]);
    assert_eq!(
        f.mailbox.status("c")["operations"],
        json!([{"request":"r1","kind":"docker","subject":"Docker scope: work","state":"pending"}])
    );
    // Only the requester sees its outstanding operations.
    assert_eq!(f.mailbox.status("p")["operations"], json!([]));
    f.operations(vec![operation(
        "r1",
        "c",
        crate::operation::Change::Decided { approved: true },
    )]);
    assert_eq!(f.mailbox.status("c")["operations"][0]["state"], "running");
    // An empty inbox does not mean the operation is finished.
    assert_eq!(f.mailbox.status("c")["queued"], 0);
    f.operations(vec![completed("r1", "c", "ready", true)]);
    assert_eq!(f.mailbox.status("c")["operations"], json!([]));
    assert_eq!(f.mailbox.status("c")["queued"], 1);
    let event = f.audit.events.last().unwrap();
    assert_eq!(event.kind, "operation.completed");
    assert_eq!(event.actor, NOTICE_SENDER);
    assert_eq!(event.sessions, ["c"]);
    assert_eq!(event.data["status"], "ready");
    let notice_id = event.data["notice"]["id"].clone();
    // Retried or duplicated delivery queues nothing more.
    f.operations(vec![
        completed("r1", "c", "ready", true),
        requested("r1", "c"),
    ]);
    assert_eq!(f.mailbox.status("c")["queued"], 1);
    assert_eq!(f.kinds().last(), Some(&"operation.duplicate"));
    let claim = f.next("c", "fetch");
    let notice = &claim["message"];
    assert_eq!(notice["id"], notice_id);
    assert_eq!(notice["sender"], NOTICE_SENDER);
    assert_eq!(
        notice["operation"],
        json!({"request":"r1","kind":"docker","subject":"Docker scope: work","status":"ready","message":null})
    );
    assert!(notice["body"].as_str().unwrap().contains("r1"));
    let mut message = notice.clone();
    message["claim_generation"] = claim["claim_generation"].clone();
    assert_eq!(f.reply("c", &message, "reply").unwrap_err().0, -32009);
    // The refused reply left the claim outstanding.
    f.complete("c", &message, "done").unwrap();
    assert_eq!(f.next("c", "again")["message"], Value::Null);
}

#[test]
fn concurrent_outcomes_stay_with_their_own_requests() {
    let mut f = Fixture::new();
    f.operations(vec![requested("r1", "c"), requested("r2", "s")]);
    f.operations(vec![
        completed("r2", "s", "denied", true),
        completed("r1", "c", "failed", true),
    ]);
    let c = f.next("c", "c")["message"].clone();
    let s = f.next("s", "s")["message"].clone();
    assert_eq!(c["operation"]["request"], "r1");
    assert_eq!(c["operation"]["status"], "failed");
    assert_eq!(c["operation"]["message"], "engine unavailable");
    assert!(c["body"].as_str().unwrap().contains("engine unavailable"));
    assert_eq!(s["operation"]["request"], "r2");
    assert_eq!(s["operation"]["status"], "denied");
}

#[test]
fn outcomes_nobody_can_read_are_logged_without_a_notice() {
    let mut f = Fixture::new();
    f.directory.get_mut("s").unwrap().running = false;
    f.operations(vec![
        requested("plain", "c"),
        completed("plain", "c", "cancelled", false),
        requested("gone", "s"),
        completed("gone", "s", "cancelled", true),
    ]);
    let undelivered: Vec<_> = f
        .audit
        .events
        .iter()
        .filter(|e| e.kind == "operation.completed")
        .map(|e| {
            assert!(e.data["notice"].is_null());
            e.data["undelivered"].as_str().unwrap()
        })
        .collect();
    assert_eq!(
        undelivered,
        ["no agent integration", "recipient not running"]
    );
    assert_eq!(f.mailbox.status("c")["queued"], 0);
    // A later notice to a stopped recipient fails with its inbox.
    f.operations(vec![completed("late", "c", "withdrawn", true)]);
    f.mailbox.stop_recipient("c", 1000, &mut f.audit).unwrap();
    assert_eq!(f.mailbox.status("c")["queued"], 0);
}

#[test]
fn a_failed_audit_queues_no_notice() {
    let mut f = Fixture::new();
    f.audit.fail = true;
    f.operations(vec![completed("r1", "c", "ready", true)]);
    assert_eq!(f.mailbox.status("c")["queued"], 0);
    assert_eq!(f.mailbox.status("c")["audit_failed"], true);
}

#[test]
fn a_notice_the_log_cannot_hold_still_records_its_outcome() {
    let mut f = Fixture::new();
    f.audit.notice_full = true;
    f.operations(vec![completed("r1", "c", "ready", true)]);
    let event = f.audit.events.last().unwrap();
    assert_eq!(event.kind, "operation.completed");
    assert!(event.data["notice"].is_null());
    assert_eq!(
        event.data["undelivered"],
        "communications log capacity reached"
    );
    assert_eq!(f.mailbox.status("c")["queued"], 0);
    assert_eq!(f.mailbox.status("c")["audit_failed"], false);
    f.operations(vec![completed("r1", "c", "ready", true)]);
    assert_eq!(f.kinds().last(), Some(&"operation.duplicate"));
}

#[test]
fn outcomes_are_recorded_after_the_auxiliary_budget_is_spent() {
    let mut f = Fixture::new();
    f.mailbox.auxiliary_events = 16384;
    f.operations(vec![
        requested("r1", "c"),
        completed("r1", "c", "denied", false),
    ]);
    assert_eq!(f.kinds(), ["operation.completed"]);
    assert_eq!(f.mailbox.auxiliary_events, 16384);
}

#[test]
fn only_recent_outcomes_are_remembered() {
    let mut f = Fixture::new();
    let events = (0..=FINISHED)
        .map(|n| completed(&format!("r{n}"), "c", "ready", false))
        .collect();
    f.operations(events);
    assert_eq!(f.mailbox.finished.len(), FINISHED);
    assert_eq!(f.mailbox.finished_order.len(), FINISHED);
    assert!(!f.mailbox.finished.contains("r0"));
    let recorded = f.audit.events.len();
    f.operations(vec![completed(
        &format!("r{FINISHED}"),
        "c",
        "ready",
        false,
    )]);
    assert_eq!(f.kinds().last(), Some(&"operation.duplicate"));
    assert_eq!(f.audit.events.len(), recorded + 1);
}

#[test]
fn other_events_leave_queued_messages_room_to_be_fetched_and_completed() {
    let mut f = Fixture::new();
    f.send("p", "c", "one", "first");
    f.send("p", "c", "two", "second");
    // Exactly the reserve for two queued messages remains.
    f.audit.limit = Some(f.audit.bytes + 2 * 2 * 2048);
    let recorded = f.audit.events.len();
    for n in 0..10 {
        f.operations(vec![completed(&format!("r{n}"), "c", "denied", false)]);
        let _ = f.mailbox.record(
            "c",
            "integration.wake_attempt",
            json!({"attempt":n}),
            1000,
            &mut f.audit,
        );
        f.mailbox.record_raid(
            &crate::raid::Raids::default()
                .invite(format!("raid{n}"), "review", &["c".into()], "host")
                .unwrap(),
            &f.directory,
            1000,
            &mut f.audit,
        );
    }
    assert_eq!(f.audit.events.len(), recorded);
    assert_eq!(f.mailbox.status("c")["audit_failed"], false);
    for key in ["first", "second"] {
        let claim = f.next("c", key);
        let mut message = claim["message"].clone();
        message["claim_generation"] = claim["claim_generation"].clone();
        f.complete("c", &message, &format!("{key}-done")).unwrap();
    }
}
