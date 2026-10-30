use super::Fault;
use goblins_runtime::policy::*;
pub fn fault(refusal: Refusal) -> Fault {
    match refusal {
        Refusal::Policy { code, message } => (code, message),
        // Unreachable on Linux, which supports every feature. The code
        // and its API.org entry belong to the first backend that reports
        // reduced Support (plan 4.4), not to this refactor.
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
#[cfg(test)]
mod tests {
    use super::*;
    use goblins_runtime::config::Handlers;
    use goblins_runtime::{NIX_DEV_SHELL, NIX_PACKAGES};

    pub(crate) fn policy() -> Policy {
        Policy {
            configuration: "shell".into(),
            description: String::new(),
            run_targets: vec!["shell".into()],
            integration: None,
            bypass_permissions: false,
            editor: None,
            nix: false,
            host_commands: true,
            scope: Some(ScopePolicy {
                name: "net".into(),
                docker: true,
                pid: true,
            }),
            dev_shell: Some(DevShellPolicy {
                reference: ".".into(),
                path: "/work".into(),
            }),
            initial_packages: vec![],
            handlers: Some(Handlers {
                on_attach: goblins_runtime::config::OnAttach {
                    command: vec!["client".into()],
                    timeout: 30,
                    wait_for: vec![],
                },
                on_detach: None,
            }),
        }
    }
    fn operations() -> Vec<(Operation, Feature)> {
        vec![
            (Operation::Package, Feature::LivePackages),
            (Operation::DevShellRefresh, Feature::DevShells),
            (Operation::DevShellDiff, Feature::DevShells),
            (Operation::FlakeRun, Feature::FlakeApps),
            (Operation::Docker, Feature::Docker),
            (Operation::HostCommand, Feature::HostJobs),
            (Operation::Exec, Feature::Exec),
            (Operation::Attach, Feature::AttachClients),
        ]
    }

    #[test]
    fn linux_supports_everything_the_policy_allows_and_lists_todays_features() {
        let (policy, support) = (policy(), Support::linux());
        let capabilities = Capabilities {
            policy: &policy,
            support: &support,
        };
        for (operation, _) in operations() {
            assert_eq!(capabilities.check(&operation), Ok(()), "{operation:?}");
        }
        assert_eq!(
            support.sandbox_features(),
            [
                "scopes",
                "docker-enable",
                "package-grants",
                "terminal-detach",
                "sandbox-status",
                "agent-activity-v1",
                "agent-inbox-v1",
                "flake-run",
                "devshell-diff",
                "run-targets",
                "raids",
                "raid-owner",
                "editor-open",
                "variables",
                "host-commands"
            ]
        );
        assert_eq!(support.lacks(&policy, false), None);
    }

    #[test]
    fn a_missing_feature_refuses_exactly_its_operations_and_drops_its_protocol_feature() {
        let policy = policy();
        for (operation, missing) in operations() {
            let mut support = Support::linux();
            support.0.remove(&missing);
            let capabilities = Capabilities {
                policy: &policy,
                support: &support,
            };
            for (other, feature) in operations() {
                let expected = if feature == missing {
                    Err(Refusal::Unsupported(missing))
                } else {
                    Ok(())
                };
                assert_eq!(
                    capabilities.check(&other),
                    expected,
                    "{missing:?} {other:?}"
                );
            }
            let features = support.sandbox_features();
            let name = match missing {
                Feature::LivePackages => Some("package-grants"),
                Feature::Docker => Some("docker-enable"),
                Feature::DevShells => Some("devshell-diff"),
                Feature::FlakeApps => Some("flake-run"),
                Feature::HostJobs => Some("host-commands"),
                _ => None,
            };
            assert_eq!(
                features.len(),
                15 - usize::from(name.is_some()),
                "{operation:?}"
            );
            if let Some(name) = name {
                assert!(!features.contains(&name));
            }
        }
        let mut support = Support::linux();
        support.0.remove(&Feature::Scopes);
        assert!(!support.sandbox_features().contains(&"scopes"));
    }

    #[test]
    fn policy_refuses_before_support_with_todays_codes_and_messages() {
        let mut policy = policy();
        policy.nix = true;
        policy.host_commands = false;
        policy.scope = None;
        policy.dev_shell = None;
        let mut support = Support::linux();
        support.0.clear();
        let capabilities = Capabilities {
            policy: &policy,
            support: &support,
        };
        let refused = |operation: Operation| match capabilities.check(&operation) {
            Err(Refusal::Policy { code, message }) => (code, message),
            other => panic!("{operation:?}: {other:?}"),
        };
        assert_eq!(refused(Operation::Package), (-32602, NIX_PACKAGES.into()));
        assert_eq!(
            refused(Operation::DevShellRefresh),
            (-32602, NIX_DEV_SHELL.into())
        );
        assert_eq!(refused(Operation::FlakeRun).1, NIX_DEV_SHELL);
        assert_eq!(
            refused(Operation::Docker).1,
            "Docker requires a scope with docker.enable"
        );
        assert_eq!(
            refused(Operation::HostCommand).1,
            "this goblin may not run host commands (mkGoblin hostCommands.enable)"
        );
        // Without nix the dev shell is what is missing.
        policy.nix = false;
        let capabilities = Capabilities {
            policy: &policy,
            support: &support,
        };
        assert_eq!(
            capabilities.check(&Operation::DevShellDiff),
            Err(Refusal::Policy {
                code: -32602,
                message: "this sandbox was not started with a dev shell".into()
            })
        );
        let (code, message): Fault = fault(Refusal::Unsupported(Feature::LivePackages));
        assert_eq!(
            (code, message.as_str()),
            (-32009, "not supported by this sandbox: live-packages")
        );
    }

    #[test]
    fn run_targets_keep_todays_two_messages() {
        let mut policy = policy();
        let support = Support::linux();
        let member = |policy: &Policy, target: &str| {
            Capabilities {
                policy,
                support: &support,
            }
            .check(&Operation::RunMember {
                configuration: target.into(),
            })
        };
        assert_eq!(member(&policy, "shell"), Ok(()));
        assert_eq!(
            member(&policy, "other"),
            Err(Refusal::Policy {
                code: -32602,
                message:
                    "configuration 'other' is not an allowed run target of 'shell'; allowed: shell"
                        .into()
            })
        );
        policy.run_targets.clear();
        assert_eq!(
            member(&policy, "shell"),
            Err(Refusal::Policy {
                code: -32602,
                message: "configuration 'shell' permits no run targets".into()
            })
        );
    }

    #[test]
    fn a_feature_counts_only_where_the_policy_needs_it() {
        let mut policy = policy();
        let mut support = Support::linux();
        support.0.remove(&Feature::AttachClients);
        support.0.remove(&Feature::SharedPid);
        assert_eq!(support.lacks(&policy, false), Some(Feature::SharedPid));
        policy.scope.as_mut().unwrap().pid = false;
        assert_eq!(support.lacks(&policy, false), Some(Feature::AttachClients));
        policy.handlers = None;
        // A configured dev shell needs the feature that evaluates it.
        support.0.remove(&Feature::DevShells);
        assert_eq!(support.lacks(&policy, false), Some(Feature::DevShells));
        policy.dev_shell = None;
        assert_eq!(support.lacks(&policy, false), None);
        // Docker counts for a goblin that attaches it as it starts, however
        // that was decided, and for no other.
        support.0.remove(&Feature::Docker);
        assert_eq!(support.lacks(&policy, true), Some(Feature::Docker));
        assert_eq!(support.lacks(&policy, false), None);
        // A goblin without handlers has no client to start, whatever the
        // support.
        let capabilities = Capabilities {
            policy: &policy,
            support: &support,
        };
        assert_eq!(
            capabilities.check(&Operation::Attach),
            Err(Refusal::Policy {
                code: -32009,
                message: "no attach handlers".into()
            })
        );
        support.0.remove(&Feature::Scopes);
        assert_eq!(support.lacks(&policy, false), Some(Feature::Scopes));
    }
}
