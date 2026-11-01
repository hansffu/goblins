//! What a goblin may do. Two inputs are fixed for its life: `Policy`, what
//! its configuration allows, and `Support`, what the sandbox implementation
//! can do. The third, what is true now, is the daemon's own state.
use crate::config::{Editor, Handlers};
use serde::{Deserialize, Serialize};
use std::{collections::BTreeSet, path::PathBuf};

/// What the configuration allows, resolved by the runtime from the manifest
/// and reported in `Started`. It holds no descriptors or references.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Policy {
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

#[cfg(any(test, feature = "test-support"))]
impl Policy {
    /// A plain configuration for controller tests: no Nix, no host commands,
    /// no scope, no dev shell, no handlers.
    pub fn fixture(configuration: &str, run_targets: &[&str]) -> Self {
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
pub struct ScopePolicy {
    pub name: String,
    /// The scope's definition enables Docker.
    pub docker: bool,
    /// The scope shares its PID namespace.
    pub pid: bool,
}

/// The configured dev shell, which never changes; the current generation
/// does, and is a `DevShellSummary`.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct DevShellPolicy {
    /// The host flake reference as given at launch.
    pub reference: String,
    /// The host flake directory its lock is read from.
    pub path: PathBuf,
}

/// The current dev shell generation, as `devshell.lock` and `status` show it.
/// Starts with `Started` and is replaced by each refresh.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct DevShellSummary {
    pub identity: String,
    pub generation: u32,
    pub lock: serde_json::Value,
}

/// What a sandbox implementation can do. It describes the implementation,
/// not the configuration.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Feature {
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
pub struct Support(pub BTreeSet<Feature>);

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
pub enum Operation {
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
pub enum Refusal {
    /// The configuration does not allow it; today's code and message.
    Policy { code: i32, message: String },
    /// This sandbox implementation cannot do it.
    Unsupported(Feature),
}

/// A view over `Policy` and `Support`, so the supervisor and the daemon can
/// use the same code.
pub struct Capabilities<'a> {
    pub policy: &'a Policy,
    pub support: &'a Support,
}

const NIX_PACKAGES: &str = crate::NIX_PACKAGES;
const NIX_DEV_SHELL: &str = crate::NIX_DEV_SHELL;

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
