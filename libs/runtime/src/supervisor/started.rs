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
