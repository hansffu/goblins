//! Independent original-wire equivalence checks for generated mailbox data.
#[cfg(test)]
mod tests {
    use super::super::*;
    use crate::messages::MAX_BODY;
    fn fixtures() -> Value {
        serde_json::from_str(include_str!("../../schema/fixtures.json")).unwrap()
    }
    #[allow(dead_code)]
    mod original {
        use super::*;
        #[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
        #[serde(deny_unknown_fields)]
        pub struct Send {
            pub key: String,
            pub to: String,
            pub body: String,
            pub conversation: Option<String>,
        }

        #[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
        #[serde(deny_unknown_fields)]
        pub struct Next {
            pub key: String,
        }

        #[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
        #[serde(deny_unknown_fields)]
        pub struct Claim {
            pub key: String,
            pub message: String,
            pub claim_generation: u64,
        }

        #[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
        #[serde(deny_unknown_fields)]
        pub struct Reply {
            pub key: String,
            pub message: String,
            pub claim_generation: u64,
            pub body: String,
        }

        #[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
        #[serde(deny_unknown_fields)]
        pub struct Requeue {
            pub key: String,
            pub message: String,
            pub claim_generation: u64,
            pub reason: String,
        }

        #[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
        #[serde(tag = "method", content = "params")]
        pub enum Mutation {
            #[serde(rename = "messages.send")]
            Send(Send),
            #[serde(rename = "inbox.next")]
            Next(Next),
            #[serde(rename = "messages.reply")]
            Reply(Reply),
            #[serde(rename = "inbox.complete")]
            Complete(Claim),
            #[serde(rename = "inbox.requeue")]
            Requeue(Requeue),
        }

        impl Mutation {
            pub fn parse(method: &str, params: Value) -> Result<Self, String> {
                let mutation: Self = serde_json::from_value(serde_json::json!({
                    "method": method, "params": params,
                }))
                .map_err(|e| e.to_string())?;
                mutation.validate()?;
                Ok(mutation)
            }

            pub fn method(&self) -> &'static str {
                match self {
                    Self::Send(_) => "messages.send",
                    Self::Next(_) => "inbox.next",
                    Self::Reply(_) => "messages.reply",
                    Self::Complete(_) => "inbox.complete",
                    Self::Requeue(_) => "inbox.requeue",
                }
            }

            pub fn key(&self) -> &str {
                match self {
                    Self::Send(p) => &p.key,
                    Self::Next(p) => &p.key,
                    Self::Reply(p) => &p.key,
                    Self::Complete(p) => &p.key,
                    Self::Requeue(p) => &p.key,
                }
            }

            pub fn validate(&self) -> Result<(), String> {
                if !crate::identifier(self.key()) {
                    return Err("invalid operation key".into());
                }
                match self {
                    Self::Send(p) => {
                        if p.to.is_empty()
                            || p.to.len() > 512
                            || p.conversation
                                .as_ref()
                                .is_some_and(|id| !crate::identifier(id))
                        {
                            return Err("invalid recipient or conversation".into());
                        }
                        validate_body(&p.body)?;
                    }
                    Self::Reply(p) => {
                        validate_claim(&p.message, p.claim_generation)?;
                        validate_body(&p.body)?;
                    }
                    Self::Complete(p) => validate_claim(&p.message, p.claim_generation)?,
                    Self::Requeue(p) => {
                        validate_claim(&p.message, p.claim_generation)?;
                        if p.reason.is_empty() || p.reason.len() > 512 {
                            return Err("requeue reason must contain 1..512 UTF-8 bytes".into());
                        }
                    }
                    Self::Next(_) => (),
                }
                Ok(())
            }
        }

        fn validate_claim(message: &str, generation: u64) -> Result<(), String> {
            if !crate::identifier(message) || generation == 0 {
                return Err("invalid message ID or claim generation".into());
            }
            Ok(())
        }

        pub fn validate_body(body: &str) -> Result<(), String> {
            if body.len() > MAX_BODY {
                Err("message exceeds 8 KiB of UTF-8 text".into())
            } else {
                Ok(())
            }
        }

        /// Used for sender-local file/stdin input. Never transmit a pathname to the host.
        #[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
        #[serde(rename_all = "snake_case")]
        pub enum State {
            Queued,
            Claimed,
            Completed,
            Failed,
        }

        impl State {
            pub fn unresolved(self) -> bool {
                matches!(self, Self::Queued | Self::Claimed)
            }
        }

