//! Current mailbox wire behavior and boundary checks.
#[cfg(test)]
mod tests {
    use super::super::*;
    fn fixtures() -> Value {
        serde_json::from_str(include_str!("../../tests/fixtures.json")).unwrap()
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
                        serde_json::from_value::<crate::messages::Mutation>(tagged).is_ok(),
                        unsigned,
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
                    let message = serde_json::from_value::<MailboxMessage>(value);
                    assert_eq!(message.is_ok(), unsigned, "{field}: {case}");
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
