//! Public RPC parameter and result types and their validation.
use serde::{Deserialize, Serialize};
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PermissionResult {
    pub request: String,
    pub status: String,
    pub message: Option<String>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PermissionParams {
    pub kind: String,
    #[serde(default)]
    pub package: String,
    pub reason: String,
    /// Dev shell refresh: flake inputs to update; empty means all.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub update: Option<Vec<String>>,
    /// Dev shell refresh: lock inputs `flake.nix` declares but the lock lacks.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub lock: bool,
    /// Host command: what the host runs once it approves.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub host: Option<HostCommand>,
}
/// A command a sandbox asks the host to run outside it (`goblin host run`).
/// No shell is involved: `argv` is the program and its arguments.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HostCommand {
    pub argv: Vec<String>,
    /// The sandbox's absolute path; the daemon maps it to the host.
    pub cwd: String,
    /// Added to, or replacing, the daemon's environment.
    #[serde(default)]
    pub env: Vec<(String, String)>,
    /// Seconds before the job is cancelled; none when absent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timeout: Option<u64>,
    /// The request returns once submitted instead of waiting for the job, and
    /// survives its connection closing.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub detach: bool,
}
/// Variables a host command's environment may not override.
pub const HOST_RESERVED_ENV: &[&str] = &["GOBLINS_HOST_JOB"];
/// The total size of a host command's argument and environment strings.
pub const MAX_HOST_COMMAND: usize = 12 * 1024;
impl HostCommand {
    pub fn validate(&self) -> bool {
        let text = |s: &String| !s.contains('\0');
        let size: usize = self.argv.iter().map(String::len).sum::<usize>()
            + self
                .env
                .iter()
                .map(|(k, v)| k.len() + v.len())
                .sum::<usize>()
            + self.cwd.len();
        (1..=crate::MAX_ARGS).contains(&self.argv.len())
            && self.argv.iter().all(text)
            && !self.argv[0].is_empty()
            && self.cwd.starts_with('/')
            && text(&self.cwd)
            && self.env.len() <= crate::MAX_ENV
            && self.env.iter().all(|(k, v)| {
                crate::env_name(k) && text(v) && !HOST_RESERVED_ENV.contains(&k.as_str())
            })
            && self.timeout.is_none_or(|t| t > 0)
            && size <= MAX_HOST_COMMAND
    }
}
/// A flake input name, or a `/`-separated path to a nested input.
pub fn input_name(name: &str) -> bool {
    (1..=200).contains(&name.len())
        && name.split('/').all(|part| {
            let mut bytes = part.bytes();
            bytes
                .next()
                .is_some_and(|b| b.is_ascii_alphabetic() || b == b'_')
                && bytes.all(|b| b.is_ascii_alphanumeric() || b"_-'".contains(&b))
        })
}
impl PermissionParams {
    pub fn validate(&self) -> bool {
        let refresh = self.update.is_some() || self.lock;
        let host = self.host.is_some();
        ((self.kind == "package" && crate::package_name(&self.package) && !refresh && !host)
            || (self.kind == "docker" && self.package.is_empty() && !refresh && !host)
            || (self.kind == "devshell"
                && self.package.is_empty()
                && !host
                && !(self.update.is_some() && self.lock)
                && self
                    .update
                    .as_ref()
                    .is_none_or(|u| u.len() <= 32 && u.iter().all(|i| input_name(i))))
            || (self.kind == "host-command"
                && self.package.is_empty()
                && !refresh
                && self.host.as_ref().is_some_and(HostCommand::validate)))
            && (1..=1024).contains(&self.reason.chars().count())
    }
}
/// `flake.run`: an app of the sandbox's dev shell flake, `default` for none.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FlakeRunParams {
    pub app: String,
}
impl FlakeRunParams {
    pub fn validate(&self) -> bool {
        (1..=256).contains(&self.app.len())
            && self
                .app
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"_-+'".contains(&b))
    }
}
/// `devshell.diff`: the changes a refresh would make, as in
/// `PermissionParams`, without a reason since nothing is requested.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DevShellDiffParams {
    /// Flake inputs to update; empty means all.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub update: Option<Vec<String>>,
    /// Lock inputs `flake.nix` declares but the lock lacks.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub lock: bool,
}
impl DevShellDiffParams {
    pub fn validate(&self) -> bool {
        !(self.update.is_some() && self.lock)
            && self
                .update
                .as_ref()
                .is_none_or(|u| u.len() <= 32 && u.iter().all(|i| input_name(i)))
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::{Value, json};
    #[test]
    fn host_commands_are_bounded_argument_lists() {
        let request = |host: Value| -> PermissionParams {
            serde_json::from_value(json!({"kind":"host-command","reason":"tests","host":host}))
                .unwrap()
        };
        let ok =
            json!({"argv":["cargo","test"],"cwd":"/home/u/project","env":[["RUST_LOG","debug"]]});
        assert!(request(ok.clone()).validate());
        for bad in [
            json!({"argv":[],"cwd":"/p"}),
            json!({"argv":[""],"cwd":"/p"}),
            json!({"argv":["a\0b"],"cwd":"/p"}),
            json!({"argv":["true"],"cwd":"relative"}),
            json!({"argv":["true"],"cwd":"/p","env":[["BAD-NAME","x"]]}),
            json!({"argv":["true"],"cwd":"/p","env":[["GOBLINS_HOST_JOB","x"]]}),
            json!({"argv":["true"],"cwd":"/p","timeout":0}),
            json!({"argv":["x".repeat(MAX_HOST_COMMAND)],"cwd":"/p"}),
        ] {
            assert!(!request(bad.clone()).validate(), "{bad}");
        }
        // Only host commands carry one, and they need one.
        let mut package: PermissionParams =
            serde_json::from_value(json!({"kind":"package","package":"jq","reason":"x"})).unwrap();
        assert!(package.validate());
        package.host = request(ok).host;
        assert!(!package.validate());
        let bare: PermissionParams =
            serde_json::from_value(json!({"kind":"host-command","reason":"x"})).unwrap();
        assert!(!bare.validate());
        assert!(serde_json::from_value::<PermissionParams>(
            json!({"kind":"host-command","reason":"x","host":{"argv":["true"],"cwd":"/p","shell":true}})
        )
        .is_err());
    }
    #[test]
    fn devshell_diff_params_match_refresh_rules() {
        let diff = |v: Value| serde_json::from_value::<DevShellDiffParams>(v);
        for ok in [
            json!({}),
            json!({"update":[]}),
            json!({"update":["nixpkgs","a/b"]}),
            json!({"lock":true}),
        ] {
            assert!(diff(ok.clone()).unwrap().validate(), "{ok}");
        }
        for bad in [
            json!({"update":["nixpkgs"],"lock":true}),
            json!({"update":["../x"]}),
            json!({"update":["-x"]}),
            json!({"update":vec!["a"; 33]}),
        ] {
            assert!(!diff(bad.clone()).unwrap().validate(), "{bad}");
        }
        assert!(diff(json!({"reason":"x"})).is_err());
    }
}