        #[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
        pub struct Message {
            pub id: String,
            pub conversation: String,
            pub sender: String,
            pub recipient: String,
            /// Agent names when the message was accepted; `host` and `goblins` for
            /// those senders.
            #[serde(alias = "sender_path")]
            pub sender_name: String,
            #[serde(alias = "recipient_path")]
            pub recipient_name: String,
            pub in_reply_to: Option<String>,
            pub body: String,
            pub created_sequence: u64,
            /// Milliseconds since the UTC Unix epoch; event sequence is authoritative order.
            pub created_at: u64,
            pub state: State,
            pub claim_generation: u64,
            pub failure: Option<String>,
            /// Set only on daemon-authored operation completion notices.
            #[serde(default, skip_serializing_if = "Option::is_none")]
            pub operation: Option<Operation>,
            /// Set only on daemon-authored raid notices.
            #[serde(default, skip_serializing_if = "Option::is_none")]
            pub raid: Option<RaidNotice>,
        }

        /// Sender of operation completion notices. Not a session or the host.
        pub const NOTICE_SENDER: &str = "goblins";

        /// The terminal outcome of a Goblins-owned operation a sandbox requested,
        /// correlated by the daemon-issued request ID.
        #[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
        pub struct Operation {
            pub request: String,
            /// `package`, `docker` or `devshell`.
            pub kind: String,
            /// The package, Docker scope or refresh the request named.
            pub subject: String,
            /// `ready`, `denied`, `failed`, `withdrawn` or `cancelled`.
            pub status: String,
            /// The sandbox-facing explanation, never a host diagnostic.
            pub message: Option<String>,
        }

