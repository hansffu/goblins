//! Readers for permission records.
use super::*;

impl Response for PermissionRecordReply {
    const SCHEMA: &'static str = "PermissionRecordReply";
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
