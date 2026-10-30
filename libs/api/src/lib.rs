//! Public request data only. This crate has no launcher, Nix or approval authority.
use serde::{Deserialize, Serialize};

pub const MAX_FRAME: usize = 4096;
pub const REQUEST_SOCKET: &str = "/run/goblins/request.sock";
pub const INTEGRATION_PROMPT: &str = "Check your Goblins inbox and process the next item.";

/// Provider-neutral live activity, independent of sandbox lifecycle.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum AgentState {
    #[default]
    Unknown,
    Idle,
    Working,
    WaitingInput,
    WaitingApproval,
}
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SetState {
    pub state: AgentState,
}

/// Variables a goblin records for its own recreate record.
pub const MAX_VARIABLES: usize = 16;
/// `sessions.set-variables`: all given keys change together; an empty value
/// removes its key.
#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SetVariables {
    pub variables: std::collections::BTreeMap<String, String>,
}
impl SetVariables {
    pub fn validate(&self) -> Result<(), String> {
        if self.variables.is_empty() || self.variables.len() > MAX_VARIABLES {
            return Err(format!("set 1 to {MAX_VARIABLES} variables at a time"));
        }
        for (key, value) in &self.variables {
            if !variable_key(key) {
                return Err(format!(
                    "invalid variable name '{}': use [a-z][a-z0-9_]{{0,31}}",
                    plain_text(key)
                ));
            }
            if !variable_value(value) {
                return Err(format!(
                    "invalid value for '{key}': at most 256 bytes of text without control characters"
                ));
            }
        }
        Ok(())
    }
}
pub fn variable_key(key: &str) -> bool {
    let mut bytes = key.bytes();
    (1..=32).contains(&key.len())
        && bytes.next().is_some_and(|b| b.is_ascii_lowercase())
        && bytes.all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_')
}
pub fn variable_value(value: &str) -> bool {
    value.len() <= 256 && !value.chars().any(char::is_control)
}

pub fn scope_name(name: &str) -> bool {
    let mut bytes = name.bytes();
    (1..=64).contains(&name.len())
        && bytes
            .next()
            .is_some_and(|b| b.is_ascii_alphabetic() || b == b'_')
        && bytes.all(|b| b.is_ascii_alphanumeric() || b"_-".contains(&b))
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Request {
    pub id: String,
    #[serde(default = "package_kind")]
    pub kind: String,
    pub package: String,
    pub reason: String,
}
pub fn package_kind() -> String {
    "package".into()
}
#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Reply {
    pub id: Option<String>,
    pub status: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
}
impl Reply {
    pub fn new(id: Option<String>, status: &str, message: Option<String>) -> Self {
        Self {
            id,
            status: status.into(),
            message,
        }
    }
}
pub fn package_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 200
        && name.split('.').all(|part| {
            let mut bytes = part.bytes();
            bytes
                .next()
                .is_some_and(|b| b.is_ascii_alphabetic() || b == b'_')
                && bytes.all(|b| b.is_ascii_alphanumeric() || b"_-".contains(&b))
        })
}
pub fn identifier(name: &str) -> bool {
    (1..=64).contains(&name.len())
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"_-".contains(&b))
}

/// Escape control and bidi formatting characters only, so code and quotes
/// in summaries and diffs stay readable while nothing can drive the terminal
/// or reorder text. The Emacs frontend escapes the same set.
pub fn plain_text(s: &str) -> String {
    s.chars()
        .map(|c| {
            if c.is_control()
                || matches!(
                    c,
                    '\u{2028}'
                        | '\u{2029}'
                        | '\u{202a}'..='\u{202e}'
                        | '\u{2066}'..='\u{2069}'
                )
            {
                format!("\\u{:04x}", c as u32)
            } else {
                c.to_string()
            }
        })
        .collect()
}

pub mod editor;
pub mod init;
pub mod messages;
pub mod patch;
pub mod rpc;
pub mod terminal;
/// `raid members` as text: one line per member, leading with the name a
/// sender can use as a `send` recipient and its role.
pub fn raid_members(record: &serde_json::Value) -> String {
    let text = |v: &serde_json::Value| v.as_str().unwrap_or("-").to_string();
    let mut rows = vec![[
        "NAME".to_string(),
        "ROLE".into(),
        "CONFIGURATION".into(),
        "STATE".into(),
        "OWNER".into(),
        "CREATED".into(),
        "ID".into(),
    ]];
    for m in record["members"].as_array().into_iter().flatten() {
        rows.push([
            text(&m["agent_name"]),
            text(&m["role"]),
            text(&m["name"]),
            text(&m["state"]),
            if m["owner"] == true { "owner" } else { "-" }.into(),
            // Run by an owner of the raid, which may stop it.
            if m["created_by"].is_string() {
                "yes"
            } else {
                "-"
            }
            .into(),
            text(&m["id"]),
        ]);
    }
    let widths: Vec<_> = (0..7)
        .map(|i| rows.iter().map(|r| r[i].chars().count()).max().unwrap_or(0))
        .collect();
    let mut out = format!("Raid {} ({})\n", text(&record["name"]), text(&record["id"]));
    if record["paused"] == true {
        out.push_str(&format!(
            "Paused: the owner {} left; waiting for the host to select a new owner\n",
            text(&record["former_owner"])
        ));
    }
    for row in rows {
        let line: Vec<_> = row
            .iter()
            .zip(&widths)
            .map(|(cell, width)| format!("{cell:width$}"))
            .collect();
        out.push_str(line.join("  ").trim_end());
        out.push('\n');
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn package_grammar_has_no_host_authority() {
        for name in ["jq", "cowsay", "python3Packages.black"] {
            assert!(package_name(name));
        }
        for name in [
            "../hello",
            "nixpkgs#hello",
            "hello^out",
            "--impure",
            "foo..bar",
            "github:owner/repo",
            "foo.\"bar\"",
        ] {
            assert!(!package_name(name));
        }
        assert!(!package_name(&"x".repeat(201)));
    }
    #[test]
    fn variables_are_short_names_and_one_line_values() {
        let set = |pairs: &[(&str, &str)]| SetVariables {
            variables: pairs
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect(),
        };
        assert!(
            set(&[("session_id", "c8193c93"), ("session_name", "Plan it")])
                .validate()
                .is_ok()
        );
        assert!(set(&[("session_name", "")]).validate().is_ok());
        assert!(
            set(&[(&"x".repeat(32), &"é".repeat(128))])
                .validate()
                .is_ok()
        );
        for bad in [
            set(&[]),
            set(&[("Session", "x")]),
            set(&[("1x", "x")]),
            set(&[("a-b", "x")]),
            set(&[(&"x".repeat(33), "x")]),
            set(&[("name", "two\nlines")]),
            set(&[("name", "\u{1b}[31m")]),
            set(&[("name", &"x".repeat(257))]),
        ] {
            assert!(bad.validate().is_err(), "{bad:?}");
        }
        let many: Vec<_> = (0..17)
            .map(|i| (format!("k{i}"), "v".to_string()))
            .collect();
        let many: Vec<_> = many.iter().map(|(k, v)| (k.as_str(), v.as_str())).collect();
        assert!(set(&many).validate().is_err());
    }
    #[test]
    fn plain_text_escapes_only_control_and_bidi_characters() {
        assert_eq!(
            plain_text("a \"q\" é\t\u{1b}[31m\u{202e}x\u{2066}\u{2028}\u{7f}"),
            "a \"q\" é\\u0009\\u001b[31m\\u202ex\\u2066\\u2028\\u007f"
        );
    }
}
