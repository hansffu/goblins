//! Metadata conversions preserve runtime policy methods and authority.
use crate::{config, policy};
use goblins_protocol::contract::started_metadata as wire;

pub(super) fn encode_started_policy(value: &policy::Policy) -> wire::Policy {
    wire::Policy {
        configuration: value.configuration.clone(),
        description: value.description.clone(),
        run_targets: value.run_targets.clone(),
        integration: value.integration.clone(),
        bypass_permissions: value.bypass_permissions,
        editor: value.editor.as_ref().map(|v| wire::Editor {
            kind: v.kind.clone(),
            command: v.command.clone(),
        }),
        nix: value.nix,
        host_commands: value.host_commands,
        scope: value.scope.as_ref().map(|v| wire::ScopePolicy {
            name: v.name.clone(),
            docker: v.docker,
            pid: v.pid,
        }),
        dev_shell: value.dev_shell.as_ref().map(|v| wire::DevShellPolicy {
            reference: v.reference.clone(),
            path: v.path.clone(),
        }),
        initial_packages: value.initial_packages.clone(),
        handlers: value.handlers.as_ref().map(|v| wire::Handlers {
            on_attach: wire::OnAttach {
                command: v.on_attach.command.clone(),
                timeout: v.on_attach.timeout,
                wait_for: v.on_attach.wait_for.clone(),
            },
            on_detach: v.on_detach.clone(),
        }),
    }
}

pub(super) fn decode_started_policy(value: wire::Policy) -> Box<policy::Policy> {
    Box::new(policy::Policy {
        configuration: value.configuration,
        description: value.description,
        run_targets: value.run_targets,
        integration: value.integration,
        bypass_permissions: value.bypass_permissions,
        editor: value.editor.map(|v| config::Editor {
            kind: v.kind,
            command: v.command,
        }),
        nix: value.nix,
        host_commands: value.host_commands,
        scope: value.scope.map(|v| policy::ScopePolicy {
            name: v.name,
            docker: v.docker,
            pid: v.pid,
        }),
        dev_shell: value.dev_shell.map(|v| policy::DevShellPolicy {
            reference: v.reference,
            path: v.path,
        }),
        initial_packages: value.initial_packages,
        handlers: value.handlers.map(|v| config::Handlers {
            on_attach: config::OnAttach {
                command: v.on_attach.command,
                timeout: v.on_attach.timeout,
                wait_for: v.on_attach.wait_for,
            },
            on_detach: v.on_detach,
        }),
    })
}

macro_rules! feature_bridge {
    ($($name:ident),* $(,)?) => {
        fn encode_feature(value: policy::Feature) -> wire::Feature {
            match value { $(policy::Feature::$name => wire::Feature::$name),* }
        }
        fn decode_feature(value: wire::Feature) -> policy::Feature {
            match value { $(wire::Feature::$name => policy::Feature::$name),* }
        }
    };
}
feature_bridge! {
    LivePackages, DevShells, FlakeApps, NixDaemon, HostJobs,
    Exec, AttachClients, Scopes, SharedPid, Docker,
}

pub(super) fn encode_started_support(
    value: &policy::Support,
) -> goblins_protocol::contract::SupervisorStartedSupport {
    value.0.iter().copied().map(encode_feature).collect()
}

pub(super) fn decode_started_support(
    value: goblins_protocol::contract::SupervisorStartedSupport,
) -> policy::Support {
    policy::Support(value.into_iter().map(decode_feature).collect())
}

pub(super) fn encode_started_identity(value: &crate::Identity) -> wire::Identity {
    wire::Identity {
        pid: value.pid,
        helper_userns: value.helper_userns,
        mount_owner: value.mount_owner,
        source_mountns: value.source_mountns,
        mountns: value.mountns,
    }
}

pub(super) fn decode_started_identity(value: wire::Identity) -> crate::Identity {
    crate::Identity {
        pid: value.pid,
        helper_userns: value.helper_userns,
        mount_owner: value.mount_owner,
        source_mountns: value.source_mountns,
        mountns: value.mountns,
    }
}

fn encode_skipped(values: &[crate::history::Skipped]) -> Vec<wire::Skipped> {
    values
        .iter()
        .map(|v| wire::Skipped {
            name: v.name.clone(),
            reason: v.reason.clone(),
        })
        .collect()
}

fn decode_skipped(values: Vec<wire::Skipped>) -> Vec<crate::history::Skipped> {
    values
        .into_iter()
        .map(|v| crate::history::Skipped {
            name: v.name,
            reason: v.reason,
        })
        .collect()
}

pub(super) fn encode_started_recreated(
    value: &Option<Box<crate::Recreated>>,
) -> Option<Box<wire::Recreated>> {
    value.as_ref().map(|v| {
        Box::new(wire::Recreated {
            packages: v.packages.clone(),
            auto_approve: v.auto_approve.clone(),
            skipped: encode_skipped(&v.skipped),
            fresh: v.fresh.clone(),
        })
    })
}

pub(super) fn decode_started_recreated(
    value: Option<Box<wire::Recreated>>,
) -> Option<Box<crate::Recreated>> {
    value.map(|v| {
        Box::new(crate::Recreated {
            packages: v.packages,
            auto_approve: v.auto_approve,
            skipped: decode_skipped(v.skipped),
            fresh: v.fresh,
        })
    })
}

pub(super) fn encode_started_auto(value: &crate::Auto) -> wire::Auto {
    wire::Auto {
        rooted: value.rooted.clone(),
        skipped: encode_skipped(&value.skipped),
    }
}

pub(super) fn decode_started_auto(value: wire::Auto) -> Box<crate::Auto> {
    Box::new(crate::Auto {
        rooted: value.rooted,
        skipped: decode_skipped(value.skipped),
    })
}

pub(super) fn encode_started_report(value: &crate::worktrees::Report) -> wire::Report {
    wire::Report {
        folder: value.folder.clone(),
        checkout: value.checkout.clone(),
        worktree: value.worktree.clone(),
        detail: value.detail.clone(),
    }
}

pub(super) fn decode_started_report(value: wire::Report) -> Box<crate::worktrees::Report> {
    Box::new(crate::worktrees::Report {
        folder: value.folder,
        checkout: value.checkout,
        worktree: value.worktree,
        detail: value.detail,
    })
}
