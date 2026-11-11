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
    let mut types = typify::TypeSpace::new(&settings);
    types
        .add_root_schema(serde_json::from_value(common.clone()).unwrap())
        .unwrap();
    let mut output = types.to_stream().to_string();
    output.push_str(&format!(
        "\npub const API_VERSION: u32 = {};\n",
        common["x-api-version"]
    ));
    let mut decoded = std::collections::BTreeSet::from([
        "InitializeReply".to_string(),
        "ServerStatusReply".to_string(),
        "ServerStopResult".to_string(),
        "SetVariablesResult".to_string(),
    ]);
    for endpoint in ["host", "sandbox"] {
        let document: Value = serde_json::from_str(
            &fs::read_to_string(format!("schema/{endpoint}.openrpc.json")).unwrap(),
        )
        .unwrap();
        output.push_str(&format!("pub mod {endpoint} {{ use super::*;\n"));
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
            output.push_str(&format!("pub struct {rust}; impl crate::contract::Method for {rust} {{ type Params = {params}; type Produced = {result}; type Result = {reply}; const NAME: &'static str = {name:?}; }}\n"));
            names.push(name);
        }
        output.push_str(&format!("pub const METHODS: &[&str] = &{names:?}; }}\n"));
    }
    fs::write(
        PathBuf::from(env::var_os("OUT_DIR").unwrap()).join("contract.rs"),
        output,
    )
    .unwrap();
}
