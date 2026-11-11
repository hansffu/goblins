//! Schema-generated RPC contracts. Transport and authority remain handwritten.
use goblins_jsonrpc as rpc;
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use serde_json::Value;
use std::{io, os::unix::net::UnixStream};

/// Closed empty parameter objects need a struct, rather than a generated map.
#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct EmptyParams {}

pub trait Method {
    type Params: Serialize + DeserializeOwned;
    type Produced: Serialize + DeserializeOwned;
    type Result: Serialize + Response;
    const NAME: &'static str;
}

mod generated {
    use super::EmptyParams;
    include!(concat!(env!("OUT_DIR"), "/contract.rs"));
}
pub use generated::*;

/// Boolean discriminants must select a variant before its open record decodes.
#[derive(Clone, Debug, Serialize)]
#[serde(untagged)]
pub enum HistoryStatus {
    OwnedHistory(OwnedHistory),
    UnavailableHistory(UnavailableHistory),
}

impl<'de> Deserialize<'de> for HistoryStatus {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let value = Value::deserialize(deserializer)?;
        require(&value, &["directory"]).map_err(serde::de::Error::custom)?;
        match value.get("owner").and_then(Value::as_bool) {
            Some(true) => serde_json::from_value(value).map(Self::OwnedHistory),
            Some(false) => serde_json::from_value(value).map(Self::UnavailableHistory),
            None => return Err(serde::de::Error::custom("invalid history owner")),
        }
        .map_err(serde::de::Error::custom)
    }
}

/// Named fallbacks for constraints not enforced by Typify 0.7's Serde output.
pub trait Response: DeserializeOwned {
    const SCHEMA: &'static str;
    fn decode(value: Value) -> io::Result<Self> {
        validate_result_shape(&value, Self::SCHEMA)?;
        serde_json::from_value(value).map_err(io::Error::other)
    }
}

/// Serde structs also accept arrays. Results use schema object shapes, including
/// nested records, while request decoding deliberately retains positional forms.
/// This guard checks containers, required presence and union discriminants;
/// scalar bounds and other semantic constraints remain generated/named validators.
fn validate_result_shape(value: &Value, name: &str) -> io::Result<()> {
    static SCHEMA: std::sync::OnceLock<Value> = std::sync::OnceLock::new();
    let common =
        SCHEMA.get_or_init(|| serde_json::from_str(include_str!("../schema/common.json")).unwrap());
    fn check(value: &Value, schema: &Value, common: &Value) -> io::Result<()> {
        let bad = || io::Error::other("invalid result shape");
        if let Some(reference) = schema.get("$ref").and_then(Value::as_str) {
            return check(
                value,
                common
                    .pointer(reference.strip_prefix('#').ok_or_else(bad)?)
                    .ok_or_else(bad)?,
                common,
            );
        }
        if let Some(variants) = schema
            .get("anyOf")
            .or_else(|| schema.get("oneOf"))
            .and_then(Value::as_array)
        {
            return if variants.iter().any(|s| check(value, s, common).is_ok()) {
                Ok(())
            } else {
                Err(bad())
            };
        }
        if let Some(allowed) = schema.get("enum").and_then(Value::as_array) {
            if !allowed.contains(value) {
                return Err(bad());
            }
        }
        match schema.get("type").and_then(Value::as_str) {
            Some("object") => {
                let object = value.as_object().ok_or_else(bad)?;
                if let Some(required) = schema.get("required").and_then(Value::as_array) {
                    for name in required {
                        if !object.contains_key(name.as_str().ok_or_else(bad)?) {
                            return Err(bad());
                        }
                    }
                }
                for (name, value) in object {
                    if let Some(property) = schema.get("properties").and_then(|p| p.get(name)) {
                        check(value, property, common)?;
                    } else if let Some(additional) =
                        schema.get("additionalProperties").filter(|s| s.is_object())
                    {
                        check(value, additional, common)?;
                    }
                }
            }
            Some("array") => {
                let values = value.as_array().ok_or_else(bad)?;
                if let Some(items) = schema.get("items").filter(|s| s.is_object()) {
                    for item in values {
                        check(item, items, common)?;
                    }
                }
            }
            Some("null") if !value.is_null() => return Err(bad()),
            Some("string") if !value.is_string() => return Err(bad()),
            Some("boolean") if !value.is_boolean() => return Err(bad()),
            Some("integer") if !(value.is_i64() || value.is_u64()) => return Err(bad()),
            Some("number") if !value.is_number() => return Err(bad()),
            _ => (),
        }
        Ok(())
    }
    check(
        value,
        common["definitions"]
            .get(name)
            .ok_or_else(|| io::Error::other("unknown result schema"))?,
        common,
    )
}

