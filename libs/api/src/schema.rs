//! Local schema reuse, shared by the build script and runtime shape validator.
use serde_json::Value;

pub fn expand_common(source: Value) -> Value {
    fn expand(schema: &Value, source: &Value, seen: &mut Vec<String>) -> Value {
        let Some(branches) = schema.get("allOf").and_then(Value::as_array) else {
            return schema.clone();
        };
        if branches.len() != 1 || branches[0].as_object().is_none_or(|o| o.len() != 1) {
            return schema.clone();
        }
        let Some(reference) = branches[0].get("$ref").and_then(Value::as_str) else {
            return schema.clone();
        };
        if !reference.starts_with("#/definitions/")
            || schema
                .as_object()
                .unwrap()
                .keys()
                .any(|k| k != "allOf" && k != "description" && k != "title" && !k.starts_with("x-"))
        {
            return schema.clone();
        }
        assert!(
            !seen.iter().any(|s| s == reference),
            "cyclic schema alias: {reference}"
        );
        seen.push(reference.to_owned());
        let target = source
            .pointer(&reference[1..])
            .unwrap_or_else(|| panic!("missing schema alias: {reference}"));
        let mut result = expand(target, source, seen);
        seen.pop();
        let result = result.as_object_mut().unwrap();
        result.retain(|k, _| k != "description" && k != "title" && !k.starts_with("x-rust-"));
        for (key, value) in schema.as_object().unwrap() {
            if key != "allOf" {
                result.insert(key.clone(), value.clone());
            }
        }
        Value::Object(result.clone())
    }
    let mut result = source.clone();
    for (name, schema) in source["definitions"].as_object().unwrap() {
        result["definitions"][name] = expand(schema, &source, &mut Vec::new());
    }
    result
}

// Only the build script consumes OpenRPC objects; runtime uses expand_common.
#[allow(dead_code)]
pub fn expand_document(mut document: Value, common: &Value) -> Value {
    fn resolve<'a>(
        mut value: Value,
        mut owner: &'a Value,
        common: &'a Value,
    ) -> (Value, &'a Value) {
        let mut seen = Vec::new();
        while let Some(reference) = value.get("$ref").and_then(Value::as_str) {
            let pointer = if let Some(pointer) = reference.strip_prefix("common.json#") {
                owner = common;
                pointer
            } else {
                reference
                    .strip_prefix('#')
                    .expect("non-local OpenRPC reference")
            };
            let key = (owner as *const Value, pointer.to_owned());
            assert!(
                !seen.contains(&key),
                "cyclic OpenRPC reference: {reference}"
            );
            seen.push(key);
            value = owner
                .pointer(pointer)
                .unwrap_or_else(|| panic!("missing OpenRPC reference: {reference}"))
                .clone();
        }
        (value, owner)
    }
    fn rebase(value: &mut Value) {
        match value {
            Value::Object(object) => {
                for (key, value) in object {
                    if key == "$ref" && value.as_str().is_some_and(|r| r.starts_with("#/")) {
                        *value = Value::String(format!("common.json{}", value.as_str().unwrap()));
                    } else {
                        rebase(value);
                    }
                }
            }
            Value::Array(values) => {
                for value in values {
                    rebase(value);
                }
            }
            _ => (),
        }
    }
    fn slot(value: Value, owner: &Value, common: &Value) -> Value {
        let (mut value, owner) = resolve(value, owner, common);
        if std::ptr::eq(owner, common) {
            rebase(&mut value);
        }
        value
    }
    let source = document.clone();
    for field in ["methods", "x-notifications", "x-completions"] {
        if let Some(rows) = document.get_mut(field).and_then(Value::as_array_mut) {
            for row in rows {
                let (mut expanded, owner) = resolve(row.clone(), &source, common);
                for field in ["params", "errors"] {
                    if let Some(values) = expanded.get_mut(field).and_then(Value::as_array_mut) {
                        for value in values {
                            *value = slot(value.clone(), owner, common);
                        }
                    }
                }
                if let Some(value) = expanded.get_mut("result") {
                    *value = slot(value.clone(), owner, common);
                }
                if std::ptr::eq(owner, common) {
                    rebase(&mut expanded);
                }
                *row = expanded;
            }
        }
    }
    document
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn shared_source_documents_keep_inline_wire_descriptors() {
        let common: Value = serde_json::from_str(include_str!("../schema/common.json")).unwrap();
        let expanded_common = expand_common(common.clone());
        assert_eq!(
            expanded_common["definitions"]["PermissionGetParams"],
            expanded_common["definitions"]["HostJobParams"]
        );
        for text in [
            include_str!("../schema/host.openrpc.json"),
            include_str!("../schema/sandbox.openrpc.json"),
        ] {
            let document = expand_document(serde_json::from_str(text).unwrap(), &common);
            let initialize = document["methods"]
                .as_array()
                .unwrap()
                .iter()
                .find(|method| method["name"] == "initialize")
                .unwrap();
            assert_eq!(initialize["params"], json!([{
                "name":"api", "required":true,
                "schema":{"$ref":"common.json#/definitions/InitializeParams/properties/api"}
            }]));
            assert_eq!(initialize["result"], json!({
                "name":"result", "schema":{"$ref":"common.json#/definitions/InitializeResult"}
            }));
            assert_eq!(initialize["errors"], json!([
                {"code":-32602,"message":"Invalid parameters"},
                {"code":-32002,"message":"Unsupported API version"},
                {"code":-32009,"message":"Stale or conflicting operation"}
            ]));
        }
    }

    #[test]
    fn nested_shared_components_keep_their_common_document_owner() {
        let common = json!({
            "definitions":{"Text":{"type":"string"}},
            "components":{
                "contentDescriptors":{
                    "Input":{"name":"input","required":true,"schema":{"$ref":"#/definitions/Text"}},
                    "Output":{"name":"result","schema":{"$ref":"#/definitions/Text"}}
                },
                "errors":{"Invalid":{"code":-32602,"message":"Invalid parameters"}}
            },
            "x-methods":{"Shared":{
                "name":"fixture.echo",
                "params":[{"$ref":"#/components/contentDescriptors/Input"}],
                "result":{"$ref":"#/components/contentDescriptors/Output"},
                "errors":[{"$ref":"#/components/errors/Invalid"}],
                "x-params-schema":{"$ref":"#/definitions/Text"}
            }}
        });
        let source = json!({
            "methods":[{"$ref":"common.json#/x-methods/Shared"}],
            "definitions":{"Text":{"type":"integer"}},
            "components":{"errors":{"Invalid":{"code":0,"message":"local"}}}
        });
        let expanded = expand_document(source.clone(), &common);
        assert_eq!(expanded["methods"][0], json!({
            "name":"fixture.echo",
            "params":[{"name":"input","required":true,"schema":{"$ref":"common.json#/definitions/Text"}}],
            "result":{"name":"result","schema":{"$ref":"common.json#/definitions/Text"}},
            "errors":[{"code":-32602,"message":"Invalid parameters"}],
            "x-params-schema":{"$ref":"common.json#/definitions/Text"}
        }));
        assert_eq!(source["methods"][0], json!({"$ref":"common.json#/x-methods/Shared"}));
        assert_eq!(common["x-methods"]["Shared"]["params"][0], json!({"$ref":"#/components/contentDescriptors/Input"}));
    }
}
