//! Public request data only. This crate has no launcher, Nix or approval authority.
use serde::{Deserialize, Serialize};

pub const MAX_FRAME: usize = 4096;
pub const REQUEST_SOCKET: &str = "/run/goblins/request.sock";
pub const INTEGRATION_PROMPT: &str = "Check your Goblins inbox and process the next item.";

pub fn docker_scope_name(name: &str) -> bool {
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
    #[serde(default)]
    pub scope: Option<String>,
    #[serde(default)]
    pub anonymous: bool,
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

pub mod messages;
pub mod rpc;
pub mod terminal;
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
}