impl Response for SelfStatus {
    const SCHEMA: &'static str = "SelfStatus";
    fn decode(value: Value) -> io::Result<Self> {
        validate_result_shape(&value, Self::SCHEMA)?;
        require(
            &value,
            &[
                "scope",
                "dev_shell",
                "attached_since",
                "permission_bypass",
                "raid",
                "worktree",
                "created_by",
            ],
        )?;
        if !value["raid"].is_null() {
            require(&value["raid"], &["role", "owner", "owner_name"])?;
        }
        serde_json::from_value(value).map_err(io::Error::other)
    }
}

impl Response for SetVariablesResult {
    const SCHEMA: &'static str = "SetVariablesResult";
    fn decode(value: Value) -> io::Result<Self> {
        validate_result_shape(&value, Self::SCHEMA)?;
        if value.get("accepted") != Some(&Value::Bool(true)) {
            return Err(io::Error::other("invalid acknowledgement"));
        }
        serde_json::from_value(value).map_err(io::Error::other)
    }
}

fn require(value: &Value, fields: &[&str]) -> io::Result<()> {
    if fields.iter().any(|field| value.get(field).is_none()) {
        return Err(io::Error::other("missing required result field"));
    }
    Ok(())
}

impl Response for InitializeResult {
    const SCHEMA: &'static str = "InitializeResult";
    fn decode(value: Value) -> io::Result<Self> {
        validate_result_shape(&value, Self::SCHEMA)?;
        require(&value, &["configuration"])?;
        if !value["limits"].is_object() {
            return Err(io::Error::other("invalid initialization limits"));
        }
        let result: Self = serde_json::from_value(value).map_err(io::Error::other)?;
        if *result.api != API_VERSION
            || (result.role == EndpointRole::Host && result.configuration.is_some())
        {
            return Err(io::Error::other("incompatible daemon"));
        }
        Ok(result)
    }
}

impl Response for InitializeReply {
    const SCHEMA: &'static str = "InitializeReply";
    fn decode(value: Value) -> io::Result<Self> {
        validate_result_shape(&value, Self::SCHEMA)?;
        require(&value, &["api", "instance", "role", "features"])?;
        if value
            .get("limits")
            .is_some_and(|v| !v.is_null() && !v.is_object())
        {
            return Err(io::Error::other("invalid initialization limits"));
        }
        let result: Self = serde_json::from_value(value).map_err(io::Error::other)?;
        if *result.api != API_VERSION
            || (result.role == EndpointRole::Host && result.configuration.is_some())
        {
            return Err(io::Error::other("incompatible daemon"));
        }
        Ok(result)
    }
}

impl Response for ServerStatusReply {
    const SCHEMA: &'static str = "ServerStatusReply";
    fn decode(value: Value) -> io::Result<Self> {
        validate_result_shape(&value, Self::SCHEMA)?;
        require(&value, &["instance", "state", "sessions"])?;
        if value
            .get("history")
            .is_some_and(|v| !v.is_null() && !v.is_object())
        {
            return Err(io::Error::other("invalid history metadata"));
        }
        serde_json::from_value(value).map_err(io::Error::other)
    }
}

impl Response for ServerStatusResult {
    const SCHEMA: &'static str = "ServerStatusResult";
    fn decode(value: Value) -> io::Result<Self> {
        validate_result_shape(&value, Self::SCHEMA)?;
        require(&value, &["manifest", "worktrees_directory"])?;
        require(&value["history"], &["directory"])?;
        let result: Self = serde_json::from_value(value).map_err(io::Error::other)?;
        let valid = match &result.history {
            HistoryStatus::OwnedHistory(history) => history.owner,
            HistoryStatus::UnavailableHistory(history) => !history.owner,
        };
        if !valid {
            return Err(io::Error::other("invalid history ownership variant"));
        }
        Ok(result)
    }
}

impl Response for ServerStopResult {
    const SCHEMA: &'static str = "ServerStopResult";
    fn decode(value: Value) -> io::Result<Self> {
        validate_result_shape(&value, Self::SCHEMA)?;
        require(&value, &["accepted"])?;
        let result: Self = serde_json::from_value(value).map_err(io::Error::other)?;
        if !result.accepted {
            return Err(io::Error::other("invalid shutdown acknowledgement"));
        }
        Ok(result)
    }
}

/// Preserve the existing framing/correlation/error checks, then decode the DTO.
pub fn exchange<M: Method>(
    stream: &mut UnixStream,
    id: Value,
    params: M::Params,
) -> io::Result<M::Result> {
    let reply = rpc::exchange(stream, id, M::NAME, serde_json::to_value(params)?)?;
    M::Result::decode(reply)
}

