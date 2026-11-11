//! Schema-generated RPC contracts. Transport and authority remain handwritten.
use goblins_jsonrpc as rpc;
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use serde_json::Value;
use std::{io, os::unix::net::UnixStream};

/// The existing positional inbox head; Typify replacements require a named path.
pub type InboxHead = (String, u64);

/// Named empty mailbox params retain the original exposed Empty diagnostics.
pub mod mailbox_status {
    #[derive(Clone, Debug, Default, serde::Serialize, serde::Deserialize)]
    #[serde(deny_unknown_fields)]
    pub struct Empty {}
}
pub use mailbox_status::Empty as MailboxStatusParams;

impl Response for CommunicationsPageReply {
    const SCHEMA: &'static str = "CommunicationsPageReply";
    fn decode(value: Value) -> io::Result<Self> {
        Ok(Self {
            events: value.get("events").and_then(Value::as_array).cloned(),
            cursor: value.get("cursor").and_then(Value::as_u64),
            session: value
                .get("session")
                .and_then(Value::as_str)
                .map(str::to_owned),
            latest: value.get("latest").and_then(Value::as_u64),
        })
    }
}

/// A generated kind/data binding. It describes data, never publication rights.
pub trait CommunicationKind {
    type Data: Serialize;
    const KIND: &'static str;
}
pub struct CommunicationPayload {
    kind: &'static str,
    data: Value,
}
impl CommunicationPayload {
    pub fn new<K: CommunicationKind>(data: K::Data) -> Self {
        Self {
            kind: K::KIND,
            data: serde_json::to_value(data).expect("generated communication data serializes"),
        }
    }
    pub fn into_parts(self) -> (&'static str, Value) {
        (self.kind, self.data)
    }
    pub fn into_data(self) -> Value {
        self.data
    }
}

/// Closed empty parameter objects need a struct, rather than a generated map.
#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct EmptyParams {}

/// A compatible client which ignores a successful result body.
#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub struct IgnoredResult {}
impl Response for IgnoredResult {
    const SCHEMA: &'static str = "IgnoredResult";
    fn decode(_: Value) -> io::Result<Self> {
        Ok(Self {})
    }
}

/// Outbound field presence retained from the existing CLI callers.
pub struct ParamPresence {
    pub switch: Option<&'static str>,
    pub absent_null_fields: &'static [&'static str],
    pub present_empty_strings: &'static [&'static str],
}

/// Retained positional forms inside an array of request records.
pub struct NestedPositional {
    pub field: &'static str,
    pub order: &'static [&'static str],
    pub minimum: usize,
    pub array_items: bool,
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
    const POSITIONAL: bool = true;
    const NULL_AS_EMPTY: bool = false;
    const PARAM_NORMALIZATION: Option<&'static str> = None;
    const NESTED_POSITIONAL: &'static [NestedPositional] = &[];
    const OMIT_FALSE: &'static [&'static str] = &[];
}

/// A generated outbound parameter profile bound to its canonical RPC method.
pub trait ParamsProfile {
    type Method: Method;
    type Params: Serialize;
}
pub fn profile_params<P: ParamsProfile>(params: P::Params) -> serde_json::Result<Value> {
    serde_json::to_value(params)
}

/// An explicitly declared client result reader, bound to its RPC method.
pub trait ResultProfile {
    type Method: Method;
    type Reply: Response;
    const PARAM_ORDER: &'static [&'static str];
    const MIN_POSITIONAL: usize;
}

pub fn decode_reply_profile<M: Method, P: ResultProfile<Method = M>>(
    value: Value,
) -> io::Result<P::Reply> {
    let value =
        ordered_value(value, P::PARAM_ORDER, P::MIN_POSITIONAL).map_err(io::Error::other)?;
    P::Reply::decode(value)
}

pub fn decode_reply<M: Method>(value: Value) -> io::Result<M::Result> {
    M::Result::decode(value)
}

