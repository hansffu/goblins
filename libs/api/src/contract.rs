//! Schema-generated RPC contracts. Transport and authority remain handwritten.
use goblins_jsonrpc as rpc;
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use serde_json::Value;
use std::{io, os::unix::net::UnixStream};

/// Closed empty parameter objects need a struct, rather than a generated map.
#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct EmptyParams {}

/// Outbound field presence retained from the existing CLI callers.
pub struct ParamPresence {
    pub switch: Option<&'static str>,
    pub absent_null_fields: &'static [&'static str],
    pub present_empty_strings: &'static [&'static str],
}

pub trait Method {
    type Params: Serialize + DeserializeOwned;
    type Produced: Serialize + DeserializeOwned;
    type Result: Serialize + Response;
    const NAME: &'static str;
    const PARAM_ORDER: &'static [&'static str];
    const MIN_POSITIONAL: usize;
    const OMIT_EMPTY_OBJECTS: &'static [&'static str];
    const PRESENCE: ParamPresence;
}

/// Notifications have no result association and never become callable methods.
pub trait Notification {
    type Params: Serialize + DeserializeOwned;
    const NAME: &'static str;
    const DIRECTION: &'static str;
    const PARAM_ORDER: &'static [&'static str];
    const MIN_POSITIONAL: usize;
}

pub fn notification<N: Notification>(params: N::Params) -> serde_json::Result<Value> {
    Ok(
        serde_json::json!({"jsonrpc":"2.0", "method":N::NAME, "params":serde_json::to_value(params)?}),
    )
}