/// Shared handshake; the caller supplies its endpoint, request ID and socket.
pub fn initialize(
    stream: &mut UnixStream,
    id: Value,
    role: EndpointRole,
) -> io::Result<InitializeReply> {
    let params = InitializeParams { api: API_VERSION };
    let result = match role {
        EndpointRole::Host => exchange::<host::Initialize>(stream, id, params)?,
        EndpointRole::Sandbox => exchange::<sandbox::Initialize>(stream, id, params)?,
    };
    if *result.api != API_VERSION || result.role != role {
        return Err(io::Error::other("incompatible daemon"));
    }
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn fixtures() -> Value {
        serde_json::from_str(include_str!("../schema/fixtures.json")).unwrap()
    }

    #[test]
    fn parameters_preserve_named_positional_and_refusal_behavior() {
        for case in fixtures()["parameter_cases"].as_array().unwrap() {
            let params = case["params"].clone();
            let valid = if case["method"] == "initialize" {
                serde_json::from_value::<InitializeParams>(params).is_ok()
            } else {
                serde_json::from_value::<EmptyParams>(params).is_ok()
            };
            assert_eq!(valid, case["valid"].as_bool().unwrap(), "{case}");
        }
    }

    #[test]
    fn generated_wire_results_round_trip_contract_fixtures() {
        let fixtures = fixtures();
        for name in ["host_initialize", "sandbox_initialize"] {
            let value = fixtures[name].clone();
            assert_eq!(
                json!(InitializeResult::decode(value.clone()).unwrap()),
                value
            );
        }
        for name in ["server_status", "server_status_unavailable"] {
            let value = fixtures[name].clone();
            assert_eq!(
                json!(ServerStatusResult::decode(value.clone()).unwrap()),
                value
            );
        }
        assert_eq!(
            json!(ServerStopResult::decode(fixtures["server_stop"].clone()).unwrap()),
            fixtures["server_stop"]
        );
    }

    #[test]
    fn result_extensions_are_ignored_without_confusing_history_variants() {
        let fixture = fixtures();
        let extended = &fixture["forward_compatibility"];
        for name in ["host_initialize", "sandbox_initialize"] {
            assert_eq!(
                json!(InitializeResult::decode(extended[name].clone()).unwrap()),
                fixture[name]
            );
        }
        for name in ["server_status", "server_status_unavailable"] {
            assert_eq!(
                json!(ServerStatusResult::decode(extended[name].clone()).unwrap()),
                fixture[name]
            );
        }
        assert_eq!(
            json!(ServerStopResult::decode(extended["server_stop"].clone()).unwrap()),
            fixture["server_stop"]
        );
        let value =
            ServerStatusResult::decode(extended["server_status_unavailable"].clone()).unwrap();
        assert!(matches!(
            value.history,
            HistoryStatus::UnavailableHistory(_)
        ));
    }

    #[test]
    fn historical_api1_replies_work_through_the_framed_handshake() {
        use std::io::Write;
        let historical = fixtures()["historical_api1"].clone();
        for (name, role) in [
            ("host_initialize", EndpointRole::Host),
            ("sandbox_initialize", EndpointRole::Sandbox),
        ] {
            let value = historical[name].clone();
            let (mut client, mut peer) = UnixStream::pair().unwrap();
            let server = std::thread::spawn(move || {
                let request = rpc::read(&mut peer).unwrap();
                assert_eq!(request["method"], "initialize");
                assert_eq!(request["params"], json!({"api": API_VERSION}));
                peer.write_all(&rpc::encode(&rpc::result(request["id"].clone(), value)).unwrap())
                    .unwrap();
            });
            let reply = initialize(&mut client, json!(1), role.clone()).unwrap();
            assert_eq!(reply.role, role);
            assert!(reply.configuration.is_none());
            server.join().unwrap();
        }
        let reply = ServerStatusReply::decode(historical["server_status"].clone()).unwrap();
        assert!(reply.history.is_none());
        assert!(reply.manifest.is_none());
        assert!(reply.worktrees_directory.is_none());
        assert!(ServerStatusResult::decode(historical["server_status"].clone()).is_err());
        assert!(InitializeResult::decode(historical["host_initialize"].clone()).is_err());
        let mut bad = historical["host_initialize"].clone();
        bad["limits"] = json!([]);
        assert!(InitializeReply::decode(bad).is_err());
        let mut bad = historical["server_status"].clone();
        bad["history"] = json!([]);
        assert!(ServerStatusReply::decode(bad).is_err());
    }

    #[test]
    fn self_reporting_parameters_match_schema_and_existing_validators() {
        for case in fixtures()["self_parameter_cases"].as_array().unwrap() {
            let value = case["params"].clone();
            let valid = match case["method"].as_str().unwrap() {
                "sessions.status" => serde_json::from_value::<EmptyParams>(value).is_ok(),
                "sessions.set-state" => serde_json::from_value::<SetStateParams>(value).is_ok(),
                "sessions.set-variables" => serde_json::from_value::<SetVariablesParams>(value)
                    .is_ok_and(|p| p.validate().is_ok()),
                "sessions.set-worktree" => serde_json::from_value::<SetWorktreeParams>(value)
                    .is_ok_and(|p| {
                        p.name.as_ref().is_none_or(|n| {
                            (1..=100).contains(&n.len())
                                && n.as_bytes()[0].is_ascii_alphanumeric()
                                && n.bytes()
                                    .all(|b| b.is_ascii_alphanumeric() || b"._-".contains(&b))
                        })
                    }),
                _ => panic!("unknown fixture"),
            };
            assert_eq!(valid, case["valid"].as_bool().unwrap(), "{case}");
        }
        assert_eq!(AgentState::default(), AgentState::Unknown);
    }

    #[test]
    fn self_status_round_trips_extensions_and_preserves_absent_legacy_metadata() {
        let fixture = fixtures();
        let value = fixture["self_status"].clone();
        assert_eq!(json!(SelfStatus::decode(value.clone()).unwrap()), value);
        let mut extended = value.clone();
        extended["future"] = json!({"ignored":true});
        extended["raid"]["future"] = json!(true);
        extended["worktree"]["future"] = json!(true);
        assert_eq!(json!(SelfStatus::decode(extended).unwrap()), value);
        let legacy =
            SelfStatusReply::decode(json!({"agent_name":"goblin","attached":true})).unwrap();
        assert_eq!(legacy.agent_name.as_deref(), Some("goblin"));
        assert_eq!(legacy.attached, Some(true));
        assert!(legacy.allowed_run_targets.is_none());
        assert!(legacy.options.is_none());
        assert!(SelfStatusReply::decode(json!({"attached":"true"})).is_err());
        assert!(SelfStatusReply::decode(json!([])).is_err());
        for field in ["scope", "worktree", "created_by", "permission_bypass"] {
            let mut bad = value.clone();
            bad.as_object_mut().unwrap().remove(field);
            assert!(SelfStatus::decode(bad).is_err(), "{field}");
        }
        assert!(SetVariablesResult::decode(json!({"accepted":false,"recorded":true})).is_err());
        assert!(SetVariablesResult::decode(json!({"accepted":true})).is_err());
    }

    #[test]
    fn nested_result_records_require_objects_and_preserve_null_and_omission() {
        for case in fixtures()["self_result_cases"].as_array().unwrap() {
            let value = case["result"].clone();
            let valid = match case["schema"].as_str().unwrap() {
                "SelfStatus" => SelfStatus::decode(value).is_ok(),
                "SelfStatusReply" => SelfStatusReply::decode(value).is_ok(),
                "SetWorktreeReply" => SetWorktreeReply::decode(value).is_ok(),
                _ => panic!("unknown fixture"),
            };
            assert_eq!(valid, case["valid"].as_bool().unwrap(), "{case}");
        }
    }

    #[test]
    fn handwritten_result_fallbacks_enforce_schema_constraints() {
        let fixture = fixtures();
        let mut value = fixture["host_initialize"].clone();
        value.as_object_mut().unwrap().remove("configuration");
        assert!(InitializeResult::decode(value).is_err());
        for (field, bad) in [
            ("api", json!(2)),
            ("features", json!([1])),
            ("limits", json!([])),
            ("configuration", json!("host-only")),
        ] {
            let mut value = fixture["host_initialize"].clone();
            value[field] = bad;
            assert!(InitializeResult::decode(value).is_err());
        }
        for name in ["server_status", "server_status_unavailable"] {
            let mut value = fixture[name].clone();
            value["history"]["owner"] = json!(!value["history"]["owner"].as_bool().unwrap());
            assert!(ServerStatusResult::decode(value).is_err());
            let mut value = fixture[name].clone();
            value.as_object_mut().unwrap().remove("manifest");
            assert!(ServerStatusResult::decode(value).is_err());
        }
        assert!(ServerStopResult::decode(json!({"accepted":false})).is_err());
        assert!(ServerStopResult::decode(json!([true])).is_err());
        assert!(ServerStopResult::decode(json!({"accepted":"true","extra":1})).is_err());
    }
}
