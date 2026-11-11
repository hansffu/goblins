use serde_json::Value;
use std::{env, fs, path::PathBuf};

fn main() {
    println!("cargo:rerun-if-changed=schema");
    let common: Value =
        serde_json::from_str(&fs::read_to_string("schema/common.json").unwrap()).unwrap();
    let mut settings = typify::TypeSpaceSettings::default();
    settings.with_map_type("::std::collections::BTreeMap");
    settings.with_replacement(
        "Variables",
        "std::collections::BTreeMap<String, String>",
        [].into_iter(),
    );
    settings.with_replacement("WorktreeName", "std::string::String", [].into_iter());
    settings.with_replacement(
        "EmptyParams",
        "crate::contract::EmptyParams",
        [].into_iter(),
    );
    settings.with_replacement("HostPath", "std::path::PathBuf", [].into_iter());
    settings.with_replacement(
        "HistoryStatus",
        "crate::contract::HistoryStatus",
        [].into_iter(),
    );
    for name in [
        "OperationKey",
        "LaunchConfigurationName",
        "LaunchAgentName",
        "LaunchScope",
        "LaunchManifest",
    ] {
        settings.with_replacement(name, "std::string::String", [].into_iter());
    }
    settings.with_replacement("LaunchDimension", "u16", [].into_iter());
    settings.with_replacement("ExecArguments", "std::vec::Vec<String>", [].into_iter());
    settings.with_replacement("ExecDirectory", "std::string::String", [].into_iter());
    settings.with_replacement("ExecText", "std::string::String", [].into_iter());
    settings.with_replacement(
        "ExecEnvironment",
        "std::vec::Vec<(String, String)>",
        [].into_iter(),
    );
    settings.with_replacement("ExecSignalNumber", "i32", [].into_iter());
    settings.with_replacement("TerminalName", "std::string::String", [].into_iter());
    settings.with_replacement("TerminalDimension", "u16", [].into_iter());
    for name in [
        "TerminalReadSource",
        "TerminalKey",
        "TerminalRegex",
        "TerminalSendText",
    ] {
        settings.with_replacement(name, "std::string::String", [].into_iter());
    }
    settings.with_replacement("TerminalLines", "usize", [].into_iter());
    settings.with_replacement("TerminalTimeout", "u64", [].into_iter());
    settings.with_replacement("TerminalKeys", "std::vec::Vec<String>", [].into_iter());
    settings.with_replacement("DevShellGeneration", "u32", [].into_iter());
    settings.with_replacement("FlakeAppName", "std::string::String", [].into_iter());
    settings.with_replacement("AttachVersion", "u64", [].into_iter());
    settings.with_replacement("AttachDimension", "u16", [].into_iter());
    for name in ["LaunchOptions", "HostLaunchOptions"] {
        settings.with_replacement(
            name,
            "std::collections::BTreeMap<String, String>",
            [].into_iter(),
        );
    }
    settings.with_replacement("DevShellInputName", "std::string::String", [].into_iter());
    settings.with_replacement("DevShellInputs", "std::vec::Vec<String>", [].into_iter());
    settings.with_replacement("PreviewCount", "usize", [].into_iter());
    for name in ["DevShellLocked", "DevShellIncoming"] {
        settings.with_patch(
            name,
            typify::TypeSpacePatch::default().with_derive("PartialEq"),
        );
    }
    settings.with_replacement("EditorPath", "std::string::String", [].into_iter());
    settings.with_replacement("EditorPosition", "u32", [].into_iter());
    settings.with_replacement("EditorExitCode", "i32", [].into_iter());
    settings.with_replacement("EditorDiagnostic", "std::string::String", [].into_iter());
    settings.with_replacement("EditorFiles", "std::vec::Vec<EditorFile>", [].into_iter());
    for name in ["EditorFile", "EditorOpenParams"] {
        settings.with_patch(
            name,
            typify::TypeSpacePatch::default().with_derive("PartialEq"),
        );
    }
    for name in [
        "PermissionKind",
        "PermissionPackage",
        "PermissionPackageAttribute",
        "HostCommandText",
        "HostCommandDirectory",
        "HostCommandName",
        "HostCommandEnvName",
        "PermissionGrantState",
        "LegacyGrantState",
        "PermissionHostLog",
        "PermissionReason",
    ] {
        settings.with_replacement(name, "std::string::String", [].into_iter());
    }
    settings.with_replacement(
        "HostCommandArguments",
        "std::vec::Vec<String>",
        [].into_iter(),
    );
    settings.with_replacement(
        "HostCommandEnvironment",
        "std::vec::Vec<(String, String)>",
        [].into_iter(),
    );
    settings.with_replacement("HostCommandTimeout", "u64", [].into_iter());
    settings.with_replacement("PermissionHostCode", "i32", [].into_iter());
    settings.with_replacement(
        "PermissionRequestResult",
        "crate::contract::PermissionRequestResult",
        [].into_iter(),
    );
    settings.with_patch(
        "HostCommandParams",
        typify::TypeSpacePatch::default().with_derive("PartialEq"),
    );
    settings.with_replacement(
        "PermissionRecordKind",
        "std::string::String",
        [].into_iter(),
    );
    settings.with_replacement(
        "PermissionPreviewValue",
        "serde_json::Value",
        [].into_iter(),
    );
    settings.with_replacement("HostJob", "crate::contract::HostJob", [].into_iter());
    settings.with_replacement(
        "IgnoredResult",
        "crate::contract::IgnoredResult",
        [].into_iter(),
    );
    settings.with_replacement("RaidNameRule", "std::string::String", [].into_iter());
    settings.with_replacement(
        "RaidLegacyOptions",
        "std::collections::BTreeMap<String,String>",
        [].into_iter(),
    );
    let mut types = typify::TypeSpace::new(&settings);
    // Typify 0.7 cannot process if/then/else. Keep the canonical producer
    // schema precise; Edit::outcome enforces this named dependency while
    // the generated DTO describes its common fields (see exceptions.json).
    let mut generation_schema = common.clone();
    let editor_result = generation_schema["definitions"]["EditorOpenResult"]
        .as_object_mut()
        .unwrap();
    for keyword in ["if", "then", "else"] {
        assert!(editor_result.remove(keyword).is_some());
    }
    // Public record kind/preview dependencies follow the existing producer
    // branches. Keep the canonical schema; Typify cannot compile conditionals.
    assert!(
        generation_schema["definitions"]["PermissionRecord"]
            .as_object_mut()
            .unwrap()
            .remove("allOf")
            .is_some()
    );
    types
        .add_root_schema(serde_json::from_value(generation_schema).unwrap())
        .unwrap();
    let mut output = types.to_stream().to_string();
    output.push_str(&format!(
        "\npub const API_VERSION: u32 = {};\n",
        common["x-api-version"]
    ));
    output.push_str(&format!(
        "pub const ATTACH_VERSION: u64 = {};\n",
        common["x-attach-version"]
    ));
    let mut decoded = std::collections::BTreeSet::from([
        "InitializeReply".to_string(),
        "ServerStatusReply".to_string(),
        "ServerStopResult".to_string(),
        "SetVariablesResult".to_string(),
        "ExecReply".to_string(),
        "TerminalCreateReply".to_string(),
        "TerminalListReply".to_string(),
        "TerminalCloseReply".to_string(),
        "TerminalReadReply".to_string(),
        "TerminalWaitReply".to_string(),
        "AttachResult".to_string(),
        "FlakeRunReply".to_string(),
        "DevShellLockReply".to_string(),
        "DevShellPreviewReply".to_string(),
        "EditorOpenReply".to_string(),
        "PermissionDecisionReply".to_string(),
        "PermissionRequestReply".to_string(),
        "PermissionRecordReply".to_string(),
        "PermissionListReply".to_string(),
        "IgnoredResult".to_string(),
        "RaidDisplayReply".to_string(),
    ]);
    for endpoint in ["host", "sandbox", "attach"] {
        let document: Value = serde_json::from_str(
            &fs::read_to_string(format!("schema/{endpoint}.openrpc.json")).unwrap(),
        )
        .unwrap();
        output.push_str(&format!(
            "pub mod {endpoint} {{ #[allow(unused_imports)] use super::*;\n"
        ));
        let mut names = Vec::new();
        for method in document["methods"].as_array().unwrap() {
            let name = method["name"].as_str().unwrap();
            let rust = method["x-rust-name"].as_str().unwrap();
            let definition = |value: &Value| {
                value["$ref"]
                    .as_str()
                    .unwrap()
                    .strip_prefix("common.json#/definitions/")
                    .unwrap()
                    .to_owned()
            };
            let params = definition(&method["x-params-schema"]);
            let result = definition(&method["result"]["schema"]);
            let reply = method
                .get("x-client-result-schema")
                .map(definition)
                .unwrap_or_else(|| result.clone());
            assert!(common["definitions"].get(&params).is_some());
            assert!(common["definitions"].get(&result).is_some());
            assert!(common["definitions"].get(&reply).is_some());
            if decoded.insert(reply.clone()) {
                output.push_str(&format!(
                    "impl crate::contract::Response for {reply} {{ const SCHEMA: &'static str = {reply:?}; }}\n"
                ));
            }
            let positional = &method["x-positional-schema"];
            let positional = if let Some(reference) = positional.get("$ref").and_then(Value::as_str)
            {
                common
                    .pointer(reference.strip_prefix("common.json#").unwrap())
                    .unwrap()
            } else {
                positional
            };
            let positional_allowed = method["paramStructure"] != "by-name";
            let minimum = positional["minItems"].as_u64().unwrap_or(0);
            let null_as_empty = method["x-null-params-as-empty"].as_bool().unwrap_or(false);
            let omit_false: Vec<_> = method
                .get("x-client-omit-default-false")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .map(|v| v.as_str().unwrap())
                .collect();
            let omit_empty: Vec<_> = method
                .get("x-client-omit-empty-objects")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .map(|v| v.as_str().unwrap())
                .collect();
            let presence = &method["x-client-param-presence"];
            let switch = presence.get("switch").and_then(Value::as_str);
            let nullable: Vec<_> = presence
                .get("absent-null-fields")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .map(|v| v.as_str().unwrap())
                .collect();
            let empty_strings: Vec<_> = presence
                .get("present-empty-strings")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .map(|v| v.as_str().unwrap())
                .collect();
            let order: Vec<_> = method["params"]
                .as_array()
                .unwrap()
                .iter()
                .map(|p| p["name"].as_str().unwrap())
                .collect();
            let nested: Vec<_> = method["x-nested-positional-params"].as_array().into_iter().flatten().map(|row| {
                let field = row["field"].as_str().unwrap();
                let order: Vec<_> = row["order"].as_array().unwrap().iter().map(|v| v.as_str().unwrap()).collect();
                let minimum = row["minimum"].as_u64().unwrap();
                let array_items = row.get("array-items").and_then(Value::as_bool).unwrap_or(true);
                format!("crate::contract::NestedPositional {{ field: {field:?}, order: &{order:?}, minimum: {minimum}, array_items: {array_items} }}")
            }).collect();
            let nested = nested.join(",");
            let param_normalization = method
                .get("x-client-param-normalization")
                .and_then(Value::as_str);
            output.push_str(&format!("pub struct {rust}; impl crate::contract::Method for {rust} {{ type Params = {params}; type Produced = {result}; type Result = {reply}; const NAME: &'static str = {name:?}; const PARAM_ORDER: &'static [&'static str] = &{order:?}; const MIN_POSITIONAL: usize = {minimum}; const POSITIONAL: bool = {positional_allowed}; const NULL_AS_EMPTY: bool = {null_as_empty}; const PARAM_NORMALIZATION: Option<&'static str> = {param_normalization:?}; const NESTED_POSITIONAL: &'static [crate::contract::NestedPositional] = &[{nested}]; const OMIT_FALSE: &'static [&'static str] = &{omit_false:?}; const OMIT_EMPTY_OBJECTS: &'static [&'static str] = &{omit_empty:?}; const PRESENCE: crate::contract::ParamPresence = crate::contract::ParamPresence {{ switch:{switch:?}, absent_null_fields:&{nullable:?}, present_empty_strings:&{empty_strings:?} }}; }}\n"));
            names.push(name);
        }
        output.push_str(&format!("pub const METHODS: &[&str] = &{names:?}; }}\n"));
        output.push_str(&format!(
            "pub mod {endpoint}_reply_profiles {{ #[allow(unused_imports)] use super::*;\n"
        ));
        for method in document["methods"].as_array().unwrap() {
            for profile in method["x-client-result-profiles"]
                .as_array()
                .into_iter()
                .flatten()
            {
                let rust = profile["x-rust-name"].as_str().unwrap();
                let method_rust = method["x-rust-name"].as_str().unwrap();
                let reply = profile["x-client-result-schema"]["$ref"]
                    .as_str()
                    .unwrap()
                    .strip_prefix("common.json#/definitions/")
                    .unwrap();
                let order: Vec<_> = profile["x-result-order"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .map(|v| v.as_str().unwrap())
                    .collect();
                let minimum = profile["x-min-positional"].as_u64().unwrap();
                output.push_str(&format!("pub struct {rust}; impl crate::contract::ResultProfile for {rust} {{ type Method = super::{endpoint}::{method_rust}; type Reply = {reply}; const PARAM_ORDER: &'static [&'static str] = &{order:?}; const MIN_POSITIONAL: usize = {minimum}; }}\n"));
            }
        }
        output.push_str("}\n");
        output.push_str(&format!(
            "pub mod {endpoint}_notifications {{ #[allow(unused_imports)] use super::*;\n"
        ));
        let mut notifications = Vec::new();
        for notification in document["x-notifications"].as_array().into_iter().flatten() {
            let name = notification["name"].as_str().unwrap();
            let rust = notification["x-rust-name"].as_str().unwrap();
            let params = notification["x-params-schema"]["$ref"]
                .as_str()
                .unwrap()
                .strip_prefix("common.json#/definitions/")
                .unwrap();
            let positional = &notification["x-positional-schema"];
            let positional = if let Some(reference) = positional["$ref"].as_str() {
                common
                    .pointer(reference.strip_prefix("common.json#").unwrap())
                    .unwrap()
            } else {
                positional
            };
            let minimum = positional["minItems"].as_u64().unwrap_or(0);
            let positional_allowed = notification["paramStructure"] != "by-name";
            let order: Vec<_> = notification["params"]
                .as_array()
                .unwrap()
                .iter()
                .map(|p| p["name"].as_str().unwrap())
                .collect();
            let direction = notification["direction"].as_str().unwrap();
            output.push_str(&format!("pub struct {rust}; impl crate::contract::Notification for {rust} {{ type Params = {params}; const NAME: &'static str = {name:?}; const DIRECTION: &'static str = {direction:?}; const PARAM_ORDER: &'static [&'static str] = &{order:?}; const MIN_POSITIONAL: usize = {minimum}; const POSITIONAL: bool = {positional_allowed}; }}\n"));
            notifications.push(name);
        }
        output.push_str(&format!(
            "pub const NOTIFICATIONS: &[&str] = &{notifications:?}; }}\n"
        ));
        output.push_str(&format!(
            "pub mod {endpoint}_exchanges {{ #[allow(unused_imports)] use super::*;\n"
        ));
        let mut exchanges = Vec::new();
        for exchange in document["x-completions"].as_array().into_iter().flatten() {
            let name = exchange["name"].as_str().unwrap();
            let rust = exchange["x-rust-name"].as_str().unwrap();
            let result = exchange["result"]["schema"]["$ref"]
                .as_str()
                .unwrap()
                .strip_prefix("common.json#/definitions/")
                .unwrap();
            let id = exchange["id"].as_u64().unwrap();
            let error = exchange["errors"][0]["code"].as_i64().unwrap();
            output.push_str(&format!("pub struct {rust}; impl crate::contract::Completion for {rust} {{ type Produced = {result}; type Result = {result}; const NAME: &'static str = {name:?}; const ID: u64 = {id}; const ERROR_CODE: i32 = {error}; }}\n"));
            exchanges.push(name);
        }
        output.push_str(&format!(
            "pub const COMPLETIONS: &[&str] = &{exchanges:?}; }}\n"
        ));
    }
    fs::write(
        PathBuf::from(env::var_os("OUT_DIR").unwrap()).join("contract.rs"),
        output,
    )
    .unwrap();
}