/// Select generated public outcome records without losing host fields to an
/// overlapping untagged failed-grant branch.
#[derive(Clone, Debug, Serialize)]
#[serde(untagged)]
pub enum PermissionRequestResult {
    Grant(PermissionGrantResult),
    Pending(PermissionPendingResult),
    Host(PermissionHostOutcome),
}
impl<'de> Deserialize<'de> for PermissionRequestResult {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let value = Value::deserialize(deserializer)?;
        let host = ["name", "started_at", "exit_code", "signal", "code", "log"]
            .iter()
            .any(|key| value.get(*key).is_some());
        let pending = value.get("status").and_then(Value::as_str) == Some("pending")
            && value.get("message").is_none();
        let result = if host {
            validate_result_shape(&value, "PermissionHostOutcome").and_then(|()| {
                let outcome: PermissionHostOutcome =
                    serde_json::from_value(value).map_err(io::Error::other)?;
                // Typify's String replacement retains the existing producer;
                // strict result decoding must also enforce PermissionHostLog.
                let component = outcome
                    .log
                    .strip_prefix("/run/goblins/host-jobs/")
                    .and_then(|path| path.strip_suffix("/output.log"));
                if !component.is_some_and(|request| !request.is_empty() && !request.contains('/')) {
                    return Err(io::Error::other("invalid host outcome log path"));
                }
                Ok(Self::Host(outcome))
            })
        } else if pending {
            validate_result_shape(&value, "PermissionPendingResult").and_then(|()| {
                serde_json::from_value(value)
                    .map(Self::Pending)
                    .map_err(io::Error::other)
            })
        } else {
            validate_result_shape(&value, "PermissionGrantResult").and_then(|()| {
                serde_json::from_value(value)
                    .map(Self::Grant)
                    .map_err(io::Error::other)
            })
        };
        result.map_err(serde::de::Error::custom)
    }
}

/// Notifications have no result association and never become callable methods.
pub trait Notification {
    type Params: Serialize + DeserializeOwned;
    const NAME: &'static str;
    const DIRECTION: &'static str;
    const PARAM_ORDER: &'static [&'static str];
    const MIN_POSITIONAL: usize;
    const POSITIONAL: bool = true;
}

/// Completion exchanges are separate from callable methods and notifications.
pub trait Completion {
    type Produced: Serialize;
    type Result: Response;
    const NAME: &'static str;
    const ID: u64;
    const ERROR_CODE: i32;
}

pub fn completion_payload<C: Completion>(value: C::Produced) -> serde_json::Result<Value> {
    serde_json::to_value(value)
}

pub fn completion_error<C: Completion>(message: &str) -> Value {
    rpc::error(serde_json::json!(C::ID), C::ERROR_CODE, message)
}

pub fn decode_completion<C: Completion>(value: Value) -> io::Result<C::Result> {
    rpc::validate_response(&value, &serde_json::json!(C::ID))?;
    if let Some(error) = value.get("error") {
        return Err(io::Error::other(
            error["message"].as_str().unwrap_or("attachment rejected"),
        ));
    }
    C::Result::decode(value["result"].clone())
}

/// Preserve the old client's method-only waiting classification and its
/// nonempty-array test, without tightening legacy peer envelopes or elements.
pub fn attach_waiting(value: &Value) -> Option<AttachWaitingReply> {
    (value.get("method").and_then(Value::as_str) == Some(attach_notifications::AttachWaiting::NAME))
        .then(|| AttachWaitingReply {
            has_missing: value
                .get("params")
                .and_then(|p| p.get("missing"))
                .and_then(Value::as_array)
                .is_some_and(|m| !m.is_empty()),
        })
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
    if call.params.is_array() && !N::POSITIONAL {
        return Err(<serde_json::Error as serde::de::Error>::custom(
            "params must be an object",
        ));
    }
    decode_ordered(call.params, N::PARAM_ORDER, N::MIN_POSITIONAL)
}

