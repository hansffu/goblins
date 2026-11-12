#[cfg(test)]
mod tests {
    use super::super::*;
    fn fixtures() -> Value {
        serde_json::from_str(include_str!("../../tests/fixtures.json")).unwrap()
    }
    fn page_size() -> usize {
        100
    }
    #[derive(Deserialize, Serialize)]
    #[serde(deny_unknown_fields)]
    struct List {
        #[serde(default)]
        after: u64,
        #[serde(default = "page_size")]
        limit: usize,
        session: Option<String>,
    }
    #[derive(Deserialize, Serialize)]
    struct Event {
        instance: String,
        sequence: u64,
        timestamp: u64,
        kind: String,
        actor: String,
        sessions: Vec<String>,
        data: Value,
    }
    #[test]
    fn communications_params_keep_original_defaults_sequences_diagnostics_and_values() {
        for value in fixtures()["communications_parameter_cases"]
            .as_array()
            .unwrap()
        {
            let old = serde_json::from_value::<List>(value.clone());
            let new = serde_json::from_value::<CommunicationsListParams>(value.clone());
            assert_eq!(old.is_ok(), new.is_ok(), "{value}");
            assert_eq!(
                old.as_ref().err().map(ToString::to_string),
                new.as_ref().err().map(ToString::to_string),
                "{value}"
            );
            if let (Ok(old), Ok(new)) = (old, new) {
                assert_eq!(
                    serde_json::to_string(&old).unwrap(),
                    serde_json::to_string(&new).unwrap(),
                    "{value}"
                );
            }
            assert_eq!(
                decode_params::<host::CommunicationsList>(value.clone()).is_ok(),
                serde_json::from_value::<List>(value.clone()).is_ok()
            );
        }
    }
    #[test]
    fn event_envelopes_keep_original_stored_bytes_and_serde_readers() {
        let base = fixtures()["communications_event"].clone();
        let order = [
            "instance",
            "sequence",
            "timestamp",
            "kind",
            "actor",
            "sessions",
            "data",
        ];
        let seq = Value::Array(order.iter().map(|k| base[*k].clone()).collect());
        for value in [
            base.clone(),
            seq,
            serde_json::json!(["i", 1]),
            Value::Null,
            {
                let mut v = base.clone();
                v["timestamp"] = serde_json::json!(1.0);
                v
            },
            {
                let mut v = base.clone();
                v["extra"] = serde_json::json!(true);
                v
            },
        ] {
            let old = serde_json::from_value::<Event>(value.clone());
            let new = serde_json::from_value::<CommunicationsEvent>(value.clone());
            assert_eq!(old.is_ok(), new.is_ok(), "{value}");
            assert_eq!(
                old.as_ref().err().map(ToString::to_string),
                new.as_ref().err().map(ToString::to_string),
                "{value}"
            );
            if let (Ok(old), Ok(new)) = (old, new) {
                assert_eq!(
                    serde_json::to_string(&old).unwrap(),
                    serde_json::to_string(&new).unwrap()
                );
            }
        }
    }
    #[test]
    fn page_reply_projection_retains_independent_cli_field_fallbacks() {
        for value in fixtures()["communications_page_cases"].as_array().unwrap() {
            let new = decode_reply::<host::CommunicationsList>(value.clone()).unwrap();
            assert_eq!(new.events.as_ref(), value["events"].as_array());
            assert_eq!(new.cursor, value["cursor"].as_u64());
            assert_eq!(new.latest, value["latest"].as_u64());
            assert_eq!(new.session.as_deref(), value["session"].as_str());
        }
        let wire =
            profile_params::<host_param_profiles::CommunicationsCli>(CommunicationsCliParams {
                after: 0,
                session: None,
            })
            .unwrap();
        assert_eq!(wire, serde_json::json!({"after":0,"session":null}));
    }
}
