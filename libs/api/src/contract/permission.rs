//! Compatible readers for retained permission records.
use super::*;

pub const RECORD_ORDER: &[&str] = &[
    "kind",
    "id",
    "session",
    "agent_name",
    "approval",
    "package",
    "reason",
    "state",
    "approved",
    "preview",
    "message",
    "name",
    "started_at",
    "log",
];

impl Response for PermissionRecordReply {
    const SCHEMA: &'static str = "PermissionRecordReply";
    fn decode(value: Value) -> io::Result<Self> {
        // The original record's final non-default sequence slot was message.
        // Named Option fields could be omitted; kind defaulted to package.
        decode_ordered(value, RECORD_ORDER, 11).map_err(io::Error::other)
    }
}
impl Response for PermissionListReply {
    const SCHEMA: &'static str = "PermissionListReply";
    fn decode(value: Value) -> io::Result<Self> {
        let values = value
            .as_array()
            .ok_or_else(|| io::Error::other("invalid permission list"))?;
        values
            .iter()
            .cloned()
            .map(PermissionRecordReply::decode)
            .collect::<io::Result<Vec<_>>>()
            .map(Self)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn fixtures() -> Value {
        serde_json::from_str(include_str!("../../tests/fixtures.json")).unwrap()
    }
    #[derive(Deserialize, Serialize)]
    struct OldRecord {
        #[serde(default = "crate::package_kind")]
        kind: String,
        id: String,
        session: String,
        agent_name: String,
        approval: String,
        package: String,
        reason: String,
        state: String,
        approved: Option<bool>,
        preview: Option<Value>,
        message: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        name: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        started_at: Option<u64>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        log: Option<String>,
    }
    #[test]
    fn generated_permission_record_reader_matches_original_serde_and_wire_presence() {
        for case in fixtures()["permission_record_cases"].as_array().unwrap() {
            let wire = case["result"].clone();
            let old = serde_json::from_value::<OldRecord>(wire.clone());
            let new = PermissionRecordReply::decode(wire.clone());
            assert_eq!(old.is_ok(), new.is_ok(), "{case}");
            assert_eq!(new.is_ok(), case["legacy"].as_bool().unwrap(), "{case}");
            if let (Ok(old), Ok(new)) = (old, new) {
                assert_eq!(
                    serde_json::to_value(old).unwrap(),
                    serde_json::to_value(new).unwrap(),
                    "{case}"
                );
            }
        }
        for record in fixtures()["permission_records"].as_array().unwrap() {
            let dto: PermissionRecord = serde_json::from_value(record.clone()).unwrap();
            assert_eq!(serde_json::to_value(dto).unwrap(), *record);
        }
    }
    #[test]
    fn generated_permission_read_requests_keep_original_positional_rules() {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct OldFilter {
            session: Option<String>,
            state: Option<String>,
        }
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct OldGet {
            request: String,
        }
        for case in fixtures()["permission_read_parameter_cases"]
            .as_array()
            .unwrap()
        {
            let value = case["params"].clone();
            let valid = if case["method"] == "permissions.list" {
                let old = serde_json::from_value::<OldFilter>(value.clone());
                let new = decode_params::<host::PermissionsList>(value);
                assert_eq!(old.is_ok(), new.is_ok(), "{case}");
                if let (Ok(old), Ok(new)) = (&old, &new) {
                    assert_eq!(old.session, new.session);
                    assert_eq!(old.state, new.state);
                }
                new.is_ok()
            } else {
                let old = serde_json::from_value::<OldGet>(value.clone());
                let new = decode_params::<host::PermissionsGet>(value);
                assert_eq!(old.is_ok(), new.is_ok(), "{case}");
                if let (Ok(old), Ok(new)) = (&old, &new) {
                    assert_eq!(old.request, new.request);
                }
                new.is_ok()
            };
            assert_eq!(valid, case["valid"].as_bool().unwrap(), "{case}");
        }
    }
    #[test]
    fn generated_permission_reads_bind_methods_and_compatible_record_results() {
        let records = fixtures()["permission_records"].clone();
        let reply = invoke::<host::PermissionsList, io::Error>(
            PermissionListParams {
                session: None,
                state: Some("future".into()),
            },
            |name, params| {
                assert_eq!(name, "permissions.list");
                assert_eq!(params, serde_json::json!({"state":"future"}));
                Ok(records.clone())
            },
        )
        .unwrap();
        assert_eq!(reply.0.len(), records.as_array().unwrap().len());
        let reply = invoke::<host::PermissionsGet, io::Error>(
            PermissionGetParams {
                request: "not an identifier".into(),
            },
            |name, params| {
                assert_eq!(name, "permissions.get");
                assert_eq!(params, serde_json::json!({"request":"not an identifier"}));
                Ok(records[0].clone())
            },
        )
        .unwrap();
        assert_eq!(reply.id, records[0]["id"].as_str().unwrap());
        for invalid in [Value::Null, serde_json::json!({}), serde_json::json!([{}])] {
            assert!(PermissionListReply::decode(invalid).is_err());
        }
    }
}
