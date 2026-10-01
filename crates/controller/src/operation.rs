//! Lifecycle events of Goblins-owned operations that a sandbox requested:
//! package grants, Docker attachment and dev shell refreshes. The controller
//! queues them in order; the messaging worker records them in the
//! communications log and turns each terminal outcome into one inbox notice
//! for the requesting goblin, so its integration can resume it.
use serde_json::{Value, json};

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Change {
    /// The request record exists. `automatic` requests need no decision.
    Requested {
        kind: String,
        subject: String,
        reason: String,
        automatic: bool,
    },
    /// A decision ended the approval wait. An approval starts execution.
    Decided { approved: bool },
    /// A terminal outcome. `notify` is false for sessions without an agent
    /// integration; nobody would be woken to read a notice.
    Completed {
        kind: String,
        subject: String,
        status: String,
        message: Option<String>,
        notify: bool,
    },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Event {
    pub request: String,
    /// The requesting session.
    pub session: String,
    /// The session, `host`, or `goblins` for the daemon itself.
    pub actor: String,
    pub change: Change,
}

impl Event {
    pub fn kind(&self) -> &'static str {
        match self.change {
            Change::Requested { .. } => "operation.requested",
            Change::Decided { .. } => "operation.decided",
            Change::Completed { .. } => "operation.completed",
        }
    }
    pub fn data(&self) -> Value {
        match &self.change {
            Change::Requested {
                kind,
                subject,
                reason,
                automatic,
            } => json!({"request":self.request,"kind":kind,"subject":subject,
                "reason":reason,"automatic":automatic}),
            Change::Decided { approved } => json!({"request":self.request,"approved":approved}),
            Change::Completed {
                kind,
                subject,
                status,
                message,
                ..
            } => json!({"request":self.request,"kind":kind,"subject":subject,
                "status":status,"message":message}),
        }
    }
}

/// The notice body. Structured fields are in `Message::operation`.
pub fn notice(
    request: &str,
    kind: &str,
    subject: &str,
    status: &str,
    message: Option<&str>,
) -> String {
    let what = match kind {
        "docker" => format!("Docker attachment ({subject})"),
        "package" => format!("package request '{subject}'"),
        "devshell" => format!("dev shell {subject}"),
        other => format!("{other} request '{subject}'"),
    };
    let outcome = match status {
        "ready" => "succeeded",
        "denied" => "was denied by the host",
        "failed" => "failed",
        "withdrawn" => "was withdrawn before a decision because its request connection closed",
        "cancelled" => "was cancelled",
        other => other,
    };
    let detail = message.map(|m| format!(": {m}")).unwrap_or_default();
    format!(
        "Goblins operation {request}: {what} {outcome}{detail}.\n\
         This notice needs no reply. Complete it with goblins inbox complete, then \
         continue the work that waited for this result. If you already handled the \
         result, completing the notice is all that is needed."
    )
}
