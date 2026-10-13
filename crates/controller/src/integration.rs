//! Baseline notify-then-fetch lifecycle, independent of message contents.
use crate::mailbox::{self, Audit, Mailbox};
use goblins_protocol::AgentState;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::BTreeMap;

pub use goblins_protocol::INTEGRATION_PROMPT as PROMPT;
#[derive(Clone, Default, Serialize)]
pub struct View {
    pub revision: u64,
    pub ready: bool,
    pub input: u64,
    pub interrupted: bool,
    pub input_pending: bool,
}
#[derive(Default)]
pub struct Integrations {
    states: BTreeMap<String, Agent>,
    next_epoch: u64,
}
#[derive(Clone)]
struct Agent {
    key: String,
    driver: String,
    epoch: u64,
    state: String,
    paused: bool,
    last_seen: u64,
    last_attempt: u64,
    attempts: u8,
    input: u64,
    claim: Value,
    last_hook: String,
    head: Value,
    /// Claude's informational activity, separate from the wakeup `state`.
    activity: AgentState,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Params {
    key: Option<String>,
    driver: Option<String>,
    epoch: Option<u64>,
    event: Option<String>,
    delivery: Option<String>,
    #[serde(default)]
    active: bool,
    session: Option<String>,
    tool: Option<String>,
    notification: Option<String>,
}
/// Activity after a hook that only reports activity; `None` rejects the event.
/// Ending a wait never revives activity that a turn end already settled.
fn hook_activity(
    event: &str,
    tool: Option<&str>,
    notification: Option<&str>,
    current: AgentState,
) -> Option<AgentState> {
    use AgentState::*;
    let waiting = matches!(current, WaitingInput | WaitingApproval);
    Some(match (event, tool, notification) {
        ("StopFailure", ..) => Idle,
        ("PreToolUse", Some("AskUserQuestion" | "ExitPlanMode"), _) | ("Elicitation", ..) => {
            WaitingInput
        }
        ("PermissionRequest", ..) => WaitingApproval,
        ("PostToolUse" | "PostToolUseFailure" | "ElicitationResult", ..) if waiting => Working,
        // Claude reports an interrupted turn only through its idle reminder.
        ("Notification", _, Some("idle_prompt")) => Idle,
        ("Notification", _, Some("permission_prompt")) => WaitingApproval,
        ("Notification", _, Some("elicitation_dialog" | "elicitation_url_dialog")) => WaitingInput,
        (
            "PreToolUse" | "PostToolUse" | "PostToolUseFailure" | "ElicitationResult"
            | "Notification",
            ..,
        ) => current,
        _ => return None,
    })
}
impl Integrations {
    pub fn status(&self, actor: &str, now: u64) -> Value {
        self.states.get(actor).map_or(json!({"health":"unregistered"}), |a| {
            json!({"driver":a.driver,"epoch":a.epoch,"state":a.state,"activity":a.activity,"health":if a.paused {"paused"} else if now.saturating_sub(a.last_seen)>5000 {"degraded"} else if a.attempts>=3 {"stalled"} else {"healthy"},"attempts":a.attempts})
        })
    }
    pub fn call(
        &mut self,
        actor: &str,
        method: &str,
        params: Value,
        observation: (&View, u64),
        mailbox: &mut Mailbox,
        audit: &mut impl Audit,
    ) -> mailbox::Result<Value> {
        let (view, now) = observation;
        let p: Params = serde_json::from_value(params).map_err(|e| (-32602, e.to_string()))?;
        let target = if actor == "host" {
            p.session
                .as_deref()
                .ok_or((-32602, "session required".into()))?
        } else {
            if p.session.is_some() {
                return Err((-32602, "own integration only".into()));
            }
            actor
        };
        if method == "integration.status" {
            return Ok(self.status(target, now));
        }
        if method == "integration.register" {
            let Some(driver @ ("codex" | "claude")) = p.driver.as_deref() else {
                return Err((-32602, "unsupported driver".into()));
            };
            if actor == "host" {
                return Err((-32602, "unsupported driver".into()));
            }
            let key = p.key.ok_or((-32602, "registration key required".into()))?;
            if !goblins_protocol::identifier(&key) {
                return Err((-32602, "invalid key".into()));
            }
            if let Some(a) = self.states.get(actor) {
                if a.key == key {
                    return Ok(json!({"epoch":a.epoch}));
                }
                return Err((-32009, "driver already registered".into()));
            }
            self.next_epoch += 1;
            mailbox.record(
                actor,
                "integration.registered",
                json!({"driver":driver,"epoch":self.next_epoch}),
                now,
                audit,
            )?;
            self.states.insert(
                actor.into(),
                Agent {
                    key,
                    driver: driver.into(),
                    epoch: self.next_epoch,
                    state: "starting".into(),
                    paused: false,
                    last_seen: now,
                    last_attempt: 0,
                    attempts: 0,
                    input: view.input,
                    claim: Value::Null,
                    last_hook: String::new(),
                    head: Value::Null,
                    activity: AgentState::Unknown,
                },
            );
            return Ok(json!({"epoch":self.next_epoch,"driver":driver}));
        }
        let a = self
            .states
            .get_mut(target)
            .ok_or((-32004, "integration absent".into()))?;
        let mut next = a.clone();
        let status = mailbox.status(target);
        if next.head != status["head"] {
            next.attempts = 0;
            next.last_attempt = 0;
            next.head = status["head"].clone();
        }
        let pending = status["queued"].as_u64().unwrap_or(0) > 0 || !status["claim"].is_null();
        let mut result = json!({"continue":false,"notify":false});
        let mut event = None;
        if actor == "host" {
            match method {
                "integration.pause" => {
                    next.paused = true;
                    event = Some("integration.paused");
                }
                "integration.resume" => {
                    next.paused = false;
                    next.state = "ready".into();
                    next.attempts = 0;
                    next.last_attempt = 0;
                    next.input = view.input;
                    event = Some("integration.resumed");
                }
                _ => return Err((-32601, "host integration method unavailable".into())),
            }
        } else {
            if p.epoch != Some(a.epoch) {
                return Err((-32009, "stale driver epoch".into()));
            }
            match method {
                "integration.check" => {
                    let key = p.key.ok_or((-32602, "hook key required".into()))?;
                    if key == a.last_hook {
                        return Ok(json!({"continue":a.state=="working","notify":false}));
                    }
                    if !goblins_protocol::identifier(&key) {
                        return Err((-32602, "invalid key".into()));
                    }
                    next.last_hook = key;
                    match p.event.as_deref() {
                        Some("SessionStart") => {
                            next.state = "starting".into();
                            next.activity = AgentState::Idle;
                            if next.input != view.input {
                                next.attempts = 0;
                                next.last_attempt = 0;
                            }
                            next.input = view.input;
                        }
                        Some("UserPromptSubmit") => {
                            next.state = "working".into();
                            next.activity = AgentState::Working;
                            next.paused = false;
                            if next.input != view.input {
                                next.attempts = 0;
                                next.last_attempt = 0;
                            }
                            next.input = view.input;
                        }
                        Some("Stop") => {
                            if view.interrupted && view.input != a.input {
                                next.paused = true;
                            }
                            next.input = view.input;
                            let remind = status["claim"].is_null() || status["claim"] != a.claim;
                            let continuation = pending
                                && !p.active
                                && !next.paused
                                && !view.input_pending
                                && remind;
                            next.state = if continuation { "working" } else { "ready" }.into();
                            next.activity = if continuation {
                                AgentState::Working
                            } else {
                                AgentState::Idle
                            };
                            next.claim = status["claim"].clone();
                            result["continue"] = json!(continuation);
                        }
                        // Activity-only hooks leave the wakeup lifecycle and log alone.
                        Some(other) => {
                            next.activity = hook_activity(
                                other,
                                p.tool.as_deref(),
                                p.notification.as_deref(),
                                a.activity,
                            )
                            .ok_or((-32602, "unsupported hook event".into()))?;
                        }
                        None => return Err((-32602, "unsupported hook event".into())),
                    }
                    if matches!(
                        p.event.as_deref(),
                        Some("SessionStart" | "UserPromptSubmit" | "Stop")
                    ) {
                        event = Some("integration.hook");
                    }
                }
                "integration.tick" => {
                    let notification = match p.delivery.as_deref() {
                        None | Some("terminal") => false,
                        Some("notification") if a.driver == "claude" => true,
                        Some("notification") => {
                            return Err((-32602, "notification delivery requires Claude".into()));
                        }
                        Some(_) => return Err((-32602, "unsupported delivery mode".into())),
                    };
                    next.last_seen = now;
                    if !notification && view.interrupted && view.input != a.input {
                        next.paused = true;
                    }
                    if !pending {
                        next.attempts = 0;
                        next.claim = Value::Null;
                    }
                    // Human input after the last hook owns the composer until a
                    // submitted turn or explicit host resume. Never erase it.
                    let eligible = pending
                        && !next.paused
                        && (notification || (view.ready && view.input == next.input))
                        && matches!(next.state.as_str(), "starting" | "ready")
                        && status["audit_failed"] != true;
                    if eligible
                        && next.attempts < 3
                        && (next.last_attempt == 0
                            || now.saturating_sub(next.last_attempt) >= 30000)
                    {
                        next.attempts += 1;
                        next.last_attempt = now;
                        result["notify"] = json!(true);
                        result["delivery"] = json!(if notification {
                            "notification"
                        } else {
                            "terminal"
                        });
                        result["revision"] = json!(view.revision);
                        event = Some("integration.wake_attempt");
                    }
                }
                _ => return Err((-32601, "integration method unavailable".into())),
            }
        }
        if let Some(event) = event {
            mailbox.record(target,event,json!({"driver":next.driver,"epoch":next.epoch,"state":next.state,"attempt":next.attempts,"hook":p.event,"continue":result["continue"],"delivery":result["delivery"],"terminal_revision":view.revision}),now,audit)?;
        }
        // Claude's hooks and monitor ticks are its activity reports and heartbeat.
        if actor != "host" && next.driver == "claude" {
            result["agent_state"] = json!(next.activity);
        }
        *a = next;
        result["integration"] = self.status(target, now);
        Ok(result)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mailbox::{AuditError, Directory, Event, Limits, Session};
    use goblins_protocol::messages::{Claim, Mutation, Next, Send};
    #[derive(Default)]
    struct Log {
        events: Vec<Event>,
        fail: bool,
    }
    impl Audit for Log {
        fn append(&mut self, event: &Event, _: usize) -> Result<(), AuditError> {
            if self.fail {
                Err(AuditError::Unavailable)
            } else {
                self.events.push(event.clone());
                Ok(())
            }
        }
    }
    struct Fixture {
        state: Integrations,
        inbox: Mailbox,
        log: Log,
        view: View,
        now: u64,
    }
    impl Fixture {
        fn new() -> Self {
            let mut f = Self {
                state: Integrations::default(),
                inbox: Mailbox::new("a".repeat(32), Limits::default()).unwrap(),
                log: Log::default(),
                view: View {
                    ready: true,
                    ..View::default()
                },
                now: 1000,
            };
            f.call(
                "integration.register",
                json!({"driver":"codex","key":"register"}),
            )
            .unwrap();
            f
        }
        fn call(&mut self, method: &str, p: Value) -> mailbox::Result<Value> {
            self.state.call(
                "child",
                method,
                p,
                (&self.view, self.now),
                &mut self.inbox,
                &mut self.log,
            )
        }
        fn send(&mut self) {
            let dir = Directory::from([(
                "child".into(),
                Session {
                    id: "child".into(),
                    path: "child".into(),
                    parent: None,
                    running: true,
                },
            )]);
            self.inbox
                .execute(
                    "host",
                    Mutation::Send(Send {
                        key: "send".into(),
                        to: "child".into(),
                        body: "private body".into(),
                        conversation: None,
                    }),
                    &dir,
                    self.now,
                    &mut self.log,
                )
                .unwrap();
        }
        fn hook(&mut self, event: &str, active: bool) -> Value {
            self.now += 1;
            self.call(
                "integration.check",
                json!({"epoch":1,"key":format!("hook-{}",self.now),"event":event,"active":active}),
            )
            .unwrap()
        }
        fn tick(&mut self) -> Value {
            self.call("integration.tick", json!({"epoch":1})).unwrap()
        }
    }
    impl Fixture {
        fn directory() -> Directory {
            Directory::from([(
                "child".into(),
                Session {
                    id: "child".into(),
                    path: "child".into(),
                    parent: None,
                    running: true,
                },
            )])
        }
        /// A Goblins-owned operation finishing, as the controller reports it.
        fn finish(&mut self, request: &str, status: &str) {
            let event = crate::operation::Event {
                request: request.into(),
                session: "child".into(),
                actor: "goblins".into(),
                change: crate::operation::Change::Completed {
                    kind: "docker".into(),
                    subject: "Docker scope: work".into(),
                    status: status.into(),
                    message: None,
                    notify: true,
                },
            };
            self.inbox
                .record_operations(&[event], &Self::directory(), self.now, &mut self.log);
        }
        fn fetch_and_complete(&mut self, key: &str) -> Value {
            let claim = self
                .inbox
                .execute(
                    "child",
                    Mutation::Next(Next { key: key.into() }),
                    &Self::directory(),
                    self.now,
                    &mut self.log,
                )
                .unwrap();
            let message = claim["message"].clone();
            self.inbox
                .execute(
                    "child",
                    Mutation::Complete(Claim {
                        key: format!("{key}-done"),
                        message: message["id"].as_str().unwrap().into(),
                        claim_generation: claim["claim_generation"].as_u64().unwrap(),
                    }),
                    &Self::directory(),
                    self.now,
                    &mut self.log,
                )
                .unwrap();
            message
        }
    }
    #[test]
    fn operation_completion_wakes_an_idle_agent_with_an_empty_inbox() {
        for status in ["ready", "denied", "failed", "withdrawn", "cancelled"] {
            let mut f = Fixture::new();
            f.hook("UserPromptSubmit", false);
            // The agent yielded while its operation was pending.
            assert_eq!(f.hook("Stop", false)["continue"], false);
            assert_eq!(f.tick()["notify"], false);
            f.finish("r1", status);
            assert_eq!(f.tick()["notify"], true, "{status}");
            let notice = f.fetch_and_complete("fetch");
            assert_eq!(notice["operation"]["status"], status);
            assert_eq!(f.inbox.status("child")["queued"], 0);
        }
    }
    #[test]
    fn operation_completion_while_working_continues_at_stop_without_a_wakeup() {
        let mut f = Fixture::new();
        f.hook("UserPromptSubmit", false);
        f.finish("r1", "ready");
        // A working agent gets no terminal or notification wakeup.
        assert_eq!(f.tick()["notify"], false);
        f.now += 30001;
        assert_eq!(f.tick()["notify"], false);
        // Its turn end is held open once to fetch the notice.
        assert_eq!(f.hook("Stop", false)["continue"], true);
        assert_eq!(f.fetch_and_complete("fetch")["operation"]["request"], "r1");
        assert_eq!(f.hook("Stop", true)["continue"], false);
        assert_eq!(f.tick()["notify"], false);
        // A duplicate outcome after it was handled does not resume it again.
        f.finish("r1", "ready");
        assert_eq!(f.tick()["notify"], false);
    }
    #[test]
    fn operation_completion_respects_human_input_and_pause() {
        let mut f = Fixture::new();
        f.hook("UserPromptSubmit", false);
        f.hook("Stop", false);
        f.view.input += 1;
        f.finish("r1", "ready");
        assert_eq!(f.tick()["notify"], false);
        f.state.states.get_mut("child").unwrap().paused = true;
        f.view.input -= 1;
        assert_eq!(f.tick()["notify"], false);
        f.state.states.get_mut("child").unwrap().paused = false;
        assert_eq!(f.tick()["notify"], true);
    }
    #[test]
    fn operation_completion_reaches_claude_through_its_notification_monitor() {
        let mut f = Fixture::new();
        f.state.states.get_mut("child").unwrap().driver = "claude".into();
        f.hook("UserPromptSubmit", false);
        f.hook("Stop", false);
        // Claude's composer state does not matter for native notifications.
        f.view.ready = false;
        f.finish("r1", "denied");
        let result = f
            .call(
                "integration.tick",
                json!({"epoch":1,"delivery":"notification"}),
            )
            .unwrap();
        assert_eq!(result["notify"], true);
        assert_eq!(result["delivery"], "notification");
    }
    #[test]
    fn startup_message_wakes_after_session_start() {
        let mut f = Fixture::new();
        f.hook("SessionStart", false);
        f.send();
        assert_eq!(f.tick()["notify"], true);
    }
    #[test]
    fn claude_notification_delivery_does_not_require_an_insertable_composer() {
        let mut f = Fixture::new();
        f.state.states.get_mut("child").unwrap().driver = "claude".into();
        f.hook("SessionStart", false);
        f.send();
        f.view.ready = false;
        f.view.input += 1;
        f.view.interrupted = true;
        let result = f
            .call(
                "integration.tick",
                json!({"epoch":1,"delivery":"notification"}),
            )
            .unwrap();
        assert_eq!(result["notify"], true);
        assert_eq!(result["delivery"], "notification");
    }
    #[test]
    fn notification_delivery_is_restricted_to_claude() {
        let mut f = Fixture::new();
        assert!(
            f.call(
                "integration.tick",
                json!({"epoch":1,"delivery":"notification"}),
            )
            .is_err()
        );
    }
    #[test]
    fn empty_stop_then_arrival_and_busy_continuation_do_not_claim() {
        let mut f = Fixture::new();
        f.hook("UserPromptSubmit", false);
        assert_eq!(f.hook("Stop", false)["continue"], false);
        f.send();
        assert_eq!(f.tick()["notify"], true);
        assert_eq!(f.tick()["notify"], false);
        assert_eq!(f.inbox.status("child")["queued"], 1);
        assert!(f.inbox.status("child")["claim"].is_null());
        f.hook("UserPromptSubmit", false);
        f.now += 30001;
        assert_eq!(f.tick()["notify"], false);
        assert_eq!(f.hook("Stop", false)["continue"], true);
        assert_eq!(f.hook("Stop", true)["continue"], false);
        assert_eq!(f.tick()["notify"], true);
        assert!(
            !f.log
                .events
                .iter()
                .filter(|e| e.kind.starts_with("integration."))
                .any(|e| e.data.to_string().contains("private body"))
        );
    }
    #[test]
    fn human_input_stale_epoch_unknown_screen_and_bounded_retries() {
        let mut f = Fixture::new();
        f.send();
        f.view.input += 1;
        assert_eq!(f.tick()["notify"], false);
        f.hook("UserPromptSubmit", false);
        f.hook("Stop", true);
        f.view.ready = false;
        assert_eq!(f.tick()["notify"], false);
        f.view.ready = true;
        assert!(f.call("integration.tick", json!({"epoch":2})).is_err());
        for _ in 0..3 {
            assert_eq!(f.tick()["notify"], true);
            f.now += 30001;
        }
        assert_eq!(f.tick()["notify"], false);
        assert_eq!(f.state.status("child", f.now)["health"], "stalled");
        f.now += 6000;
        assert_eq!(f.state.status("child", f.now)["health"], "degraded");
    }
    #[test]
    fn interruption_during_stop_stays_paused_until_a_new_turn() {
        let mut f = Fixture::new();
        f.send();
        f.hook("UserPromptSubmit", false);
        f.view.input += 1;
        f.view.interrupted = true;
        assert_eq!(f.hook("Stop", false)["continue"], false);
        assert_eq!(f.tick()["notify"], false);
        assert_eq!(f.state.status("child", f.now)["health"], "paused");
        f.hook("UserPromptSubmit", false);
        f.hook("Stop", true);
        assert_eq!(f.tick()["notify"], true);
    }
    #[test]
    fn stop_defers_while_escape_could_still_be_a_human_interrupt() {
        let mut f = Fixture::new();
        f.send();
        f.hook("UserPromptSubmit", false);
        f.view.input_pending = true;
        f.view.ready = false;
        assert_eq!(f.hook("Stop", false)["continue"], false);
        assert_eq!(f.tick()["notify"], false);
        // A complete focus report leaves the human input revision unchanged.
        f.view.input_pending = false;
        f.view.ready = true;
        assert_eq!(f.tick()["notify"], true);
    }
    impl Fixture {
        fn claude() -> Self {
            let mut f = Self::new();
            f.state.states.get_mut("child").unwrap().driver = "claude".into();
            f
        }
        fn report(&mut self, event: &str, detail: Value) -> Value {
            self.now += 1;
            let mut params = json!({"epoch":1,"key":format!("hook-{}",self.now),"event":event});
            params
                .as_object_mut()
                .unwrap()
                .extend(detail.as_object().unwrap().clone());
            self.call("integration.check", params).unwrap()["agent_state"].clone()
        }
    }
    #[test]
    fn claude_hooks_report_activity_without_touching_the_wakeup_lifecycle() {
        let mut f = Fixture::claude();
        assert_eq!(f.hook("SessionStart", false)["agent_state"], "idle");
        assert_eq!(f.hook("UserPromptSubmit", false)["agent_state"], "working");
        let logged = f.log.events.len();
        let before = (
            f.state.states["child"].state.clone(),
            f.state.states["child"].input,
        );
        // Ordinary tool calls and their ends keep working.
        assert_eq!(f.report("PreToolUse", json!({"tool":"Bash"})), "working");
        assert_eq!(f.report("PostToolUse", json!({"tool":"Bash"})), "working");
        assert_eq!(
            f.report("PreToolUse", json!({"tool":"AskUserQuestion"})),
            "waiting-input"
        );
        assert_eq!(
            f.report("PostToolUse", json!({"tool":"AskUserQuestion"})),
            "working"
        );
        assert_eq!(
            f.report("PreToolUse", json!({"tool":"ExitPlanMode"})),
            "waiting-input"
        );
        assert_eq!(
            f.report("PostToolUseFailure", json!({"tool":"ExitPlanMode"})),
            "working"
        );
        assert_eq!(f.report("Elicitation", json!({})), "waiting-input");
        assert_eq!(f.report("ElicitationResult", json!({})), "working");
        assert_eq!(
            f.report("PermissionRequest", json!({"tool":"Bash"})),
            "waiting-approval"
        );
        assert_eq!(f.report("PostToolUse", json!({"tool":"Bash"})), "working");
        assert_eq!(
            f.report("Notification", json!({"notification":"permission_prompt"})),
            "waiting-approval"
        );
        assert_eq!(
            f.report(
                "Notification",
                json!({"notification":"elicitation_url_dialog"})
            ),
            "waiting-input"
        );
        assert_eq!(
            f.report("Notification", json!({"notification":"auth_success"})),
            "waiting-input"
        );
        assert_eq!(f.report("StopFailure", json!({})), "idle");
        assert_eq!(f.log.events.len(), logged);
        assert_eq!(
            (
                f.state.states["child"].state.clone(),
                f.state.states["child"].input
            ),
            before
        );
        assert!(
            f.call(
                "integration.check",
                json!({"epoch":1,"key":"x","event":"Bogus"})
            )
            .is_err()
        );
    }
    #[test]
    fn claude_activity_settles_at_turn_ends_and_rides_on_ticks() {
        let mut f = Fixture::claude();
        f.hook("UserPromptSubmit", false);
        // A late end of a tool never revives a settled turn.
        assert_eq!(f.report("StopFailure", json!({})), "idle");
        assert_eq!(f.report("PostToolUse", json!({"tool":"Bash"})), "idle");
        f.hook("UserPromptSubmit", false);
        // Interrupted turns have no Stop; the idle reminder settles them.
        assert_eq!(
            f.report("Notification", json!({"notification":"idle_prompt"})),
            "idle"
        );
        f.send();
        f.hook("UserPromptSubmit", false);
        assert_eq!(f.hook("Stop", false)["agent_state"], "working");
        assert_eq!(f.hook("Stop", true)["agent_state"], "idle");
        let tick = f
            .call(
                "integration.tick",
                json!({"epoch":1,"delivery":"notification"}),
            )
            .unwrap();
        assert_eq!(tick["agent_state"], "idle");
        assert_eq!(f.state.status("child", f.now)["activity"], "idle");
        // Status reads and host controls are not activity reports.
        assert!(
            f.call("integration.status", json!({}))
                .unwrap()
                .get("agent_state")
                .is_none()
        );
    }
    #[test]
    fn codex_integration_reports_no_activity() {
        let mut f = Fixture::new();
        assert!(
            f.hook("UserPromptSubmit", false)
                .get("agent_state")
                .is_none()
        );
        assert!(f.tick().get("agent_state").is_none());
    }
    #[test]
    fn audit_failure_prevents_a_wakeup() {
        let mut f = Fixture::new();
        f.send();
        f.log.fail = true;
        assert!(f.call("integration.tick", json!({"epoch":1})).is_err());
        assert_eq!(f.state.status("child", f.now)["attempts"], 0);
        assert_eq!(f.inbox.status("child")["audit_failed"], true);
    }
}