        /// A raid change a member asked to hear about, such as a paused raid it
        /// could not message resuming.
        #[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
        pub struct RaidNotice {
            pub id: String,
            pub name: String,
            /// `resumed`.
            pub event: String,
            /// Session ID of the new owner.
            pub owner: String,
        }
    }
    #[test]
    fn generated_mailbox_mutations_match_original_decode_validate_and_serialization() {
        for case in fixtures()["mailbox_mutation_cases"].as_array().unwrap() {
            let method = case["method"].as_str().unwrap();
            let params = case["params"].clone();
            let tagged = serde_json::json!({"method":method,"params":params});
            let old = serde_json::from_value::<original::Mutation>(tagged.clone());
            let new = serde_json::from_value::<crate::messages::Mutation>(tagged);
            assert_eq!(old.is_ok(), new.is_ok(), "{case}");
            assert_eq!(
                old.as_ref().err().map(ToString::to_string),
                new.as_ref().err().map(ToString::to_string),
                "{case}"
            );
            assert_eq!(new.is_ok(), case["decoded"].as_bool().unwrap(), "{case}");
            if let (Ok(old), Ok(new)) = (old, new) {
                assert_eq!(
                    serde_json::to_string(&old).unwrap(),
                    serde_json::to_string(&new).unwrap(),
                    "{case}"
                );
                assert_eq!(
                    serde_json::to_value(&old).unwrap(),
                    serde_json::to_value(&new).unwrap(),
                    "{case}"
                );
                assert_eq!(old.validate(), new.validate(), "{case}");
                assert_eq!(
                    new.validate().is_ok(),
                    case["valid"].as_bool().unwrap(),
                    "{case}"
                );
                assert_eq!(old.method(), new.method());
            }
            macro_rules! binding {
                ($host:ty,$sandbox:ty) => {{
                    let host = decode_params::<$host>(params.clone());
                    let sandbox = decode_params::<$sandbox>(params.clone());
                    assert_eq!(host.is_ok(), case["decoded"].as_bool().unwrap(), "{case}");
                    assert_eq!(host.is_ok(), sandbox.is_ok());
                    if let (Ok(host), Ok(sandbox)) = (host, sandbox) {
                        assert_eq!(
                            serde_json::to_value(host).unwrap(),
                            serde_json::to_value(sandbox).unwrap()
                        );
                    }
                }};
            }
            match method {
                "messages.send" => binding!(host::MessagesSend, sandbox::MessagesSend),
                "messages.reply" => binding!(host::MessagesReply, sandbox::MessagesReply),
                "inbox.next" => binding!(host::InboxNext, sandbox::InboxNext),
                "inbox.complete" => binding!(host::InboxComplete, sandbox::InboxComplete),
                _ => binding!(host::InboxRequeue, sandbox::InboxRequeue),
            }
        }
    }
    #[test]
    fn generated_messages_and_notices_keep_original_names_sequences_nulls_and_aliases() {
        for case in fixtures()["mailbox_message_cases"].as_array().unwrap() {
            let old = serde_json::from_value::<original::Message>(case["value"].clone());
            let new = serde_json::from_value::<MailboxMessage>(case["value"].clone());
            assert_eq!(old.is_ok(), new.is_ok(), "{case}");
            assert_eq!(
                old.as_ref().err().map(ToString::to_string),
                new.as_ref().err().map(ToString::to_string),
                "{case}"
            );
            assert_eq!(new.is_ok(), case["decoded"].as_bool().unwrap(), "{case}");
            if let (Ok(old), Ok(new)) = (old, new) {
                assert_eq!(old.state.unresolved(), new.state.unresolved());
                assert_eq!(
                    serde_json::to_string(&old).unwrap(),
                    serde_json::to_string(&new).unwrap(),
                    "{case}"
                );
                assert_eq!(
                    serde_json::to_value(old).unwrap(),
                    serde_json::to_value(new).unwrap(),
                    "{case}"
                );
            }
        }
    }
    #[test]
    fn generated_mutation_dtos_retain_original_utf8_byte_boundaries() {
        for case in fixtures()["mailbox_byte_cases"].as_array().unwrap() {
            let method = case["method"].as_str().unwrap();
            let valid = case["valid"].as_bool().unwrap();
            let mut params = case["base"].clone();
            let text = case["unit"]
                .as_str()
                .unwrap()
                .repeat(case["repeat"].as_u64().unwrap() as usize)
                + case["suffix"].as_str().unwrap();
            assert_eq!(text.len() as u64, case["bytes"].as_u64().unwrap());
            params[case["field"].as_str().unwrap()] = Value::String(text);
            let order: &[&str] = match method {
                "messages.send" => &["key", "to", "body", "conversation"],
                "messages.reply" => &["key", "message", "claim_generation", "body"],
                _ => &["key", "message", "claim_generation", "reason"],
            };
            let positional =
                Value::Array(order.iter().map(|field| params[*field].clone()).collect());
            for params in [params, positional] {
                assert_eq!(
                    original::Mutation::parse(method, params.clone()).is_ok(),
                    valid,
                    "{case}"
                );
                assert_eq!(
                    crate::messages::Mutation::parse(method, params).is_ok(),
                    valid,
                    "{case}"
                );
            }
        }
    }
    #[test]
    fn mailbox_counters_retain_original_u64_wire_decoding() {
        let fixtures = fixtures();
        for case in fixtures["mailbox_u64_cases"].as_array().unwrap() {
            let number = case["value"].clone();
            let unsigned = case["unsigned"].as_bool().unwrap();
            let claim = case["claim"].as_bool().unwrap();
            assert_eq!(
                serde_json::from_value::<u64>(number.clone()).is_ok(),
                unsigned,
                "{case}"
            );
            for (method, base, order) in [
                (
                    "messages.reply",
                    serde_json::json!({"key":"k","message":"m","body":"hello","claim_generation":number}),
                    &["key", "message", "claim_generation", "body"][..],
                ),
                (
                    "inbox.complete",
                    serde_json::json!({"key":"k","message":"m","claim_generation":number}),
                    &["key", "message", "claim_generation"][..],
                ),
                (
                    "inbox.requeue",
                    serde_json::json!({"key":"k","message":"m","reason":"later","claim_generation":number}),
                    &["key", "message", "claim_generation", "reason"][..],
                ),
            ] {
                let positional = Value::Array(order.iter().map(|key| base[*key].clone()).collect());
                for params in [base, positional] {
                    let tagged = serde_json::json!({"method":method,"params":params});
                    assert_eq!(
                        serde_json::from_value::<original::Mutation>(tagged.clone()).is_ok(),
                        unsigned,
                        "{case}"
                    );
                    assert_eq!(
                        serde_json::from_value::<crate::messages::Mutation>(tagged).is_ok(),
                        unsigned,
                        "{case}"
                    );
                    assert_eq!(
                        original::Mutation::parse(method, params.clone()).is_ok(),
                        claim,
                        "{case}"
                    );
                    assert_eq!(
                        crate::messages::Mutation::parse(method, params).is_ok(),
                        claim,
                        "{case}"
                    );
                }
            }
            for field in ["created_sequence", "created_at", "claim_generation"] {
                let mut message = fixtures["mailbox_message"].clone();
                message[field] = number.clone();
                let order = [
                    "id",
                    "conversation",
                    "sender",
                    "recipient",
                    "sender_name",
                    "recipient_name",
                    "in_reply_to",
                    "body",
                    "created_sequence",
                    "created_at",
                    "state",
                    "claim_generation",
                    "failure",
                ];
                let positional =
                    Value::Array(order.iter().map(|key| message[*key].clone()).collect());
                for value in [message, positional] {
                    let old = serde_json::from_value::<original::Message>(value.clone());
                    let new = serde_json::from_value::<MailboxMessage>(value);
                    assert_eq!(old.is_ok(), unsigned, "{field}: {case}");
                    assert_eq!(new.is_ok(), unsigned, "{field}: {case}");
                    if let (Ok(old), Ok(new)) = (old, new) {
                        assert_eq!(
                            serde_json::to_string(&old).unwrap(),
                            serde_json::to_string(&new).unwrap()
                        );
                    }
                }
            }
            // These producer fields come from the same original u64 storage;
            // successful client result bodies remain ignored as before.
            assert_eq!(
                serde_json::from_value::<MailboxRequeueResult>(
                    serde_json::json!({"message":"m","claim_generation":number})
                )
                .is_ok(),
                unsigned,
                "{case}"
            );
            let optional = unsigned || number.is_null();
            assert_eq!(
                serde_json::from_value::<Option<u64>>(number.clone()).is_ok(),
                optional,
                "{case}"
            );
            assert_eq!(
                serde_json::from_value::<MailboxNextResult>(
                    serde_json::json!({"message":null,"claim_generation":number})
                )
                .is_ok(),
                optional,
                "{case}"
            );
            assert!(IgnoredResult::decode(number).is_ok());
        }
    }
    #[test]
    fn inbox_status_requires_core_fields_and_preserves_additive_legacy_defaults() {
        for case in fixtures()["inbox_status_cases"].as_array().unwrap() {
            let result = decode_reply::<sandbox::InboxStatus>(case["value"].clone());
            assert_eq!(result.is_ok(), case["valid"].as_bool().unwrap(), "{case}");
            if let Ok(result) = result {
                assert_eq!(result.queued, case["value"]["queued"].as_u64().unwrap());
            }
        }
    }
    #[test]
    fn read_parameters_retain_original_serde_diagnostics_and_empty_id_lookup() {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Get {
            message: String,
        }
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Empty {}
        for value in [
            serde_json::json!({"message":""}),
            serde_json::json!(["m"]),
            Value::Null,
            serde_json::json!([]),
            serde_json::json!(["m", 7]),
            serde_json::json!({"message":7}),
            serde_json::json!({"message":"m","sender":"host"}),
        ] {
            let old = serde_json::from_value::<Get>(value.clone());
            let new = serde_json::from_value::<MailboxGetParams>(value);
            if let (Ok(old), Ok(new)) = (&old, &new) {
                assert_eq!(old.message, new.message);
            }
            assert_eq!(old.is_ok(), new.is_ok());
            assert_eq!(
                old.err().map(|e| e.to_string()),
                new.err().map(|e| e.to_string())
            );
        }
        for value in [
            serde_json::json!({}),
            serde_json::json!([]),
            Value::Null,
            serde_json::json!([1]),
            serde_json::json!({"session":"other"}),
        ] {
            let old = serde_json::from_value::<Empty>(value.clone());
            let new = serde_json::from_value::<MailboxStatusParams>(value);
            assert_eq!(old.is_ok(), new.is_ok());
            assert_eq!(
                old.err().map(|e| e.to_string()),
                new.err().map(|e| e.to_string())
            );
        }
    }
    #[test]
    fn generated_cli_send_preserves_original_absent_conversation_and_success_body_policy() {
        assert_eq!(
            crate::messages::send_request("k".into(), "alpha".into(), "hello".into()).unwrap(),
            serde_json::json!({"key":"k","to":"alpha","body":"hello"})
        );
        for case in fixtures()["mailbox_mutation_results"].as_array().unwrap() {
            // The CLI prints original JSON; existing clients ignore successful
            // bodies, including future or malformed values.
            for value in [
                case["result"].clone(),
                Value::Null,
                serde_json::json!(["extension"]),
            ] {
                assert!(IgnoredResult::decode(value).is_ok());
            }
        }
    }
}
