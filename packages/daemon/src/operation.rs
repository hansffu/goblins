//! Lifecycle events of Goblins-owned operations that a sandbox requested:
//! package grants, Docker attachment and dev shell refreshes. The controller
//! queues them in order; the messaging worker records them in the
//! communications log and turns each terminal outcome into one inbox notice
//! for the requesting goblin, so its integration can resume it.
use goblins_protocol::contract::{self, CommunicationKind, CommunicationPayload};
use serde_json::Value;

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
            Change::Requested { .. } => contract::communication_events::OperationRequested::KIND,
            Change::Decided { .. } => contract::communication_events::OperationDecided::KIND,
            Change::Completed { .. } => contract::communication_events::OperationCompleted::KIND,
        }
    }
    pub fn payload(
        &self,
        notice: Option<contract::MailboxMessage>,
        undelivered: Option<String>,
    ) -> CommunicationPayload {
        use contract::communication_events as kinds;
        match &self.change {
            Change::Requested {
                kind,
                subject,
                reason,
                automatic,
            } => CommunicationPayload::new::<kinds::OperationRequested>(
                contract::CommOperationRequestedData {
                    request: self.request.clone(),
                    kind: kind.clone(),
                    subject: subject.clone(),
                    reason: reason.clone(),
                    automatic: *automatic,
                },
            ),
            Change::Decided { approved } => CommunicationPayload::new::<kinds::OperationDecided>(
                contract::CommOperationDecidedData {
                    request: self.request.clone(),
                    approved: *approved,
                },
            ),
            Change::Completed {
                kind,
                subject,
                status,
                message,
                ..
            } => CommunicationPayload::new::<kinds::OperationCompleted>(
                contract::CommOperationCompletedData {
                    request: self.request.clone(),
                    kind: kind.clone(),
                    subject: subject.clone(),
                    status: status.clone(),
                    message: message.clone(),
                    notice,
                    undelivered,
                },
            ),
        }
    }
    /// The pre-publication completion base remains separate from a logged outcome.
    pub fn data(&self) -> Value {
        match &self.change {
            Change::Completed {
                kind,
                subject,
                status,
                message,
                ..
            } => serde_json::json!(contract::CommOperationCompletedCore {
                request: self.request.clone(),
                kind: kind.clone(),
                subject: subject.clone(),
                status: status.clone(),
                message: message.clone()
            }),
            _ => self.payload(None, None).into_data(),
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
        "host-command" => format!("host command {subject}"),
        "terminal" => format!("terminal {subject}"),
        other => format!("{other} request '{subject}'"),
    };
    let outcome = match status {
        "ready" => "succeeded",
        "finished" => "finished",
        "denied" => "was denied by the host",
        "failed" => "failed",
        "withdrawn" => "was withdrawn before a decision because its request connection closed",
        "cancelled" => "was cancelled",
        other => other,
    };
    let detail = message.map(|m| format!(": {m}")).unwrap_or_default();
    // A terminal's message ends with the lines of its screen.
    let end = if kind == "terminal" { "\n\n" } else { ".\n" };
    format!(
        "Goblins operation {request}: {what} {outcome}{detail}{end}\
         This notice needs no reply. Complete it with goblin inbox complete, then \
         continue the work that waited for this result. If you already handled the \
         result, completing the notice is all that is needed."
    )
}

#[cfg(test)]
mod tests {
    #[test]
    fn generated_operation_payloads_match_original_core_and_notice_variants() {
        let mut cases = vec![
            ("operation.decided", Change::Decided { approved: true }),
            ("operation.decided", Change::Decided { approved: false }),
        ];
        for automatic in [true, false] {
            cases.push((
                "operation.requested",
                Change::Requested {
                    kind: "future".into(),
                    subject: "subject".into(),
                    reason: "why".into(),
                    automatic,
                },
            ));
        }
        for status in ["ready", "finished", "exited", "future", ""] {
            for message in [None, Some("detail".to_owned())] {
                cases.push((
                    "operation.completed",
                    Change::Completed {
                        kind: "future".into(),
                        subject: "subject".into(),
                        status: status.into(),
                        message,
                        notify: true,
                    },
                ));
            }
        }
        let fixture: Value =
            serde_json::from_str(include_str!("../../../libs/api/schema/fixtures.json")).unwrap();
        let notice: contract::MailboxMessage =
            serde_json::from_value(fixture["mailbox_message"].clone()).unwrap();
        for (kind, change) in cases {
            let event = Event {
                request: "p".into(),
                session: "s".into(),
                actor: "host".into(),
                change,
            };
            let old = original_data(&event);
            assert_eq!(event.kind(), kind);
            assert_eq!(event.data(), old);
            let mut notice = notice.clone();
            notice.sender = "goblins".into();
            notice.sender_name = "goblins".into();
            notice.recipient = event.session.clone();
            if let Change::Completed {
                kind,
                subject,
                status,
                message,
                ..
            } = &event.change
            {
                notice.operation = Some(contract::MailboxOperation {
                    request: event.request.clone(),
                    kind: kind.clone(),
                    subject: subject.clone(),
                    status: status.clone(),
                    message: message.clone(),
                });
            }

            for delivered in [true, false] {
                let (actual, data) = event
                    .payload(
                        if delivered {
                            Some(notice.clone())
                        } else {
                            None
                        },
                        if delivered { None } else { Some("why".into()) },
                    )
                    .into_parts();
                let mut expected = old.clone();
                if kind == "operation.completed" {
                    expected["notice"] = if delivered {
                        json!(notice)
                    } else {
                        Value::Null
                    };
                    if !delivered {
                        expected["undelivered"] = json!("why");
                    }
                }
                assert_eq!(actual, kind);
                assert_eq!(data, expected);
                assert_eq!(
                    serde_json::to_string(&data).unwrap(),
                    serde_json::to_string(&expected).unwrap()
                );
            }
        }
    }

    use serde_json::json;
    fn original_data(event: &Event) -> Value {
        match &event.change {
            Change::Requested {
                kind,
                subject,
                reason,
                automatic,
            } => json!({"request":event.request,"kind":kind,"subject":subject,
                "reason":reason,"automatic":automatic}),
            Change::Decided { approved } => json!({"request":event.request,"approved":approved}),
            Change::Completed {
                kind,
                subject,
                status,
                message,
                ..
            } => json!({"request":event.request,"kind":kind,"subject":subject,
                "status":status,"message":message}),
        }
    }

    use super::*;
}
