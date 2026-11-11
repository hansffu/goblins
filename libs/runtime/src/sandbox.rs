//! Sandbox-side request gate. No daemon state or Linux transport belongs here.
use crate::policy::{Capabilities, Operation, Refusal};
use goblins_jsonrpc::Call;
use goblins_protocol::rpc::{DevShellDiffParams, FlakeRunParams, PermissionParams};
use serde::{Deserialize, de::DeserializeOwned};
use serde_json::Value;

pub type Fault = (i32, String);

/// Current wire mapping; Unsupported stays internal/unreachable on Linux.
pub fn fault(refusal: Refusal) -> Fault {
    match refusal {
        Refusal::Policy { code, message } => (code, message),
        Refusal::Unsupported(feature) => (
            -32009,
            format!(
                "not supported by this sandbox: {}",
                serde_json::to_value(feature)
                    .ok()
                    .and_then(|v| v.as_str().map(str::to_string))
                    .unwrap_or_default()
            ),
        ),
    }
}
fn invalid(message: impl Into<String>) -> Refusal {
    Refusal::Policy {
        code: -32602,
        message: message.into(),
    }
}
fn parse<T: DeserializeOwned>(value: &Value) -> Result<T, Refusal> {
    serde_json::from_value(value.clone()).map_err(|e| invalid(e.to_string()))
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MemberStart {
    pub key: String,
    pub name: String,
    pub agent_name: Option<String>,
    #[serde(default)]
    pub detached: bool,
    /// Start in this worktree of the owner's folder (`goblin run --worktree`).
    #[serde(default)]
    pub worktree: Option<String>,
    /// Launch options (`goblin run -o NAME=CHOICE`); the daemon checks them
    /// against the configuration.
    #[serde(default)]
    pub options: std::collections::BTreeMap<String, String>,
}
use goblins_protocol::contract::SetWorktreeParams as SetWorktree;
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Empty {}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Session {
    session: String,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Job {
    request: String,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Raid {
    raid: String,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Role {
    role: String,
}

pub fn messaging(method: &str) -> bool {
    matches!(
        method,
        "messages.send"
            | "messages.get"
            | "messages.reply"
            | "inbox.next"
            | "inbox.status"
            | "inbox.complete"
            | "inbox.requeue"
    )
}

/// Called only for requests with an ID. Initialize and messaging are answered
/// by the daemon, including between Stop and Stopped. Notifications are dropped
/// by the relay before entering this gate.
pub fn check(
    capabilities: &Capabilities<'_>,
    call: &Call,
    body_bytes: usize,
    initialized: bool,
    stopping: bool,
    trailing: bool,
) -> Result<(), Refusal> {
    let method = call.method.as_str();
    let limit = match method {
        "sessions.set-variables" | "permissions.request" | "terminals.create" => 16384,
        // A terminal takes up to 64 KiB of text to type.
        "terminals.send" => goblins_protocol::messages::MAX_REQUEST,
        _ => 4096,
    };
    if !messaging(method) && method != "communications.list" && body_bytes > limit {
        return Err(invalid("request exceeds method frame limit"));
    }
    if method == "initialize" {
        return Ok(());
    }
    if !initialized {
        return Err(Refusal::Policy {
            code: -32001,
            message: "initialize first".into(),
        });
    }
    if messaging(method) {
        return Ok(());
    }
    if stopping {
        return Err(Refusal::Policy {
            code: -32009,
            message: "stale or conflicting operation".into(),
        });
    }
    // Match the sandbox allowlist, never the host endpoint's methods.
    if !matches!(
        method,
        "permissions.request"
            | "flake.run"
            | "devshell.diff"
            | "sessions.detach"
            | "sessions.attach"
            | "editor.open"
            | "sessions.set-state"
            | "sessions.set-variables"
            | "sessions.set-worktree"
            | "sessions.status"
            | "sessions.list"
            | "sessions.get"
            | "sessions.start"
            | "sessions.stop"
            | "targets.list"
            | "devshell.lock"
            | "raids.create"
            | "raids.leave"
            | "raids.members"
            | "raids.set_role"
            | "host.status"
            | "host.list"
            | "host.cancel"
            | "terminals.create"
            | "terminals.list"
            | "terminals.read"
            | "terminals.send"
            | "terminals.wait"
            | "terminals.close"
    ) {
        return Err(Refusal::Policy {
            code: -32601,
            message: "method unavailable on sandbox endpoint".into(),
        });
    }
    if method == "sessions.attach" {
        return Err(Refusal::Policy { code: -32601, message: "goblins do not attach to other goblins' terminals; the host can, with goblins attach NAME".into() });
    }
    if trailing
        && matches!(
            method,
            "permissions.request" | "flake.run" | "devshell.diff" | "editor.open"
        )
    {
        return Err(Refusal::Policy {
            code: -32600,
            message: "unexpected trailing input".into(),
        });
    }
    let operation = match method {
        "permissions.request" => {
            let p: PermissionParams = parse(&call.params)?;
            if p.host
                .as_ref()
                .and_then(|h| h.name.as_deref())
                .is_some_and(|n| !goblins_protocol::rpc::host_name(n))
            {
                return Err(invalid(goblins_protocol::rpc::HOST_NAME_RULE));
            }
            if !p.validate() {
                return Err(invalid("invalid permission request or reason"));
            }
            Some(match p.kind.as_str() {
                "package" => Operation::Package,
                "devshell" => Operation::DevShellRefresh,
                "docker" => Operation::Docker,
                "host-command" => Operation::HostCommand,
                _ => unreachable!("validated permission kind"),
            })
        }
        "flake.run" => {
            let p: FlakeRunParams = parse(&call.params)?;
            if !p.validate() {
                return Err(invalid("invalid app name"));
            }
            Some(Operation::FlakeRun)
        }
        "devshell.diff" => {
            let p: DevShellDiffParams = parse(&call.params)?;
            if !p.validate() {
                return Err(invalid("invalid flake inputs"));
            }
            Some(Operation::DevShellDiff)
        }
        "sessions.start" => {
            let p: MemberStart = parse(&call.params)?;
            if !goblins_protocol::identifier(&p.key)
                || !goblins_protocol::identifier(&p.name)
                || p.worktree
                    .as_deref()
                    .is_some_and(|n| !crate::worktrees::valid_name(n))
                || p.options.len() > goblins_protocol::MAX_LAUNCH_OPTIONS
                || !p.options.iter().all(|(option, choice)| {
                    goblins_protocol::launch_option_name(option)
                        && goblins_protocol::launch_choice(choice)
                })
            {
                return Err(invalid("invalid launch parameters"));
            }
            Some(Operation::RunMember {
                configuration: p.name,
            })
        }
        "editor.open" => {
            let p: goblins_protocol::editor::Open = parse(&call.params)?;
            p.validate().map_err(invalid)?;
            if capabilities.policy.editor.is_none() {
                return Err(Refusal::Policy {
                    code: -32009,
                    message: "no editor is configured; enable editor.emacs in mkGoblins".into(),
                });
            }
            None
        }
        "sessions.status" | "sessions.list" => {
            let _: goblins_protocol::contract::EmptyParams = parse(&call.params)?;
            None
        }
        "sessions.set-state" => {
            let _: goblins_protocol::SetState = parse(&call.params)?;
            None
        }
        "sessions.set-variables" => {
            let p: goblins_protocol::SetVariables = parse(&call.params)?;
            p.validate().map_err(invalid)?;
            None
        }
        "sessions.set-worktree" => {
            let p: SetWorktree = parse(&call.params)?;
            if p.name
                .as_deref()
                .is_some_and(|n| !crate::worktrees::valid_name(n))
            {
                return Err(invalid("invalid worktree name"));
            }
            None
        }
        "sessions.get" => {
            let _: goblins_protocol::contract::SessionSelectorParams = parse(&call.params)?;
            None
        }
        "sessions.stop" => {
            let Session { session: _session } = parse(&call.params)?;
            None
        }
        "host.status" | "host.cancel" => {
            let Job { request: _request } = parse(&call.params)?;
            None
        }
        // A terminal is an exec. The daemon checks the parameters; the call
        // never names a goblin, so it reaches only the caller's own.
        "terminals.create" => Some(Operation::Exec),
        "terminals.list" | "terminals.read" | "terminals.send" | "terminals.wait"
        | "terminals.close" => None,
        "raids.create" => {
            let Raid { raid: _raid } = parse(&call.params)?;
            None
        }
        "raids.set_role" => {
            let Role { role: _role } = parse(&call.params)?;
            None
        }
        _ => {
            let _: Empty = parse(&call.params)?;
            if method == "sessions.detach" && trailing {
                return Err(Refusal::Policy {
                    code: -32600,
                    message: "unexpected trailing input".into(),
                });
            }
            if method == "devshell.lock" && capabilities.policy.dev_shell.is_none() {
                return Err(invalid("this sandbox was not started with a dev shell"));
            }
            None
        }
    };
    operation
        .as_ref()
        .map_or(Ok(()), |op| capabilities.check(op))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::policy::{DevShellPolicy, Feature, Policy, ScopePolicy, Support};
    use serde_json::json;

    fn call(method: &str, params: Value) -> Call {
        Call {
            jsonrpc: "2.0".into(),
            id: Some(json!(1)),
            method: method.into(),
            params,
        }
    }
    fn policy() -> Policy {
        let mut p = Policy::fixture("shell", &["shell"]);
        p.host_commands = true;
        p.scope = Some(ScopePolicy {
            name: "net".into(),
            docker: true,
            pid: false,
        });
        p.dev_shell = Some(DevShellPolicy {
            reference: ".".into(),
            path: "/work".into(),
        });
        p
    }
    #[test]
    fn every_operation_is_checked_before_a_call_can_reach_daemon_state() {
        let rows = [
            (
                "permissions.request",
                json!({"kind":"package","package":"hello","reason":"t"}),
                Feature::LivePackages,
            ),
            (
                "permissions.request",
                json!({"kind":"devshell","reason":"t"}),
                Feature::DevShells,
            ),
            (
                "permissions.request",
                json!({"kind":"docker","reason":"t"}),
                Feature::Docker,
            ),
            (
                "permissions.request",
                json!({"kind":"host-command","reason":"t","host":{"argv":["true"],"cwd":"/work"}}),
                Feature::HostJobs,
            ),
            ("flake.run", json!({"app":"hello"}), Feature::FlakeApps),
            ("devshell.diff", json!({}), Feature::DevShells),
        ];
        for (method, params, feature) in rows {
            let call = call(method, params);
            let mut policy = policy();
            let mut support = Support::linux();
            assert_eq!(
                check(
                    &Capabilities {
                        policy: &policy,
                        support: &support
                    },
                    &call,
                    100,
                    true,
                    false,
                    false
                ),
                Ok(())
            );
            support.0.remove(&feature);
            assert_eq!(
                check(
                    &Capabilities {
                        policy: &policy,
                        support: &support
                    },
                    &call,
                    100,
                    true,
                    false,
                    false
                ),
                Err(Refusal::Unsupported(feature))
            );
            policy.nix = true;
            policy.host_commands = false;
            policy.scope = None;
            policy.dev_shell = None;
            let (code, message) = fault(
                check(
                    &Capabilities {
                        policy: &policy,
                        support: &support,
                    },
                    &call,
                    100,
                    true,
                    false,
                    false,
                )
                .unwrap_err(),
            );
            assert_eq!(code, -32602);
            let expected = match feature {
                Feature::LivePackages => crate::NIX_PACKAGES,
                Feature::DevShells | Feature::FlakeApps => crate::NIX_DEV_SHELL,
                Feature::Docker => "Docker requires a scope with docker.enable",
                Feature::HostJobs => {
                    "this goblin may not run host commands (mkGoblin hostCommands.enable)"
                }
                _ => unreachable!(),
            };
            assert_eq!(message, expected);
            // Parameters and trailing input precede even a forbidden policy.
            let mut malformed = call;
            malformed.params = json!({"invalid":true});
            let caps = Capabilities {
                policy: &policy,
                support: &support,
            };
            assert_eq!(
                fault(check(&caps, &malformed, 100, true, false, true).unwrap_err()),
                (-32600, "unexpected trailing input".into())
            );
            assert_eq!(
                fault(check(&caps, &malformed, 100, true, false, false).unwrap_err()).0,
                -32602
            );
        }
    }
    #[test]
    fn stop_and_initialize_keep_their_place_before_method_checks() {
        let policy = policy();
        let support = Support::linux();
        let caps = Capabilities {
            policy: &policy,
            support: &support,
        };
        for method in [
            "unknown",
            "permissions.request",
            "sessions.start",
            "sessions.attach",
        ] {
            let c = call(method, json!({"invalid":true}));
            assert_eq!(
                fault(check(&caps, &c, 100, false, true, false).unwrap_err()).0,
                -32001
            );
            assert_eq!(
                fault(check(&caps, &c, 100, true, true, false).unwrap_err()),
                (-32009, "stale or conflicting operation".into())
            );
        }
        for method in ["initialize", "messages.send", "inbox.next"] {
            assert_eq!(
                check(
                    &caps,
                    &call(method, json!({"invalid":true})),
                    100,
                    true,
                    true,
                    true
                ),
                Ok(())
            );
        }
        assert_eq!(
            fault(
                check(
                    &caps,
                    &call("initialize", json!({})),
                    4097,
                    false,
                    true,
                    false
                )
                .unwrap_err()
            )
            .1,
            "request exceeds method frame limit"
        );
    }
    #[test]
    fn run_targets_are_policy_before_raid_or_launch_state() {
        let policy = policy();
        let support = Support::linux();
        let caps = Capabilities {
            policy: &policy,
            support: &support,
        };
        let c = call("sessions.start", json!({"key":"k","name":"other"}));
        assert_eq!(
            fault(check(&caps, &c, 100, true, false, false).unwrap_err()),
            (
                -32602,
                "configuration 'other' is not an allowed run target of 'shell'; allowed: shell"
                    .into()
            )
        );
        let c = call("sessions.start", json!({"key":"!","name":"other"}));
        assert_eq!(
            fault(check(&caps, &c, 100, true, false, false).unwrap_err()).1,
            "invalid launch parameters"
        );
    }
    #[test]
    fn launch_options_are_checked_at_the_gate_and_the_largest_selection_fits() {
        let policy = policy();
        let support = Support::linux();
        let caps = Capabilities {
            policy: &policy,
            support: &support,
        };
        let start = |options: Value| {
            let c = call(
                "sessions.start",
                json!({"key":"k".repeat(64),"name":"shell","agent_name":"a".repeat(32),
                    "detached":true,"worktree":"w".repeat(64),"options":options}),
            );
            let bytes = serde_json::to_vec(&json!({"jsonrpc":"2.0","id":4096,
                "method":c.method,"params":c.params}))
            .unwrap()
            .len();
            check(&caps, &c, bytes, true, false, false).map_err(fault)
        };
        let largest: serde_json::Map<_, _> = (0..16)
            .map(|i| (format!("o{i:x<31}"), json!(format!("{i:x<64}"))))
            .collect();
        assert_eq!(start(json!(largest)), Ok(()));
        let mut many = largest.clone();
        many.insert("extra".into(), json!("x"));
        for bad in [
            json!(many),
            json!({"mo.de": "x"}),
            json!({"mode": "a=b"}),
            json!({"mode": ""}),
            json!({"x".repeat(33): "x"}),
            json!({"mode": "x".repeat(65)}),
        ] {
            assert_eq!(
                start(bad.clone()).unwrap_err().1,
                "invalid launch parameters",
                "{bad}"
            );
        }
        assert_eq!(
            check(
                &caps,
                &call("targets.list", json!({})),
                100,
                true,
                false,
                false
            ),
            Ok(())
        );
        assert!(
            check(
                &caps,
                &call("targets.list", json!({"x":1})),
                100,
                true,
                false,
                false
            )
            .is_err()
        );
    }
    #[test]
    fn terminals_are_an_exec_and_the_sandbox_endpoint_has_no_attachment_methods() {
        let policy = policy();
        let mut support = Support::linux();
        let gate = |support: &Support, method: &str, bytes: usize| {
            check(
                &Capabilities {
                    policy: &policy,
                    support,
                },
                &call(method, json!({})),
                bytes,
                true,
                false,
                false,
            )
        };
        for method in [
            "terminals.create",
            "terminals.list",
            "terminals.read",
            "terminals.send",
            "terminals.wait",
            "terminals.close",
        ] {
            assert_eq!(gate(&support, method, 100), Ok(()), "{method}");
        }
        // A goblin cannot attach, detach or resize a terminal.
        for method in ["terminals.attach", "terminals.detach", "terminals.resize"] {
            assert_eq!(
                fault(gate(&support, method, 100).unwrap_err()),
                (-32601, "method unavailable on sandbox endpoint".into()),
                "{method}"
            );
        }
        // Only text for a terminal may fill a request, and a program its arguments.
        let limit = goblins_protocol::messages::MAX_REQUEST;
        assert_eq!(gate(&support, "terminals.send", limit), Ok(()));
        assert!(gate(&support, "terminals.send", limit + 1).is_err());
        assert_eq!(gate(&support, "terminals.create", 16384), Ok(()));
        assert!(gate(&support, "terminals.create", 16385).is_err());
        assert!(gate(&support, "terminals.read", 4097).is_err());
        // Creating one is the exec operation: without exec it is refused.
        support.0.remove(&Feature::Exec);
        assert_eq!(
            gate(&support, "terminals.create", 100),
            Err(Refusal::Unsupported(Feature::Exec))
        );
        assert_eq!(gate(&support, "terminals.list", 100), Ok(()));
    }
}
