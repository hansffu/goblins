//! Legacy display projection for dev shell previews. Producer DTOs stay strict.
use super::*;

fn optional(v: &Value, key: &str) -> Option<String> {
    v.get(key).and_then(Value::as_str).map(str::to_owned)
}
fn text(v: &Value, key: &str) -> String {
    optional(v, key).unwrap_or_default()
}
fn list<'a>(v: &'a Value, key: &str) -> &'a [Value] {
    v.get(key)
        .and_then(Value::as_array)
        .map_or(&[], Vec::as_slice)
}
fn number(v: &Value, key: &str) -> PreviewNumber {
    let value = v.get(key).unwrap_or(&Value::Null);
    PreviewNumber {
        json: value.to_string(),
        unsigned: value.as_u64(),
    }
}
fn locked(v: &Value, key: &str) -> Option<PreviewLockedReply> {
    let value = v.get(key)?;
    value.as_object()?;
    Some(PreviewLockedReply {
        source: optional(value, "source"),
        id: text(value, "id"),
        date: optional(value, "date"),
    })
}
fn versions(v: &Value, key: &str) -> Option<Vec<String>> {
    Some(
        v.get(key)?
            .as_array()?
            .iter()
            .map(|v| v.as_str().unwrap_or_default().to_owned())
            .collect(),
    )
}

/// Preserve the independently read fields of historical CLI/TUI previews.
/// Permission snapshots retain Value until C09; project them at display entry.
pub fn preview_reply(v: &Value) -> DevShellPreviewReply {
    DevShellPreviewReply {
        reference: text(v, "reference"),
        from: number(v, "from"),
        to: number(v, "to"),
        attention: list(v, "attention")
            .iter()
            .map(|n| PreviewAttentionReply {
                kind: text(n, "kind"),
                input: text(n, "input"),
                source: text(n, "source"),
                old: text(n, "old"),
                new: text(n, "new"),
                count: number(n, "count"),
            })
            .collect(),
        files: list(v, "files")
            .iter()
            .map(|f| PreviewFileReply {
                path: text(f, "path"),
                added: number(f, "added"),
                removed: number(f, "removed"),
            })
            .collect(),
        inputs: list(v, "inputs")
            .iter()
            .map(|c| PreviewInputReply {
                name: text(c, "name"),
                change: text(c, "change"),
                old: locked(c, "old"),
                new: locked(c, "new"),
            })
            .collect(),
        packages: list(v, "packages")
            .iter()
            .map(|c| PreviewPackageReply {
                name: text(c, "name"),
                change: text(c, "change"),
                old: versions(c, "old"),
                new: versions(c, "new"),
            })
            .collect(),
        env: list(v, "env")
            .iter()
            .map(|c| PreviewEnvReply {
                name: text(c, "name"),
                change: text(c, "change"),
                old: optional(c, "old"),
                new: optional(c, "new"),
                long: c.get("long") == Some(&Value::Bool(true)),
            })
            .collect(),
        fetch: number(v, "fetch"),
        build: number(v, "build"),
        download: optional(v, "download"),
        incoming: list(v, "incoming")
            .iter()
            .map(|c| PreviewIncomingReply {
                action: text(c, "action"),
                name: text(c, "name"),
                versions: versions(c, "versions"),
            })
            .collect(),
        diff: text(v, "diff"),
    }
}
impl Response for DevShellPreviewReply {
    const SCHEMA: &'static str = "DevShellPreviewReply";
    fn decode(value: Value) -> io::Result<Self> {
        if !value.is_object() {
            return Err(io::Error::other("daemon returned an invalid preview"));
        }
        Ok(preview_reply(&value))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    #[test]
    fn generated_diff_params_match_original_decoder_and_validation() {
        #[derive(serde::Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Old {
            #[serde(default)]
            update: Option<Vec<String>>,
            #[serde(default)]
            lock: bool,
        }
        let fixtures: Value =
            serde_json::from_str(include_str!("../../schema/fixtures.json")).unwrap();
        for case in fixtures["devshell_diff_parameter_cases"]
            .as_array()
            .unwrap()
        {
            let value = case["params"].clone();
            let old = serde_json::from_value::<Old>(value.clone());
            let new = decode_params::<sandbox::DevShellDiff>(value);
            assert_eq!(old.is_ok(), new.is_ok(), "{case}");
            if let (Ok(old), Ok(new)) = (old, new) {
                assert_eq!(old.update, new.update);
                assert_eq!(old.lock, new.lock);
                let valid = !(old.update.is_some() && old.lock)
                    && old.update.as_ref().is_none_or(|u| {
                        u.len() <= 32 && u.iter().all(|i| crate::rpc::input_name(i))
                    });
                assert_eq!(new.validate(), valid);
                assert_eq!(valid, case["valid"].as_bool().unwrap(), "{case}");
            }
        }
    }
    #[test]
    fn preview_projection_preserves_presence_and_independent_fields() {
        let fixtures: Value =
            serde_json::from_str(include_str!("../../schema/fixtures.json")).unwrap();
        let v =
            DevShellPreviewReply::decode(fixtures["devshell_preview_legacy"][1].clone()).unwrap();
        assert_eq!(v.reference, "");
        assert_eq!(v.from.json, "\"old\"");
        assert_eq!(v.from.unsigned, None);
        assert_eq!(v.to.json, "{\"future\":true}");
        assert_eq!(v.fetch.unsigned, None);
        assert_eq!(v.attention.len(), 3);
        assert_eq!(v.attention[0].kind, "");
        assert_eq!(v.inputs[0].old.as_ref().unwrap().id, "");
        assert_eq!(v.inputs[0].old.as_ref().unwrap().source, None);
        assert_eq!(
            v.inputs[0].new.as_ref().unwrap().source.as_deref(),
            Some("")
        );
        assert_eq!(v.packages[0].old.as_ref().unwrap(), &vec!["", "", "", "1"]);
        assert_eq!(v.packages[0].new.as_ref().unwrap(), &Vec::<String>::new());
        assert!(!v.env[0].long);
        assert_eq!(v.env[0].old, None);
        assert_eq!(v.env[1].new, None);
        assert!(v.incoming[1].versions.is_none());
        assert_eq!(v.download, None);
        assert_eq!(v.diff, "");
        for nonobject in [Value::Null, json!([]), json!(1), json!("preview")] {
            assert_eq!(
                DevShellPreviewReply::decode(nonobject)
                    .unwrap_err()
                    .to_string(),
                "daemon returned an invalid preview"
            );
        }
    }
}