pub fn decode_notification<N: Notification>(call: rpc::Call) -> serde_json::Result<N::Params> {
    if call.id.is_some() || call.method != N::NAME {
        return Err(<serde_json::Error as serde::de::Error>::custom(
            "unexpected notification",
        ));
    }
    decode_ordered(call.params, N::PARAM_ORDER, N::MIN_POSITIONAL)
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

/// The original exec CLI read these fields independently. Keep that legacy
/// fallback before decoding the generated normalized DTO; current producers
/// still use the precise exit/startup-failure schemas.
impl Response for ExecReply {
    const SCHEMA: &'static str = "ExecReply";
    fn decode(value: Value) -> io::Result<Self> {
        let normalized = serde_json::json!({
            "code": value.get("code").and_then(Value::as_i64),
            "signal": value.get("signal").and_then(Value::as_i64),
            "message": value.get("message").and_then(Value::as_str),
        });
        serde_json::from_value(normalized).map_err(io::Error::other)
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

/// Decode requests using their original endpoint-specific positional field order.
/// Typify emits alphabetic Rust fields, so wire arrays must first become named data.
pub fn decode_params<M: Method>(value: Value) -> serde_json::Result<M::Params> {
    decode_ordered(value, M::PARAM_ORDER, M::MIN_POSITIONAL)
}

fn decode_ordered<P: DeserializeOwned>(
    value: Value,
    order: &[&str],
    minimum: usize,
) -> serde_json::Result<P> {
    let value = match value {
        Value::Array(values) => {
            if values.len() < minimum {
                return Err(<serde_json::Error as serde::de::Error>::custom(
                    "too few positional parameters",
                ));
            }
            if values.len() > order.len() {
                return Err(<serde_json::Error as serde::de::Error>::custom(
                    "too many positional parameters",
                ));
            }
            Value::Object(
                order
                    .iter()
                    .zip(values)
                    .map(|(name, value)| ((*name).to_string(), value))
                    .collect(),
            )
        }
        other => other,
    };
    serde_json::from_value(value)
}

/// Retain method-specific API-1 omission behavior that Typify cannot generate
/// for a replaced map type. Only empty objects marked in the contract are omitted.
pub fn params_value<M: Method>(params: M::Params) -> serde_json::Result<Value> {
    let mut value = serde_json::to_value(params)?;
    if let Some(object) = value.as_object_mut() {
        let switched = M::PRESENCE
            .switch
            .is_some_and(|key| object.get(key).is_some_and(|v| !v.is_null()));
        if switched {
            for name in M::PRESENCE.present_empty_strings {
                if object.get(*name).and_then(Value::as_str) == Some("") {
                    object.remove(*name);
                }
            }
        } else {
            for name in M::PRESENCE.absent_null_fields {
                object.entry((*name).to_string()).or_insert(Value::Null);
            }
        }
        for name in M::OMIT_EMPTY_OBJECTS {
            if object
                .get(*name)
                .is_some_and(|v| v.as_object().is_some_and(|m| m.is_empty()))
            {
                object.remove(*name);
            }
        }
    }
    Ok(value)
}

/// Typed dispatch for an existing transport callback, retaining its error type.
pub fn invoke<M: Method, E: From<io::Error>>(
    params: M::Params,
    transport: impl FnOnce(&str, Value) -> Result<Value, E>,
) -> Result<M::Result, E> {
    let params = params_value::<M>(params).map_err(io::Error::other)?;
    let reply = transport(M::NAME, params)?;
    Ok(M::Result::decode(reply)?)
}

/// Preserve framing/correlation/error checks, then decode the associated DTO.
pub fn exchange<M: Method>(
    stream: &mut UnixStream,
    id: Value,
    params: M::Params,
) -> io::Result<M::Result> {
    invoke::<M, io::Error>(params, |method, params| {
        rpc::exchange(stream, id, method, params)
    })
}

/// CLI JSON output retains the original wire fields after typed validation.
/// This preserves nulls, omitted legacy fields and unknown result extensions.
pub fn exchange_with_wire<M: Method>(
    stream: &mut UnixStream,
    id: Value,
    params: M::Params,
) -> io::Result<(M::Result, Value)> {
    let wire = rpc::exchange(stream, id, M::NAME, params_value::<M>(params)?)?;
    let result = M::Result::decode(wire.clone())?;
    Ok((result, wire))
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
    fn exec_parameters_preserve_legacy_order_defaults_and_decoding() {
        #[derive(Debug, Deserialize, Serialize)]
        #[serde(deny_unknown_fields)]
        struct LegacyExec {
            session: String,
            #[serde(default)]
            pin: Option<String>,
            #[serde(default)]
            argv: Option<Vec<String>>,
            #[serde(default)]
            cwd: Option<String>,
            #[serde(default)]
            env: Vec<(String, String)>,
            #[serde(default)]
            tty: bool,
            #[serde(default)]
            interactive: bool,
        }
        for case in fixtures()["exec_parameter_cases"].as_array().unwrap() {
            if case["method"] != host::SessionsExec::NAME {
                continue;
            }
            let old = serde_json::from_value::<LegacyExec>(case["params"].clone());
            let new = decode_params::<host::SessionsExec>(case["params"].clone());
            assert_eq!(old.is_ok(), new.is_ok(), "{case}");
            assert_eq!(new.is_ok(), case["decoded"].as_bool().unwrap(), "{case}");
            if let (Ok(old), Ok(new)) = (old, new) {
                assert_eq!(old.session, new.session);
                assert_eq!(old.pin, new.pin);
                assert_eq!(old.argv, new.argv);
                assert_eq!(old.cwd, new.cwd);
                assert_eq!(old.env, new.env);
                assert_eq!(old.tty, new.tty);
                assert_eq!(old.interactive, new.interactive);
                let wire = params_value::<host::SessionsExec>(new).unwrap();
                for key in ["session", "env", "tty", "interactive"] {
                    assert!(wire.get(key).is_some());
                }
                for (key, value) in [
                    ("argv", old.argv.is_none()),
                    ("cwd", old.cwd.is_none()),
                    ("pin", old.pin.is_none()),
                ] {
                    if value {
                        assert!(wire.get(key).is_none());
                    }
                }
            }
        }
    }

    #[test]
    fn exec_notification_identity_params_and_compatible_results() {
        assert!(!host::METHODS.contains(&host_notifications::ExecSignal::NAME));
        assert_eq!(
            host_notifications::ExecSignal::DIRECTION,
            "client-to-server"
        );
        for case in fixtures()["exec_signal_cases"].as_array().unwrap() {
            let call = rpc::Call {
                jsonrpc: "2.0".into(),
                id: None,
                method: host_notifications::ExecSignal::NAME.into(),
                params: case["params"].clone(),
            };
            assert_eq!(
                decode_notification::<host_notifications::ExecSignal>(call).is_ok(),
                case["decoded"].as_bool().unwrap(),
                "{case}"
            );
        }
        let value = notification::<host_notifications::ExecSignal>(ExecSignalParams { signal: 15 })
            .unwrap();
        assert_eq!(
            value,
            serde_json::json!({"jsonrpc":"2.0","method":"exec.signal","params":{"signal":15}})
        );
        for case in fixtures()["exec_result_cases"].as_array().unwrap() {
            let wire = case["result"].clone();
            let normalized = ExecReply::decode(wire.clone()).unwrap();
            assert_eq!(normalized.code, wire.get("code").and_then(Value::as_i64));
            assert_eq!(
                normalized.signal,
                wire.get("signal").and_then(Value::as_i64)
            );
            assert_eq!(
                normalized.message.as_deref(),
                wire.get("message").and_then(Value::as_str)
            );
            assert_eq!(
                ExecReply::decode(case["result"].clone()).is_ok(),
                case["compatible"].as_bool().unwrap(),
                "{case}"
            );
        }
        assert_eq!(
            serde_json::to_value(ExecExitResult {
                code: Some(7),
                signal: None
            })
            .unwrap(),
            serde_json::json!({"code":7,"signal":null})
        );
        assert_eq!(
            serde_json::to_value(ExecStartupFailureResult {
                code: 127,
                message: "missing".into()
            })
            .unwrap(),
            serde_json::json!({"code":127,"message":"missing"})
        );
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
                "sessions.set-worktree" => decode_params::<sandbox::SessionsSetWorktree>(value)
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
    fn session_read_contracts_preserve_requests_results_and_legacy_defaults() {
        let f = fixtures();
        for case in f["session_parameter_cases"].as_array().unwrap() {
            let valid = if case["method"] == "sessions.list" {
                serde_json::from_value::<EmptyParams>(case["params"].clone()).is_ok()
            } else {
                serde_json::from_value::<SessionSelectorParams>(case["params"].clone()).is_ok()
            };
            assert_eq!(valid, case["valid"].as_bool().unwrap(), "{case}");
        }
        for case in f["session_result_cases"].as_array().unwrap() {
            let valid = if case["schema"] == "HostSessionReply" {
                HostSessionReply::decode(case["result"].clone()).is_ok()
            } else {
                SandboxSessionReply::decode(case["result"].clone()).is_ok()
            };
            assert_eq!(valid, case["valid"].as_bool().unwrap(), "{case}");
        }
        let original =
            HostSessionReply::decode(f["original_api1"]["host_session"].clone()).unwrap();
        assert!(original.agent_name.is_none());
        let original = invoke::<host::SessionsGet, io::Error>(
            SessionSelectorParams {
                session: "session-id".into(),
            },
            |name, params| {
                assert_eq!(name, "sessions.get");
                assert_eq!(params, json!({"session":"session-id"}));
                Ok(f["original_api1"]["host_session"].clone())
            },
        )
        .unwrap();
        assert_eq!(original.terminal, "/host/terminal.sock");
        let legacy = HostSessionReply::decode(f["host_session_legacy"].clone()).unwrap();
        assert_eq!(legacy.docker_scope.as_deref(), Some("legacy-scope"));
        assert_eq!(legacy.agent_state, "unknown");
        assert_eq!(legacy.inbox, 0);
        assert!(!legacy.terminal_attached);
        assert!(legacy.worktree.is_none());
        assert_eq!(
            HostSessionListReply::decode(json!([f["host_session"], f["host_session_legacy"]]))
                .unwrap()
                .0
                .len(),
            2
        );
        assert!(HostSessionListReply::decode(json!({})).is_err());
        assert!(HostSessionListReply::decode(json!([[]])).is_err());
    }

    #[test]
    fn typed_exchange_keeps_cli_wire_fields_after_validation() {
        use std::io::Write;
        let wire = fixtures()["forward_compatibility"]["host_session"].clone();
        let expected = wire.clone();
        let (mut client, mut peer) = UnixStream::pair().unwrap();
        let server = std::thread::spawn(move || {
            let request = rpc::read(&mut peer).unwrap();
            assert_eq!(request["method"], "sessions.get");
            assert_eq!(request["params"], json!({"session":"goblin"}));
            peer.write_all(&rpc::encode(&rpc::result(request["id"].clone(), wire)).unwrap())
                .unwrap();
        });
        let (record, original) = exchange_with_wire::<host::SessionsGet>(
            &mut client,
            json!(1),
            SessionSelectorParams {
                session: "goblin".into(),
            },
        )
        .unwrap();
        assert_eq!(record.id, expected["id"]);
        assert_eq!(original, expected);
        server.join().unwrap();
    }

    // Reference decoders retain the original pre-migration field order and defaults.
    #[derive(Debug, PartialEq, Serialize, Deserialize)]
    #[serde(deny_unknown_fields)]
    struct LegacyHostLaunch {
        key: String,
        configuration: String,
        #[serde(default)]
        name: String,
        agent_name: Option<String>,
        #[serde(default)]
        cwd: Option<std::path::PathBuf>,
        #[serde(default)]
        scope: Option<String>,
        #[serde(default)]
        dev_shell: Option<String>,
        rows: u16,
        cols: u16,
        #[serde(default)]
        detached: bool,
        #[serde(default)]
        raid: Option<String>,
        #[serde(default)]
        recreate: Option<String>,
        #[serde(skip)]
        recorded: Option<()>,
        #[serde(skip)]
        worktree: Option<String>,
        #[serde(default)]
        options: std::collections::BTreeMap<String, String>,
    }
    #[derive(Debug, PartialEq, Serialize, Deserialize)]
    #[serde(deny_unknown_fields)]
    struct LegacyMemberLaunch {
        key: String,
        name: String,
        agent_name: Option<String>,
        #[serde(default)]
        detached: bool,
        #[serde(default)]
        worktree: Option<String>,
        #[serde(default)]
        options: std::collections::BTreeMap<String, String>,
    }
    #[derive(Debug, PartialEq, Serialize, Deserialize)]
    #[serde(deny_unknown_fields)]
    struct LegacyResize {
        session: String,
        rows: u16,
        cols: u16,
    }

    #[derive(Debug, PartialEq, Serialize, Deserialize)]
    #[serde(deny_unknown_fields)]
    struct LegacySetWorktree {
        name: Option<String>,
    }
    #[test]
    fn nullable_positional_fields_preserve_original_required_positions() {
        for value in [
            json!({}),
            json!([]),
            json!([null]),
            json!({"name":null}),
            json!(["branch"]),
        ] {
            let previous = serde_json::from_value::<LegacySetWorktree>(value.clone());
            let current = decode_params::<sandbox::SessionsSetWorktree>(value.clone());
            assert_eq!(previous.is_ok(), current.is_ok(), "{value}");
            if let (Ok(previous), Ok(current)) = (previous, current) {
                assert_eq!(previous.name, current.name);
            }
        }
    }

    fn lifecycle_request(endpoint: &str, method: &str, value: Value) -> serde_json::Result<Value> {
        fn decode<M: Method>(v: Value) -> serde_json::Result<Value> {
            serde_json::to_value(decode_params::<M>(v)?)
        }
        match (endpoint, method) {
            ("host", "sessions.start") => decode::<host::SessionsStart>(value),
            ("host", "sessions.stop") => decode::<host::SessionsStop>(value),
            ("host", "sessions.detach") => decode::<host::SessionsDetach>(value),
            ("host", "sessions.resize") => decode::<host::SessionsResize>(value),
            ("sandbox", "sessions.start") => decode::<sandbox::SessionsStart>(value),
            ("sandbox", "sessions.stop") => decode::<sandbox::SessionsStop>(value),
            ("sandbox", "sessions.detach") => decode::<sandbox::SessionsDetach>(value),
            ("sandbox", "targets.list") => decode::<sandbox::TargetsList>(value),
            _ => panic!("unknown lifecycle fixture"),
        }
    }

    #[test]
    fn lifecycle_parameters_preserve_original_positional_order_and_defaults() {
        for case in fixtures()["lifecycle_parameter_cases"].as_array().unwrap() {
            let endpoint = case["endpoint"].as_str().unwrap();
            let method = case["method"].as_str().unwrap();
            let value = case["params"].clone();
            let result = lifecycle_request(endpoint, method, value.clone());
            assert_eq!(result.is_ok(), case["decoded"].as_bool().unwrap(), "{case}");
            macro_rules! compare {
                ($legacy:ty) => {{
                    let previous = serde_json::from_value::<$legacy>(value);
                    assert_eq!(previous.is_ok(), result.is_ok(), "legacy refusal: {case}");
                    if let (Ok(previous), Ok(current)) = (previous, result) {
                        let current = serde_json::from_value::<$legacy>(current).unwrap();
                        assert_eq!(previous, current, "legacy field mapping: {case}");
                    }
                }};
            }
            match (endpoint, method) {
                ("host", "sessions.start") => compare!(LegacyHostLaunch),
                ("sandbox", "sessions.start") => compare!(LegacyMemberLaunch),
                ("host", "sessions.resize") => compare!(LegacyResize),
                _ => (),
            }
        }
    }

    #[test]
    fn host_launch_omits_only_empty_options_from_typed_wire_requests() {
        let value =
            json!({"key":"k","configuration":"/manifest","name":"shell","rows":24,"cols":80});
        let p = decode_params::<host::SessionsStart>(value.clone()).unwrap();
        assert!(
            params_value::<host::SessionsStart>(p)
                .unwrap()
                .get("options")
                .is_none()
        );
        let wire = params_value::<host::SessionsStart>(
            decode_params::<host::SessionsStart>(value.clone()).unwrap(),
        )
        .unwrap();
        assert_eq!(wire["agent_name"], Value::Null);
        assert!(wire.get("agent_name").is_some());
        assert!(wire.get("scope").is_some());
        assert!(wire.get("dev_shell").is_some());
        assert_eq!(wire["detached"], false);
        let mut resume = value.clone();
        resume["name"] = json!("");
        resume["recreate"] = json!("record-id");
        let wire = params_value::<host::SessionsStart>(
            decode_params::<host::SessionsStart>(resume.clone()).unwrap(),
        )
        .unwrap();
        for field in ["name", "agent_name", "scope", "dev_shell", "options"] {
            assert!(wire.get(field).is_none(), "{field}");
        }
        resume["name"] = json!("explicit-invalid");
        assert_eq!(
            params_value::<host::SessionsStart>(
                decode_params::<host::SessionsStart>(resume).unwrap()
            )
            .unwrap()["name"],
            "explicit-invalid"
        );
        let mut nonempty = value;
        nonempty["options"] = json!({"model":"fast"});
        let p = decode_params::<host::SessionsStart>(nonempty).unwrap();
        assert_eq!(
            params_value::<host::SessionsStart>(p).unwrap()["options"],
            json!({"model":"fast"})
        );
        let member =
            decode_params::<sandbox::SessionsStart>(json!({"key":"k","name":"shell"})).unwrap();
        assert_eq!(
            params_value::<sandbox::SessionsStart>(member).unwrap()["options"],
            json!({})
        );
    }

    #[test]
    fn lifecycle_results_accept_old_metadata_and_validate_nested_records() {
        let fixture = fixtures();
        for case in fixture["lifecycle_result_cases"].as_array().unwrap() {
            let value = case["result"].clone();
            let valid = match case["schema"].as_str().unwrap() {
                "HostLaunchReply" => HostLaunchReply::decode(value).is_ok(),
                "MemberLaunchReply" => MemberLaunchReply::decode(value).is_ok(),
                "RunTargetListReply" => RunTargetListReply::decode(value).is_ok(),
                _ => panic!("unknown lifecycle result fixture"),
            };
            assert_eq!(valid, case["valid"].as_bool().unwrap(), "{case}");
        }
        let accepted =
            HostLaunchReply::decode(fixture["original_api1"]["host_launch"].clone()).unwrap();
        assert!(accepted.agent_name.is_none());
        assert!(accepted.attach.is_none());
        assert_eq!(
            accepted.terminal.unwrap(),
            std::path::PathBuf::from("/host/terminal.sock")
        );
        assert_eq!(
            RunTargetListReply::decode(fixture["forward_compatibility"]["run_targets"].clone())
                .unwrap()
                .0
                .len(),
            1
        );
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
