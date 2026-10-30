//! What a goblin may do. Two inputs are fixed for its life: `Policy`, what
//! its configuration allows, and `Support`, what the sandbox implementation
//! can do. The third, what is true now, is the daemon's own state.
use super::Fault;
use crate::config::{Editor, Handlers};
use serde::{Deserialize, Serialize};
use std::{collections::BTreeSet, path::PathBuf};

/// What the configuration allows, resolved by the runtime from the manifest
/// and reported in `Started`. It holds no descriptors or references.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub(crate) struct Policy {
    pub configuration: String,
    pub description: String,
    /// `allowedRunTargets`.
    pub run_targets: Vec<String>,
    pub integration: Option<String>,
    pub bypass_permissions: bool,
    pub editor: Option<Editor>,
    /// `allowNix`: the whole store and the host Nix daemon.
    pub nix: bool,
    /// `hostCommands.enable`.
    pub host_commands: bool,
    pub scope: Option<ScopePolicy>,
    pub dev_shell: Option<DevShellPolicy>,
    /// Exact store outputs the sandbox starts with.
    pub initial_packages: Vec<PathBuf>,
    /// `onAttach` and `onDetach`.
    pub handlers: Option<Handlers>,
}

#[cfg(test)]
impl Policy {
    /// A plain configuration for controller tests: no Nix, no host commands,
    /// no scope, no dev shell, no handlers.
    pub(crate) fn fixture(configuration: &str, run_targets: &[&str]) -> Self {
        Self {
            configuration: configuration.into(),
            description: "Test shell running in a Goblins sandbox".into(),
            run_targets: run_targets.iter().map(|t| t.to_string()).collect(),
            integration: None,
            bypass_permissions: false,
            editor: None,
            nix: false,
            host_commands: false,
            scope: None,
            dev_shell: None,
            initial_packages: vec![],
            handlers: None,
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub(crate) struct ScopePolicy {
    pub name: String,
    /// The scope's definition enables Docker.
    pub docker: bool,
    /// The scope shares its PID namespace.
    pub pid: bool,
}

/// The configured dev shell, which never changes; the current generation
/// does, and is a `DevShellSummary`.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub(crate) struct DevShellPolicy {
    /// The host flake reference as given at launch.
    pub reference: String,
    /// The host flake directory its lock is read from.
    pub path: PathBuf,
}

/// The current dev shell generation, as `devshell.lock` and `status` show it.
/// Starts with `Started` and is replaced by each refresh.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub(crate) struct DevShellSummary {
    pub identity: String,
    pub generation: u32,
    pub lock: serde_json::Value,
}

/// What a sandbox implementation can do. It describes the implementation,
/// not the configuration.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub(crate) enum Feature {
    /// Mount new store paths into the running sandbox and publish a new
    /// package profile generation (`permissions.request` package).
    LivePackages,
    /// Evaluate a host dev shell and refresh it in place
    /// (`permissions.request` devshell, `devshell.diff`).
    DevShells,
    /// Build a flake app and mount its closure (`flake.run`).
    FlakeApps,
    /// The whole store and the host Nix daemon (`allowNix`).
    NixDaemon,
    /// Run approved commands on the host (`hostCommands.enable`).
    HostJobs,
    /// Start more processes in the running sandbox (`goblins exec`, TRAMP).
    Exec,
    /// A fresh PTY client per attachment (`onAttach`).
    AttachClients,
    /// Join a scope's network namespace and storage.
    Scopes,
    /// Share a scope's PID namespace.
    SharedPid,
    /// Use the scope's Docker engine.
    Docker,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct Support(pub BTreeSet<Feature>);

impl Support {
    /// Linux reports every feature.
    pub fn linux() -> Self {
        use Feature::*;
        Self(
            [
                LivePackages,
                DevShells,
                FlakeApps,
                NixDaemon,
                HostJobs,
                Exec,
                AttachClients,
                Scopes,
                SharedPid,
                Docker,
            ]
            .into(),
        )
    }
    pub fn has(&self, feature: Feature) -> bool {
        self.0.contains(&feature)
    }
    /// The first feature `policy` needs that this implementation lacks. A
    /// feature counts only where it is needed: a goblin without `onAttach`
    /// never needs `AttachClients`, and `Docker` only for one that attaches
    /// the scope's engine as it starts (`docker_at_start`), configured or
    /// inherited from its raid owner.
    pub fn lacks(&self, policy: &Policy, docker_at_start: bool) -> Option<Feature> {
        let mut needs = vec![];
        if docker_at_start {
            needs.push(Feature::Docker);
        }
        if policy.nix {
            needs.push(Feature::NixDaemon);
        }
        if policy.host_commands {
            needs.push(Feature::HostJobs);
        }
        if let Some(scope) = &policy.scope {
            needs.push(Feature::Scopes);
            if scope.pid {
                needs.push(Feature::SharedPid);
            }
        }
        if policy.handlers.is_some() {
            needs.push(Feature::AttachClients);
        }
        if policy.dev_shell.is_some() {
            needs.push(Feature::DevShells);
        }
        needs.into_iter().find(|f| !self.has(*f))
    }
}

/// What a goblin may be asked to do.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) enum Operation {
    Package,
    DevShellRefresh,
    DevShellDiff,
    FlakeRun,
    Docker,
    HostCommand,
    Exec,
    /// Start an attach client or its `onDetach`.
    Attach,
    RunMember {
        configuration: String,
    },
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) enum Refusal {
    /// The configuration does not allow it; today's code and message.
    Policy { code: i32, message: String },
    /// This sandbox implementation cannot do it.
    Unsupported(Feature),
}

