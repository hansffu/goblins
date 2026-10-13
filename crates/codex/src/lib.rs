//! Sandbox-owned delivery bookkeeping. No inbox bodies or acknowledgement authority.
use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Submission {
    pub client_id: String,
    pub queue_id: Option<String>,
    pub seen_in_history: bool,
}
#[derive(Debug, Default, Clone, Serialize, Deserialize)]
pub struct Delivery {
    pub pending: Option<Submission>,
    pub head: Value,
    pub attempts: u8,
    pub last_attempt_ms: Option<u64>,
    pub uncertain: bool,
}
impl Delivery {
    pub fn progress(&mut self, head: &Value) {
        if &self.head != head {
            self.head = head.clone();
            self.resume();
        }
    }
    /// Resume resets the retry budget, never ambiguous submission ownership.
    pub fn resume(&mut self) {
        self.attempts = 0;
        self.last_attempt_ms = None;
    }
    pub fn eligible(&self, now_ms: u64, paused: bool, inbox: &Value) -> bool {
        !paused
            && !self.uncertain
            && self.pending.is_none()
            && inbox["audit_failed"] == false
            && (inbox["queued"].as_u64().unwrap_or(0) > 0 || !inbox["claim"].is_null())
            && self.attempts < 3
            && self
                .last_attempt_ms
                .is_none_or(|at| now_ms.saturating_sub(at) >= 30_000)
    }
    pub fn begin(&mut self, client_id: String, now_ms: u64) {
        assert!(self.pending.is_none());
        self.pending = Some(Submission {
            client_id,
            queue_id: None,
            seen_in_history: false,
        });
        self.attempts += 1;
        self.last_attempt_ms = Some(now_ms);
    }
    /// A queue absence alone cannot establish whether a submission was rejected.
    pub fn reconcile(&mut self, queue: &[Value], items: &Value, idle: bool) {
        let Some(pending) = &mut self.pending else {
            return;
        };
        if let Some(q) = queue
            .iter()
            .find(|q| q["clientUserMessageId"] == pending.client_id)
        {
            pending.queue_id = q["id"].as_str().map(str::to_owned);
            self.uncertain = false;
            return;
        }
        if has_client_id(items, &pending.client_id) {
            pending.seen_in_history = true;
            self.uncertain = false;
        }
        if pending.seen_in_history {
            if idle {
                self.pending = None;
            }
        } else {
            self.uncertain = true;
        }
    }
}
pub fn has_client_id(value: &Value, id: &str) -> bool {
    match value {
        Value::Object(v) => {
            (v.get("type").is_some_and(|t| t == "userMessage")
                && v.get("clientId").is_some_and(|c| c == id))
                || v.values().any(|v| has_client_id(v, id))
        }
        Value::Array(v) => v.iter().any(|v| has_client_id(v, id)),
        _ => false,
    }
}
pub fn activity(status: &Value) -> goblins_protocol::AgentState {
    use goblins_protocol::AgentState::*;
    match status["type"].as_str() {
        Some("idle") => Idle,
        Some("active") => {
            let flags = status["activeFlags"].as_array();
            if flags.is_some_and(|f| f.iter().any(|v| v == "waitingOnUserInput")) {
                WaitingInput
            } else if flags.is_some_and(|f| f.iter().any(|v| v == "waitingOnApproval")) {
                WaitingApproval
            } else {
                Working
            }
        }
        _ => Unknown,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    fn inbox() -> Value {
        json!({"queued":1,"claim":null,"head":["m",0],"audit_failed":false})
    }
    fn items(id: &str) -> Value {
        json!({"data":[{"item":{"type":"userMessage","clientId":id}}]})
    }
    #[test]
    fn coalesces_and_bounds_retries_until_head_or_claim_progress() {
        let mut d = Delivery::default();
        let mut inbox = inbox();
        d.progress(&inbox["head"]);
        for n in 0..3 {
            let now = n * 30_000;
            assert!(d.eligible(now, false, &inbox));
            d.begin(format!("c{n}"), now);
            inbox["queued"] = json!(9);
            assert!(!d.eligible(now, false, &inbox));
            d.reconcile(&[], &items(&format!("c{n}")), true);
            assert!(!d.eligible(now + 29_999, false, &inbox));
        }
        assert!(!d.eligible(999_999, false, &inbox));
        inbox["head"] = json!(["m", 1]);
        d.progress(&inbox["head"]);
        assert!(d.eligible(999_999, false, &inbox));
    }
    #[test]
    fn uncertain_ack_reconciles_queue_and_history_without_duplicate() {
        let mut d = Delivery::default();
        d.begin("c".into(), 0);
        d.reconcile(&[], &json!({}), true);
        assert!(d.uncertain);
        d.resume();
        assert!(!d.eligible(90_000, false, &inbox()));
        d.reconcile(
            &[json!({"id":"q","clientUserMessageId":"c"})],
            &json!({}),
            false,
        );
        assert!(!d.uncertain);
        assert_eq!(d.pending.as_ref().unwrap().queue_id.as_deref(), Some("q"));
        d.reconcile(&[], &items("c"), false);
        assert!(d.pending.is_some());
        d.reconcile(&[], &items("c"), true);
        assert!(d.pending.is_none());
    }
    #[test]
    fn pause_audit_failure_and_claims() {
        let d = Delivery::default();
        let mut v = inbox();
        assert!(!d.eligible(0, true, &v));
        v["audit_failed"] = json!(true);
        assert!(!d.eligible(0, false, &v));
        v["audit_failed"] = json!(false);
        v["queued"] = json!(0);
        assert!(!d.eligible(0, false, &v));
        v["claim"] = json!("m");
        assert!(d.eligible(0, false, &v));
    }
    #[test]
    fn waiting_precedence_and_unavailable() {
        use goblins_protocol::AgentState::*;
        assert_eq!(
            activity(
                &json!({"type":"active","activeFlags":["waitingOnApproval","waitingOnUserInput"]})
            ),
            WaitingInput
        );
        assert_eq!(activity(&json!({"type":"systemError"})), Unknown);
        assert_eq!(
            activity(&json!({"type":"active","activeFlags":[]})),
            Working
        );
        assert_eq!(activity(&json!({"type":"idle"})), Idle);
    }
    #[test]
    fn persist_before_send_and_thread_switch_keeps_ownership() {
        let mut a = Delivery::default();
        a.begin("a".into(), 1);
        let restored: Delivery = serde_json::from_str(&serde_json::to_string(&a).unwrap()).unwrap();
        assert!(!restored.eligible(90_000, false, &inbox()));
        let b = Delivery::default();
        assert!(b.eligible(90_000, false, &inbox()));
        assert_eq!(restored.pending.unwrap().client_id, "a");
    }
    #[test]
    fn completed_failed_and_interrupted_reminders_all_allow_pending_work() {
        for outcome in ["completed", "failed", "interrupted"] {
            let mut d = Delivery::default();
            d.begin("c".into(), 0);
            d.reconcile(
                &[],
                &json!({"turn":{"status":outcome,"items":items("c")}}),
                true,
            );
            assert!(d.eligible(30_000, false, &inbox()));
        }
    }
}