mod generated {
    use super::{
        EmptyParams, HostJob, IgnoredResult, MailboxStatusParams, PermissionRequestResult,
    };
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

mod host_job;
pub use host_job::{HostJob, host_submitted};
#[cfg(test)]
mod communications;
#[cfg(test)]
mod mailbox;
mod raid;
pub use raid::{decode_session_raid, raid_display_reply};
mod permission;
mod preview;
pub use preview::preview_reply;

impl Response for PermissionRequestReply {
    const SCHEMA: &'static str = "PermissionRequestReply";
    fn decode(value: Value) -> io::Result<Self> {
        Ok(Self {
            request: value
                .get("request")
                .and_then(Value::as_str)
                .map(str::to_owned),
            status: value
                .get("status")
                .and_then(Value::as_str)
                .map(str::to_owned),
            message: value
                .get("message")
                .and_then(Value::as_str)
                .map(str::to_owned),
            code: value.get("code").and_then(Value::as_i64),
        })
    }
}
impl Response for PermissionGrantReply {
    const SCHEMA: &'static str = "PermissionGrantReply";
    fn decode(value: Value) -> io::Result<Self> {
        // Named missing message was accepted by the original Option decoder.
        let reply: Self = serde_json::from_value(value).map_err(io::Error::other)?;
        if !crate::identifier(&reply.request)
            || !matches!(reply.status.as_str(), "ready" | "denied" | "error")
        {
            return Err(io::Error::other("invalid terminal result"));
        }
        Ok(reply)
    }
}

impl Response for PermissionDecisionReply {
    const SCHEMA: &'static str = "PermissionDecisionReply";
    fn decode(value: Value) -> io::Result<Self> {
        Ok(Self {
            accepted: value.get("accepted").and_then(Value::as_bool),
            request: value
                .get("request")
                .and_then(Value::as_str)
                .map(str::to_owned),
            approved: value.get("approved").and_then(Value::as_bool),
        })
    }
}

impl Response for EditorOpenReply {
    const SCHEMA: &'static str = "EditorOpenReply";
    fn decode(value: Value) -> io::Result<Self> {
        serde_json::from_value(serde_json::json!({
            "code": value.get("code").and_then(Value::as_i64),
            "message": value.get("message").and_then(Value::as_str),
        }))
        .map_err(io::Error::other)
    }
}

impl Response for FlakeRunReply {
    const SCHEMA: &'static str = "FlakeRunReply";
    fn decode(value: Value) -> io::Result<Self> {
        Ok(Self {
            program: value
                .get("program")
                .and_then(Value::as_str)
                .map(str::to_owned),
        })
    }
}

impl Response for DevShellLockReply {
    const SCHEMA: &'static str = "DevShellLockReply";
    fn decode(value: Value) -> io::Result<Self> {
        serde_json::from_value(serde_json::json!({
            "path": value.get("path").and_then(Value::as_str),
            "lock": value.get("lock").unwrap_or(&Value::Null),
            "generation": value.get("generation").and_then(Value::as_u64).unwrap_or(0),
        }))
        .map_err(io::Error::other)
    }
}

impl Response for AttachResult {
    const SCHEMA: &'static str = "AttachResult";
    fn decode(value: Value) -> io::Result<Self> {
        if value.get("attached") != Some(&Value::Bool(true)) {
            return Err(io::Error::other("invalid terminal handshake"));
        }
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

impl Response for TerminalCreateReply {
    const SCHEMA: &'static str = "TerminalCreateReply";
    fn decode(value: Value) -> io::Result<Self> {
        Ok(Self {
            id: value.get("id").and_then(Value::as_str).map(str::to_owned),
        })
    }
}

impl Response for TerminalListReply {
    const SCHEMA: &'static str = "TerminalListReply";
    fn decode(value: Value) -> io::Result<Self> {
        Ok(Self {
            terminals: value
                .get("terminals")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .map(|t| {
                    let text = |name| t.get(name).and_then(Value::as_str).map(str::to_owned);
                    TerminalDisplay {
                        id: text("id"),
                        name: text("name"),
                        label: text("label"),
                        origin: text("origin"),
                        rows: t.get("rows").unwrap_or(&Value::Null).to_string(),
                        cols: t.get("cols").unwrap_or(&Value::Null).to_string(),
                        attached: t.get("attached").and_then(Value::as_bool),
                    }
                })
                .collect(),
        })
    }
}

impl Response for TerminalCloseReply {
    const SCHEMA: &'static str = "TerminalCloseReply";
    fn decode(value: Value) -> io::Result<Self> {
        Ok(Self {
            accepted: value.get("accepted").and_then(Value::as_bool),
        })
    }
}

impl Response for TerminalReadReply {
    const SCHEMA: &'static str = "TerminalReadReply";
    fn decode(value: Value) -> io::Result<Self> {
        Ok(Self {
            text: value.get("text").and_then(Value::as_str).map(str::to_owned),
        })
    }
}

impl Response for TerminalWaitReply {
    const SCHEMA: &'static str = "TerminalWaitReply";
    fn decode(value: Value) -> io::Result<Self> {
        let text = |name| value.get(name).and_then(Value::as_str).map(str::to_owned);
        Ok(Self {
            status: text("status"),
            line: text("line"),
            text: text("text"),
            code: value.get("code").and_then(Value::as_i64),
            signal: value.get("signal").and_then(Value::as_i64),
        })
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
    let mut value = if value.is_null() && M::NULL_AS_EMPTY {
        serde_json::json!({})
    } else {
        value
    };
    if value.is_array() && !M::POSITIONAL {
        return Err(<serde_json::Error as serde::de::Error>::custom(
            "params must be an object",
        ));
    }
    for nested in M::NESTED_POSITIONAL {
        let collection = match &mut value {
            Value::Object(object) => object.get_mut(nested.field),
            Value::Array(values) => M::PARAM_ORDER
                .iter()
                .position(|name| *name == nested.field)
                .and_then(|index| values.get_mut(index)),
            _ => None,
        };
        if let Some(collection) = collection {
            if nested.array_items {
                if let Value::Array(records) = collection {
                    for record in records {
                        normalize_nested_record(record, nested)?;
                    }
                }
            } else {
                normalize_nested_record(collection, nested)?;
            }
        }
    }
    decode_ordered(value, M::PARAM_ORDER, M::MIN_POSITIONAL)
}

fn normalize_nested_record(
    record: &mut Value,
    nested: &NestedPositional,
) -> serde_json::Result<()> {
    if let Value::Array(fields) = record {
        if fields.len() < nested.minimum || fields.len() > nested.order.len() {
            return Err(<serde_json::Error as serde::de::Error>::custom(
                "invalid nested positional parameter length",
            ));
        }
        *record = Value::Object(
            nested
                .order
                .iter()
                .zip(std::mem::take(fields))
                .map(|(name, value)| ((*name).to_owned(), value))
                .collect(),
        );
    }
    Ok(())
}

fn decode_ordered<P: DeserializeOwned>(
    value: Value,
    order: &[&str],
    minimum: usize,
) -> serde_json::Result<P> {
    serde_json::from_value(ordered_value(value, order, minimum)?)
}

fn ordered_value(value: Value, order: &[&str], minimum: usize) -> serde_json::Result<Value> {
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
    Ok(value)
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
        for name in M::OMIT_FALSE {
            if object.get(*name) == Some(&Value::Bool(false)) {
                object.remove(*name);
            }
        }
        if M::PARAM_NORMALIZATION == Some("permission-request-fields") {
            if object.get("kind").and_then(Value::as_str) == Some("package") {
                object
                    .entry("package")
                    .or_insert_with(|| Value::String(String::new()));
            } else if object.get("package").and_then(Value::as_str) == Some("") {
                object.remove("package");
            }
            if let Some(host) = object.get_mut("host").and_then(Value::as_object_mut) {
                host.entry("env").or_insert_with(|| serde_json::json!([]));
                if host.get("detach") == Some(&Value::Bool(false)) {
                    host.remove("detach");
                }
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
    #[test]
    fn permission_request_decoding_preserves_old_order_defaults_and_validation() {
        use serde::{Deserialize, Serialize};
        #[derive(Deserialize, Serialize)]
        #[serde(deny_unknown_fields)]
        struct OldHost {
            argv: Vec<String>,
            cwd: String,
            #[serde(default)]
            env: Vec<(String, String)>,
            #[serde(default, skip_serializing_if = "Option::is_none")]
            timeout: Option<u64>,
            #[serde(default, skip_serializing_if = "std::ops::Not::not")]
            detach: bool,
            #[serde(default, skip_serializing_if = "Option::is_none")]
            name: Option<String>,
        }
        #[derive(Deserialize, Serialize)]
        #[serde(deny_unknown_fields)]
        struct Old {
            kind: String,
            #[serde(default)]
            package: String,
            reason: String,
            #[serde(default, skip_serializing_if = "Option::is_none")]
            update: Option<Vec<String>>,
            #[serde(default, skip_serializing_if = "std::ops::Not::not")]
            lock: bool,
            #[serde(default, skip_serializing_if = "Option::is_none")]
            host: Option<OldHost>,
        }
        let fixtures: serde_json::Value =
            serde_json::from_str(include_str!("../schema/fixtures.json")).unwrap();
        for case in fixtures["permission_request_parameter_cases"]
            .as_array()
            .unwrap()
        {
            let old = serde_json::from_value::<Old>(case["params"].clone());
            let new =
                super::decode_params::<super::sandbox::PermissionsRequest>(case["params"].clone());
            assert_eq!(old.is_ok(), new.is_ok(), "{case}");
            let valid = new.as_ref().is_ok_and(|p| p.validate());
            assert_eq!(valid, case["valid"].as_bool().unwrap(), "{case}");
            if let (Ok(old), Ok(new)) = (old, new) {
                let mut expected = serde_json::to_value(old).unwrap();
                if expected["kind"] != "package" && expected["package"] == "" {
                    expected.as_object_mut().unwrap().remove("package");
                }
                assert_eq!(
                    super::params_value::<super::sandbox::PermissionsRequest>(new).unwrap(),
                    expected,
                    "{case}"
                );
            }
        }
    }

    #[test]
    fn permission_request_client_profiles_retain_distinct_legacy_readers() {
        use super::{
            Response, sandbox::PermissionsRequest, sandbox_reply_profiles::PermissionsRequestGrant,
        };
        #[derive(serde::Deserialize, serde::Serialize)]
        #[serde(deny_unknown_fields)]
        struct OldGrant {
            request: String,
            status: String,
            message: Option<String>,
        }
        let fixtures: serde_json::Value =
            serde_json::from_str(include_str!("../schema/fixtures.json")).unwrap();
        for case in fixtures["permission_request_result_cases"]
            .as_array()
            .unwrap()
        {
            let wire = case["result"].clone();
            let old = serde_json::from_value::<OldGrant>(wire.clone())
                .ok()
                .filter(|r| {
                    crate::identifier(&r.request)
                        && matches!(r.status.as_str(), "ready" | "denied" | "error")
                });
            let new = super::decode_reply_profile::<PermissionsRequest, PermissionsRequestGrant>(
                wire.clone(),
            );
            assert_eq!(old.is_some(), new.is_ok(), "{case}");
            assert_eq!(new.is_ok(), case["grant"].as_bool().unwrap(), "{case}");
            if let (Some(old), Ok(new)) = (old, new) {
                assert_eq!(
                    serde_json::to_value(old).unwrap(),
                    serde_json::to_value(new).unwrap()
                );
            }
            let host = super::PermissionRequestReply::decode(wire.clone()).unwrap();
            assert_eq!(
                host.request.as_deref(),
                wire.get("request").and_then(serde_json::Value::as_str)
            );
            assert_eq!(
                host.status.as_deref(),
                wire.get("status").and_then(serde_json::Value::as_str)
            );
            assert_eq!(
                host.message.as_deref(),
                wire.get("message").and_then(serde_json::Value::as_str)
            );
            assert_eq!(
                host.code,
                wire.get("code").and_then(serde_json::Value::as_i64)
            );
            let produced = serde_json::from_value::<super::PermissionRequestResult>(wire.clone());
            assert_eq!(produced.is_ok(), case["valid"].as_bool().unwrap(), "{case}");
            if case["valid"] == true {
                assert!(produced.is_ok(), "{case}");
                let emitted = serde_json::to_value(produced.unwrap()).unwrap();
                let mut expected = wire.clone();
                expected.as_object_mut().unwrap().retain(|key, _| {
                    matches!(
                        key.as_str(),
                        "request"
                            | "status"
                            | "message"
                            | "name"
                            | "started_at"
                            | "exit_code"
                            | "signal"
                            | "code"
                            | "log"
                    )
                });
                assert_eq!(emitted, expected, "{case}");
            }
        }
    }

    #[test]
    fn permission_decision_requests_preserve_original_decode_and_order() {
        #[derive(serde::Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Old {
            session: String,
            request: String,
            approval: String,
            approved: bool,
        }
        let fixtures: serde_json::Value =
            serde_json::from_str(include_str!("../schema/fixtures.json")).unwrap();
        for case in fixtures["permission_decision_parameter_cases"]
            .as_array()
            .unwrap()
        {
            let old = serde_json::from_value::<Old>(case["params"].clone());
            let new =
                super::decode_params::<super::host::PermissionsDecide>(case["params"].clone());
            assert_eq!(old.is_ok(), new.is_ok(), "{case}");
            assert_eq!(new.is_ok(), case["valid"].as_bool().unwrap(), "{case}");
            if let (Ok(old), Ok(new)) = (old, new) {
                assert_eq!(old.session, new.session);
                assert_eq!(old.request, new.request);
                assert_eq!(old.approval, new.approval);
                assert_eq!(old.approved, new.approved);
            }
        }
    }

    #[test]
    fn devshell_app_requests_match_original_decoders_and_validators() {
        #[derive(serde::Deserialize)]
        #[serde(deny_unknown_fields)]
        struct OldRun {
            app: String,
        }
        #[derive(serde::Deserialize)]
        #[serde(deny_unknown_fields)]
        struct OldEmpty {}
        let fixtures: serde_json::Value =
            serde_json::from_str(include_str!("../schema/fixtures.json")).unwrap();
        for case in fixtures["devshell_app_parameter_cases"].as_array().unwrap() {
            let value = case["params"].clone();
            if case["method"] == "flake.run" {
                let old = serde_json::from_value::<OldRun>(value.clone());
                let new = super::decode_params::<super::sandbox::FlakeRun>(value);
                assert_eq!(old.is_ok(), new.is_ok(), "{case}");
                if let (Ok(old), Ok(new)) = (old, new) {
                    assert_eq!(old.app, new.app);
                    let valid = (1..=256).contains(&old.app.len())
                        && old
                            .app
                            .bytes()
                            .all(|b| b.is_ascii_alphanumeric() || b"_-+'".contains(&b));
                    assert_eq!(new.validate(), valid, "{case}");
                    assert_eq!(valid, case["valid"].as_bool().unwrap());
                }
            } else {
                assert_eq!(
                    serde_json::from_value::<OldEmpty>(value.clone()).is_ok(),
                    super::decode_params::<super::sandbox::DevShellLock>(value).is_ok(),
                    "{case}"
                );
            }
        }
    }

    #[test]
    fn devshell_app_replies_retain_independent_legacy_fallbacks() {
        use super::{DevShellLockReply, FlakeRunReply, Response};
        use serde_json::{Value, json};
        for value in [
            Value::Null,
            json!([]),
            json!({}),
            json!({"path":1,"program":true,"generation":-1}),
            json!({"path":"/p/flake.lock","lock":["opaque"],"generation":u64::MAX,"program":"/nix/store/p/bin/app"}),
            json!({"generation":1.0}),
        ] {
            let lock = DevShellLockReply::decode(value.clone()).unwrap();
            assert_eq!(
                lock.path.as_deref(),
                value.get("path").and_then(Value::as_str)
            );
            assert_eq!(lock.lock, value.get("lock").unwrap_or(&Value::Null).clone());
            assert_eq!(
                lock.generation,
                value.get("generation").and_then(Value::as_u64).unwrap_or(0)
            );
            let run = FlakeRunReply::decode(value.clone()).unwrap();
            assert_eq!(
                run.program.as_deref(),
                value.get("program").and_then(Value::as_str)
            );
        }
    }

    use super::*;
    use serde_json::json;

    fn fixtures() -> Value {
        serde_json::from_str(include_str!("../schema/fixtures.json")).unwrap()
    }

    #[test]
    fn attachment_hello_preserves_legacy_notification_decoding_and_order() {
        #[derive(Deserialize, Serialize)]
        #[serde(deny_unknown_fields)]
        struct LegacyHello {
            version: u64,
            rows: u16,
            cols: u16,
            #[serde(default)]
            terminal: Option<String>,
        }
        for case in fixtures()["attach_hello_parameter_cases"]
            .as_array()
            .unwrap()
        {
            let old = serde_json::from_value::<LegacyHello>(case["params"].clone());
            let call = rpc::Call {
                jsonrpc: "2.0".into(),
                id: None,
                method: attach_notifications::AttachHello::NAME.into(),
                params: case["params"].clone(),
            };
            let new = decode_notification::<attach_notifications::AttachHello>(call);
            assert_eq!(old.is_ok(), new.is_ok(), "{case}");
            assert_eq!(new.is_ok(), case["decoded"].as_bool().unwrap(), "{case}");
            if let (Ok(old), Ok(new)) = (old, new) {
                assert_eq!(
                    (old.version, old.rows, old.cols, old.terminal),
                    (new.version, new.rows, new.cols, new.terminal)
                );
            }
        }
        assert!(attach::METHODS.is_empty());
        assert_eq!(attach_exchanges::Reply::ID, 0);
        assert_eq!(attach_exchanges::LegacyAttach::ID, 0);
        let value = notification::<attach_notifications::AttachHello>(AttachHelloParams {
            version: ATTACH_VERSION,
            rows: 24,
            cols: 100,
            terminal: None,
        })
        .unwrap();
        assert_eq!(
            value,
            serde_json::json!({"jsonrpc":"2.0","method":"attach.hello","params":{"version":1,"rows":24,"cols":100}})
        );
    }

    #[test]
    fn attachment_waiting_and_completions_keep_old_client_acceptance() {
        for case in fixtures()["attach_waiting_cases"].as_array().unwrap() {
            let actual = attach_waiting(&case["frame"]).map(|p| p.has_missing);
            assert_eq!(actual, case["has_missing"].as_bool(), "{case}");
        }
        fn old(value: &Value) -> io::Result<()> {
            rpc::validate_response(value, &serde_json::json!(0))?;
            if let Some(error) = value.get("error") {
                return Err(io::Error::other(
                    error["message"].as_str().unwrap_or("attachment rejected"),
                ));
            }
            if value["result"]["attached"] != true {
                return Err(io::Error::other("invalid terminal handshake"));
            }
            Ok(())
        }
        for case in fixtures()["attach_completion_cases"].as_array().unwrap() {
            let wire = &case["reply"];
            let expected = old(wire);
            for actual in [
                decode_completion::<attach_exchanges::Reply>(wire.clone()),
                decode_completion::<attach_exchanges::LegacyAttach>(wire.clone()),
            ] {
                assert_eq!(actual.is_ok(), expected.is_ok(), "{case}");
                if let (Err(actual), Err(expected)) = (actual, &expected) {
                    assert_eq!(actual.to_string(), expected.to_string(), "{case}");
                }
            }
        }
        assert_eq!(
            completion_payload::<attach_exchanges::Reply>(AttachResult { attached: true }).unwrap(),
            serde_json::json!({"attached":true})
        );
    }

    #[test]
    fn terminal_io_generated_params_keep_legacy_order_and_defaults() {
        #[derive(Deserialize, Serialize)]
        #[serde(deny_unknown_fields)]
        struct Read {
            session: String,
            terminal: String,
            #[serde(default)]
            source: Option<String>,
            #[serde(default)]
            lines: Option<usize>,
        }
        #[derive(Deserialize, Serialize)]
        #[serde(deny_unknown_fields)]
        struct Send {
            session: String,
            terminal: String,
            #[serde(default)]
            text: Option<String>,
            #[serde(default)]
            keys: Vec<String>,
        }
        #[derive(Deserialize, Serialize)]
        #[serde(deny_unknown_fields)]
        struct Wait {
            session: String,
            terminal: String,
            #[serde(default, rename = "match")]
            text: Option<String>,
            #[serde(default)]
            regex: Option<String>,
            #[serde(default)]
            exit: Option<bool>,
            #[serde(default)]
            timeout: Option<u64>,
            #[serde(default)]
            lines: Option<usize>,
        }
        #[derive(Deserialize, Serialize)]
        #[serde(deny_unknown_fields)]
        struct Target {
            session: String,
            terminal: String,
        }
        #[derive(Deserialize, Serialize)]
        #[serde(deny_unknown_fields)]
        struct Resize {
            session: String,
            terminal: String,
            rows: u16,
            cols: u16,
        }
        fn compare<T: DeserializeOwned + Serialize, H: Method, S: Method>(case: &Value) {
            let scoped = |value: Value| -> Option<Value> {
                let mut map = match value {
                    Value::Null => serde_json::Map::new(),
                    Value::Object(map) if !map.contains_key("session") => map,
                    _ => return None,
                };
                map.insert("session".into(), serde_json::json!("a"));
                Some(Value::Object(map))
            };
            let value = case["params"].clone();
            let member = case["endpoint"] == "sandbox";
            let old = if member {
                scoped(value.clone()).and_then(|v| serde_json::from_value::<T>(v).ok())
            } else {
                serde_json::from_value::<T>(value.clone()).ok()
            };
            let new = if member {
                decode_params::<S>(value)
                    .ok()
                    .and_then(|p| params_value::<S>(p).ok())
                    .and_then(scoped)
            } else {
                decode_params::<H>(value)
                    .ok()
                    .and_then(|p| params_value::<H>(p).ok())
            };
            assert_eq!(new.is_some(), old.is_some(), "{case}");
            assert_eq!(new.is_some(), case["decoded"].as_bool().unwrap(), "{case}");
            if let (Some(old), Some(new)) = (old, new) {
                if case["method"] == "terminals.send" {
                    assert!(new["keys"].is_array());
                }
                assert_eq!(
                    serde_json::to_value(old).unwrap(),
                    serde_json::to_value(serde_json::from_value::<T>(new).unwrap()).unwrap(),
                    "{case}"
                );
            }
        }
        for case in fixtures()["terminal_io_parameter_cases"]
            .as_array()
            .unwrap()
        {
            match case["method"].as_str().unwrap() {
                "terminals.read" => {
                    compare::<Read, host::TerminalsRead, sandbox::TerminalsRead>(case)
                }
                "terminals.send" => {
                    compare::<Send, host::TerminalsSend, sandbox::TerminalsSend>(case)
                }
                "terminals.wait" => {
                    compare::<Wait, host::TerminalsWait, sandbox::TerminalsWait>(case)
                }
                "terminals.detach" => {
                    compare::<Target, host::TerminalsDetach, host::TerminalsDetach>(case)
                }
                "terminals.resize" => {
                    compare::<Resize, host::TerminalsResize, host::TerminalsResize>(case)
                }
                _ => panic!("unexpected corpus method"),
            }
        }
    }

    #[test]
    fn terminal_read_wait_replies_keep_independent_legacy_fields() {
        assert!(
            TerminalReadReply::decode(serde_json::json!({"text":7}))
                .unwrap()
                .text
                .is_none()
        );
        assert!(
            TerminalReadReply::decode(Value::Null)
                .unwrap()
                .text
                .is_none()
        );
        for case in fixtures()["terminal_io_result_cases"].as_array().unwrap() {
            if case["method"] != "terminals.wait" {
                continue;
            }
            let value = &case["result"];
            let reply = TerminalWaitReply::decode(value.clone()).unwrap();
            assert_eq!(
                reply.status.as_deref(),
                value.get("status").and_then(Value::as_str)
            );
            assert_eq!(
                reply.line.as_deref(),
                value.get("line").and_then(Value::as_str)
            );
            assert_eq!(
                reply.text.as_deref(),
                value.get("text").and_then(Value::as_str)
            );
            assert_eq!(reply.code, value.get("code").and_then(Value::as_i64));
            assert_eq!(reply.signal, value.get("signal").and_then(Value::as_i64));
        }
    }

    #[test]
    fn terminal_create_params_preserve_original_order_defaults_and_endpoint_forms() {
        #[derive(Deserialize, Serialize)]
        #[serde(deny_unknown_fields)]
        struct LegacyCreate {
            session: String,
            #[serde(default)]
            argv: Option<Vec<String>>,
            #[serde(default)]
            cwd: Option<String>,
            #[serde(default)]
            env: Vec<(String, String)>,
            #[serde(default)]
            name: Option<String>,
            #[serde(default)]
            rows: Option<u16>,
            #[serde(default)]
            cols: Option<u16>,
            #[serde(default)]
            notify: bool,
        }
        let scoped = |value: Value| -> Option<Value> {
            let mut map = match value {
                Value::Null => serde_json::Map::new(),
                Value::Object(m) if !m.contains_key("session") => m,
                _ => return None,
            };
            map.insert("session".into(), Value::String("a".into()));
            Some(Value::Object(map))
        };
        for case in fixtures()["terminal_parameter_cases"].as_array().unwrap() {
            if case["method"] != "terminals.create" {
                continue;
            }
            let member = case["endpoint"] == "sandbox";
            let value = case["params"].clone();
            let old = if member {
                scoped(value.clone()).and_then(|v| serde_json::from_value::<LegacyCreate>(v).ok())
            } else {
                serde_json::from_value::<LegacyCreate>(value.clone()).ok()
            };
            let new = if member {
                decode_params::<sandbox::TerminalsCreate>(value)
                    .ok()
                    .and_then(|p| params_value::<sandbox::TerminalsCreate>(p).ok())
                    .and_then(scoped)
            } else {
                decode_params::<host::TerminalsCreate>(value)
                    .ok()
                    .and_then(|p| params_value::<host::TerminalsCreate>(p).ok())
            };
            assert_eq!(old.is_some(), new.is_some(), "{case}");
            assert_eq!(new.is_some(), case["decoded"].as_bool().unwrap(), "{case}");
            if let (Some(old), Some(new)) = (old, new) {
                if !old.notify {
                    assert!(new.get("notify").is_none());
                }
                assert!(new["env"].is_array());
                let new: LegacyCreate = serde_json::from_value(new).unwrap();
                assert_eq!(
                    serde_json::to_value(old).unwrap(),
                    serde_json::to_value(new).unwrap(),
                    "{case}"
                );
            }
        }
        assert!(decode_params::<sandbox::TerminalsList>(Value::Null).is_ok());
        assert!(decode_params::<sandbox::TerminalsList>(serde_json::json!([])).is_err());
        assert!(decode_params::<host::TerminalsList>(serde_json::json!(["a"])).is_ok());
        assert!(
            decode_params::<sandbox::TerminalsClose>(serde_json::json!({"terminal":"app"})).is_ok()
        );
        assert!(decode_params::<sandbox::TerminalsClose>(serde_json::json!(["app"])).is_err());
        assert!(decode_params::<host::TerminalsClose>(serde_json::json!(["a", "app"])).is_ok());
    }

    #[test]
    fn terminal_normalized_readers_preserve_independent_legacy_fallbacks() {
        assert!(
            TerminalCreateReply::decode(serde_json::json!({"id":7}))
                .unwrap()
                .id
                .is_none()
        );
        assert!(
            TerminalCreateReply::decode(Value::Null)
                .unwrap()
                .id
                .is_none()
        );
        let list=TerminalListReply::decode(serde_json::json!({"terminals":[
            {"id":"t1","name":null,"origin":"remote","rows":"wide","cols":null,"attached":false},null,{"attached":"false"}
        ]})).unwrap();
        assert_eq!(list.terminals.len(), 3);
        let first = &list.terminals[0];
        assert_eq!(first.id.as_deref(), Some("t1"));
        assert_eq!(first.origin.as_deref(), Some("remote"));
        assert_eq!((&*first.rows, &*first.cols), ("\"wide\"", "null"));
        assert_eq!(first.attached, Some(false));
        assert!(list.terminals[1].id.is_none() && list.terminals[2].attached.is_none());
        assert!(
            TerminalListReply::decode(Value::Null)
                .unwrap()
                .terminals
                .is_empty()
        );
        assert!(TerminalCloseReply::decode(serde_json::json!([])).is_ok());
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