impl From<Refusal> for Fault {
    fn from(refusal: Refusal) -> Self {
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
}

/// A view over `Policy` and `Support`, so the supervisor and the daemon can
/// use the same code.
pub(crate) struct Capabilities<'a> {
    pub policy: &'a Policy,
    pub support: &'a Support,
}

const NIX_PACKAGES: &str = crate::session::NIX_PACKAGES;
const NIX_DEV_SHELL: &str = crate::session::NIX_DEV_SHELL;

impl Capabilities<'_> {
    /// Policy first, then Support.
    pub fn check(&self, operation: &Operation) -> Result<(), Refusal> {
        let policy = |code: i32, message: &str| Refusal::Policy {
            code,
            message: message.to_string(),
        };
        let no_dev_shell = || policy(-32602, "this sandbox was not started with a dev shell");
        let feature = match operation {
            Operation::Package => {
                if self.policy.nix {
                    return Err(policy(-32602, NIX_PACKAGES));
                }
                Feature::LivePackages
            }
            Operation::DevShellRefresh => {
                if self.policy.nix {
                    return Err(policy(-32602, NIX_DEV_SHELL));
                }
                if self.policy.dev_shell.is_none() {
                    return Err(no_dev_shell());
                }
                Feature::DevShells
            }
            Operation::DevShellDiff => {
                if self.policy.nix {
                    return Err(policy(-32602, NIX_DEV_SHELL));
                }
                if self.policy.dev_shell.is_none() {
                    return Err(no_dev_shell());
                }
                Feature::DevShells
            }
            Operation::FlakeRun => {
                if self.policy.nix {
                    return Err(policy(-32602, NIX_DEV_SHELL));
                }
                if self.policy.dev_shell.is_none() {
                    return Err(no_dev_shell());
                }
                Feature::FlakeApps
            }
            Operation::Docker => {
                if !self.policy.scope.as_ref().is_some_and(|s| s.docker) {
                    return Err(policy(-32602, "Docker requires a scope with docker.enable"));
                }
                Feature::Docker
            }
            Operation::HostCommand => {
                if !self.policy.host_commands {
                    return Err(policy(
                        -32602,
                        "this goblin may not run host commands (mkGoblin hostCommands.enable)",
                    ));
                }
                Feature::HostJobs
            }
            Operation::Exec => Feature::Exec,
            Operation::Attach => {
                // Starting an attach client: an onAttach or onDetach process.
                // A single-terminal goblin never does, so it is never
                // affected by a missing AttachClients.
                if self.policy.handlers.is_none() {
                    return Err(policy(-32009, "no attach handlers"));
                }
                Feature::AttachClients
            }
            Operation::RunMember { configuration } => {
                let own = &self.policy.configuration;
                let targets = &self.policy.run_targets;
                if targets.is_empty() {
                    return Err(policy(
                        -32602,
                        &format!("configuration '{own}' permits no run targets"),
                    ));
                }
                if !targets.iter().any(|t| t == configuration) {
                    return Err(policy(
                        -32602,
                        &format!(
                            "configuration '{configuration}' is not an allowed run target of '{own}'; allowed: {}",
                            targets.join(", ")
                        ),
                    ));
                }
                return Ok(());
            }
        };
        if self.support.has(feature) {
            Ok(())
        } else {
            Err(Refusal::Unsupported(feature))
        }
    }
}

impl Support {
    /// The protocol features of a sandbox's `initialize` result that this
    /// implementation supports, in today's order.
    pub fn sandbox_features(&self) -> Vec<&'static str> {
        let gated = |feature: Feature, name: &'static str| self.has(feature).then_some(name);
        [
            gated(Feature::Scopes, "scopes"),
            gated(Feature::Docker, "docker-enable"),
            gated(Feature::LivePackages, "package-grants"),
            Some("terminal-detach"),
            Some("sandbox-status"),
            Some("agent-activity-v1"),
            Some("agent-inbox-v1"),
            gated(Feature::FlakeApps, "flake-run"),
            gated(Feature::DevShells, "devshell-diff"),
            Some("run-targets"),
            Some("raids"),
            Some("raid-owner"),
            Some("editor-open"),
            Some("variables"),
            gated(Feature::HostJobs, "host-commands"),
        ]
        .into_iter()
        .flatten()
        .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
                on_attach: crate::config::OnAttach {
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
        let (code, message): Fault = Refusal::Unsupported(Feature::LivePackages).into();
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
