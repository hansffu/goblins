#[cfg(test)]
mod tests {
    use super::super::*;
    fn fixtures() -> Value {
        serde_json::from_str(include_str!("../../tests/fixtures.json")).unwrap()
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
