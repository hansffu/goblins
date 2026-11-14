//! One session worker: blocking host work is kept off the controller event pump.
use crate::Result;
use crate::engine::Published;
use crate::process::Cancellation as Cancel;
use crate::process::WORKER_TICK;
use crate::session::Member;
use crate::session::Session;
use goblins_protocol::Reply;
use goblins_runtime::Command;
use goblins_runtime::Event;
use goblins_runtime::Handoff;
use goblins_runtime::policy::Support;
use goblins_unix as unix;
use std::{
    os::{
        fd::{AsRawFd, OwnedFd},
        unix::net::UnixListener,
    },
    path::PathBuf,
    sync::mpsc::{self, Receiver, SyncSender},
    thread::{self, JoinHandle},
};

/// A command for the worker, with the token that cancels it.
pub(super) type Work = (Command, Cancel);
/// Sending and final receiver draining share a small gate, so an accepted
/// queued request cannot slip between the final drain and receiver close.
pub(super) struct WorkSender {
    sender: SyncSender<Work>,
    open: std::sync::Arc<std::sync::Mutex<bool>>,
}
impl WorkSender {
    pub fn new(sender: SyncSender<Work>) -> Self {
        Self {
            sender,
            open: std::sync::Arc::new(std::sync::Mutex::new(true)),
        }
    }
    pub fn try_send(&self, work: Work) -> std::result::Result<(), mpsc::TrySendError<Work>> {
        let open = self.open.lock().unwrap_or_else(|p| p.into_inner());
        if !*open {
            return Err(mpsc::TrySendError::Disconnected(work));
        }
        self.sender.try_send(work)
    }
}
fn finish_commands(open: &std::sync::Mutex<bool>, commands: Receiver<Work>) -> Vec<Work> {
    let mut accepting = open.lock().unwrap_or_else(|p| p.into_inner());
    *accepting = false;
    commands.try_iter().collect()
}
pub(super) fn cancelled(command: Command) -> Event {
    let message = "session cancelled".to_string();
    match command {
        Command::Preview { approval, .. } => Event::Preview {
            approval,
            result: Err(message),
        },
        Command::Refresh { approval, .. } => Event::Refresh {
            approval,
            result: Err(message),
        },
        Command::Run { serial, .. } | Command::Diff { serial, .. } => Event::Call {
            serial,
            result: Err(message),
        },
        Command::Decide { request, .. } => Event::Granted {
            reply: Reply::new(Some(request.id), "error", Some(message.clone())),
            detail: Some(message),
            output: None,
        },
        _ => unreachable!("the worker receives only Nix-bound requests"),
    }
}

use goblins_runtime::{Auto, Recreate, Recreated, Source};
pub(super) struct Worker {
    pub cancel: Cancel,
    pub failed_scope: std::sync::Arc<std::sync::atomic::AtomicBool>,
    pub scope_failures: Receiver<String>,
    pub commands: WorkSender,
    pub memberships: SyncSender<std::result::Result<goblins_runtime::Fd, String>>,
    pub results: Receiver<Event>,
    /// The running sandbox's endpoints, sent just before `Event::Started`.
    pub endpoints: Receiver<Box<Endpoints>>,
    pub thread: Option<JoinHandle<()>>,
}
/// What the runtime keeps of a started sandbox.
pub(super) struct Endpoints {
    /// The main PTY or log pipe never leaves the runtime implementation.
    pub master: OwnedFd,
    /// The private channel to the sandbox's goblins-init.
    pub control: OwnedFd,
    pub listener: UnixListener,
    /// The helper's pidfd.
    pub exit_watch: OwnedFd,
}
impl Worker {
    pub fn start(
        source: Source,
        directory: PathBuf,
        dimensions: (u16, u16),
        published: Published,
        handoff: Option<mpsc::Receiver<Handoff>>,
    ) -> Self {
        let cancel = Cancel::default();
        let token = cancel.clone();
        let failed_scope = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let failed = failed_scope.clone();
        let (tx, commands) = mpsc::sync_channel(4);
        let tx = WorkSender::new(tx);
        let commands_open = tx.open.clone();
        let (results, rx) = mpsc::sync_channel(16);
        let results = crate::results::Results::new(results, token.clone());
        let (memberships, membership_rx) = mpsc::sync_channel(1);
        let (scope_failures, scope_failure_rx) = mpsc::sync_channel(1);
        let (endpoints, handed) = mpsc::sync_channel(1);
        let thread = thread::spawn(move || {
            let run = || -> Result<Option<i32>> {
                // The private host attachment chooses this launch's manifest.
                // Disk reads and launch adaptation stay off the event pump.
                token.check()?;
                let (master, slave) = unix::pty(dimensions.0, dimensions.1)?;
                let mut recreated: Option<Recreated> = None;
                let mut copied = std::collections::BTreeMap::new();
                let mut session = match source {
                    Source::Member {
                        scope,
                        owner_scope,
                        manifest,
                        configuration,
                        auto,
                        state,
                        history,
                        worktree,
                        arguments,
                    } => {
                        copied = auto;
                        // Wait for what the owner hands over. Stopping ends
                        // the wait, and so does the daemon giving the launch up.
                        let handoff = handoff.ok_or("a member launch has no handoff")?;
                        let handoff = loop {
                            token.check()?;
                            match handoff.recv_timeout(WORKER_TICK) {
                                Ok(handoff) => break handoff,
                                Err(mpsc::RecvTimeoutError::Timeout) => (),
                                Err(mpsc::RecvTimeoutError::Disconnected) => {
                                    return Err("the launch was given up".into());
                                }
                            }
                        };
                        // As a host launch protects them.
                        let protected = protected_paths(history, &state)?;
                        let session = Session::member(
                            Member {
                                handoff,
                                scope,
                                owner_scope: owner_scope.as_deref(),
                                protected_paths: protected,
                                manifest: &manifest,
                                configuration: &configuration,
                                worktree,
                                arguments,
                            },
                            token.clone(),
                            directory,
                        )?;
                        results.send(Event::Mode {
                            server: session.launch.handlers.is_some(),
                        })?;
                        session
                    }
                    Source::Host {
                        configuration,
                        name,
                        scope,
                        dev_shell,
                        workspace,
                        cwd,
                        state,
                        history,
                        recreate,
                        sequence,
                    } => {
                        // A manifest that does not validate fails the launch. One
                        // that cannot be rooted only does not become current:
                        // the launch reports its own errors, such as no Nix daemon.
                        crate::config::validate(&configuration)?;
                        // Before the slow work, rooting the manifest included:
                        // whether older frontends may attach the terminal. A
                        // configuration that cannot be selected fails below.
                        if let Ok(config) = crate::config::read(&configuration)
                            .and_then(|m| crate::config::select(m, &name, scope.as_deref()))
                        {
                            results.send(Event::Mode {
                                server: config.on_attach.is_some(),
                            })?;
                        }
                        let unrooted = match crate::config::lease_manifest(
                            &configuration,
                            &state.join("manifests"),
                            sequence,
                            &token,
                        ) {
                            Ok((lease, catalogue)) => {
                                // The daemon takes the lease over and keeps it
                                // until the launch started, even once a newer
                                // manifest replaces it as its current one. An
                                // event that is refused or never read drops it,
                                // and its root goes with it.
                                results.send(Event::Manifest {
                                    sequence,
                                    lease,
                                    catalogue,
                                })?;
                                None
                            }
                            Err(error) => {
                                token.check()?;
                                Some(error.to_string())
                            }
                        };
                        let manifest = crate::config::read(&configuration)?;
                        let editor = crate::config::editor(&manifest)?;
                        let run_targets: std::sync::Arc<[String]> =
                            crate::config::run_targets(&manifest, &name).into();
                        let worktrees_directory = crate::config::worktrees_directory(&manifest);
                        let config = crate::config::select(manifest, &name, scope.as_deref())?;
                        token.check()?;
                        let mut launch = crate::config::launch(&config)?;
                        launch.editor = editor;
                        launch.cwd = cwd;
                        let mut directory_error = None;
                        match worktrees_directory {
                            Ok(path) => launch.worktrees_directory = path,
                            Err(error) => {
                                directory_error = Some(format!("worktrees.directory: {error}"))
                            }
                        }
                        if launch.initial_packages.len() > 128 {
                            return Err("initial package limit is 128".into());
                        }
                        // The scope registry takes the last entry, the daemon's
                        // state, as its realm.
                        launch
                            .protected_paths
                            .extend(protected_paths(history, &state)?);
                        let mut session = Session::new_in(
                            launch,
                            workspace.as_deref(),
                            token.clone(),
                            directory,
                        )?;
                        token.check()?;
                        if let Some(error) = unrooted {
                            session
                                .event("manifest-not-current", serde_json::json!({"detail":error}));
                        }
                        session.configuration = name;
                        session.run_targets = run_targets;
                        session.worktrees.detail = directory_error;
                        if let Some(reference) = dev_shell {
                            let cwd = dev_shell_cwd(session.launch.cwd.clone())?;
                            let mut evaluator = crate::evaluator::Evaluator::new(
                                &session.directory,
                                &session.launch.bwrap,
                                session.launch.env.get("SSL_CERT_FILE").map(String::as_str),
                                crate::evaluator::Network::Host,
                                // The host chose this flake; launch may use IFD.
                                true,
                            )?;
                            // The mounts are built later, at start: a flake in a
                            // worktree needs the layout, and the checkout's
                            // `.git`, now.
                            session.settle_layout()?;
                            let tree = goblins_runtime::devshell::parse_reference(&reference)
                                .map(|(path, _)| crate::evaluator::source_tree(&path))?;
                            if let Some(layout) = session.layout()
                                && let Some((git, destination)) =
                                    crate::worktrees::evaluator_git(layout, &tree)?
                            {
                                evaluator.bind_pinned(git, destination);
                            }
                            let folder = session.worktrees.folder.clone();
                            session.launch.dev_shell = Some(crate::devshell::prepare(
                                &reference,
                                &cwd,
                                folder.as_deref(),
                                &evaluator,
                                &session.directory,
                                &token,
                            )?);
                        }
                        if let Some(recreate) = recreate {
                            recreated = Some(prepare_recreate(&mut session, recreate)?);
                        }
                        session
                    }
                };
                // Root copied outputs here, while the source still holds the
                // directory whose roots kept them, so they outlive it.
                session.scope_broker = Some(crate::scope_client::Broker {
                    events: results.clone(),
                    scope_failures,
                    memberships: membership_rx,
                });
                let mut auto = Auto::default();
                for (name, output) in copied {
                    token.check()?;
                    match session.root(&output) {
                        Ok(output) => auto.rooted.push((name, output)),
                        Err(error) => auto.skipped.push(goblins_runtime::history::Skipped::new(
                            &format!("auto-approve {name}"),
                            &error.to_string(),
                        )),
                    }
                }
                if let Some(recreated) = &mut recreated {
                    auto.rooted.append(&mut recreated.auto_approve);
                }
                // A configuration that needs what this sandbox implementation
                // cannot do fails before anything starts.
                let support = Support::linux();
                if let Some(feature) = support.lacks(&session.policy(), session.docker_at_start()) {
                    return Err(format!(
                        "this sandbox does not support {}, which the configuration needs",
                        serde_json::to_value(feature)?.as_str().unwrap_or_default()
                    )
                    .into());
                }
                // With attach handlers the agent runs without a terminal: no
                // input, and its output goes to the server log.
                let handlers = session.launch.handlers.clone();
                let started = if handlers.is_some() {
                    drop(slave);
                    session.start(None)
                } else {
                    session.start(Some(slave))
                };
                let master = match started {
                    Ok(()) if handlers.is_some() => {
                        session.input.take();
                        OwnedFd::from(session.output.take().unwrap())
                    }
                    Ok(()) => master,
                    Err(error) => {
                        // Bubblewrap writes setup errors to the payload's
                        // output, which is not handed to the event pump until
                        // Started. Preserve that diagnostic on failure.
                        use std::io::Read;
                        let output = match session.output.take() {
                            Some(output) => OwnedFd::from(output),
                            None => master,
                        };
                        unix::nonblocking(output.as_raw_fd())?;
                        let mut text = Vec::new();
                        let _ = std::fs::File::from(output)
                            .take(4096)
                            .read_to_end(&mut text);
                        return Err(format!("{error}: {}", String::from_utf8_lossy(&text)).into());
                    }
                };
                // Everything it mounts is rooted by now.
                endpoints.try_send(Box::new(Endpoints {
                    master,
                    control: session.control.take().unwrap(),
                    listener: session.listener.take().unwrap(),
                    exit_watch: session.exit_watch()?,
                }))?;
                let inheritance = session.inheritance();
                let started = Event::Started {
                    docker_enabled: session.docker_enabled(),
                    policy: Box::new(inheritance.policy()),
                    support,
                    dev_shell: inheritance.dev_shell_summary(),
                    identity: session.identity.clone().unwrap(),
                    recreated: recreated.map(Box::new),
                    auto: Box::new(auto),
                    worktrees: Box::new(session.worktrees.clone()),
                };
                // What an export hands over, before the daemon can ask.
                if let Ok(mut published) = published.lock() {
                    *published = Some(inheritance);
                }
                results.send(started)?;
                loop {
                    if token.check().is_err() || !session.alive() {
                        break;
                    }
                    match commands.recv_timeout(WORKER_TICK) {
                        Ok((Command::Preview { approval, package }, cancel)) => {
                            let result = crate::catalog::preview(
                                &session.catalog(),
                                &package,
                                &session.directory,
                                &cancel,
                            )
                            .map_err(|e| e.to_string());
                            if results.send(Event::Preview { approval, result }).is_err() {
                                break;
                            }
                        }
                        Ok((Command::Refresh { approval, changes }, cancel)) => {
                            let result = session
                                .prepare_refresh(&changes, &cancel)
                                .map_err(|e| e.to_string());
                            if results.send(Event::Refresh { approval, result }).is_err() {
                                break;
                            }
                        }
                        Ok((Command::Run { serial, app }, cancel)) => {
                            // The program to execute and the generation it came from.
                            let result = session
                                .run_app(&app, &cancel)
                                .map(|(program, generation)| {
                                    serde_json::json!(goblins_protocol::contract::FlakeRunResult {
                                        program,
                                        generation,
                                    })
                                })
                                .map_err(|e| e.to_string());
                            if results.send(Event::Call { serial, result }).is_err() {
                                break;
                            }
                        }
                        Ok((Command::Diff { serial, changes }, cancel)) => {
                            let result = session
                                .diff_refresh(&changes, &cancel)
                                .map_err(|e| e.to_string());
                            if results.send(Event::Call { serial, result }).is_err() {
                                break;
                            }
                        }
                        Ok((
                            Command::Decide {
                                request: req,
                                approved,
                                output,
                            },
                            _,
                        )) => {
                            session.event(
                                "decision",
                                serde_json::json!({"request":req,"approved":approved}),
                            );
                            let result = if approved {
                                if req.kind == "docker" {
                                    session.enable_docker()
                                } else if req.kind == "devshell" {
                                    session.apply_refresh().and_then(|()| {
                                        let inheritance = session.inheritance();
                                        let summary = inheritance
                                            .dev_shell_summary()
                                            .ok_or("the dev shell is gone")?;
                                        // The new generation, whole, for exports
                                        // made from now on.
                                        if let Ok(mut published) = published.lock() {
                                            *published = Some(inheritance);
                                        }
                                        results.send(Event::DevShell { summary })?;
                                        Ok(())
                                    })
                                } else {
                                    session.grant_observed(
                                        &req.package,
                                        output.as_deref(),
                                        |_, _| Ok(()),
                                        |state| {
                                            results.send(Event::Progress {
                                                request: req.id.clone(),
                                                state: state.into(),
                                            })?;
                                            Ok(())
                                        },
                                    )
                                }
                            } else {
                                Ok(())
                            };
                            let output = if approved && result.is_ok() {
                                session.packages.get(&req.package).cloned()
                            } else {
                                None
                            };
                            let (reply, detail) = match result {
                                Ok(()) => (
                                    Reply::new(
                                        Some(req.id),
                                        if approved { "ready" } else { "denied" },
                                        None,
                                    ),
                                    None,
                                ),
                                Err(error) => {
                                    let detail = error.to_string();
                                    session.event("error", serde_json::json!({"detail":detail}));
                                    let category = if req.kind == "devshell" {
                                        format!(
                                            "dev shell refresh failed: {}",
                                            detail.chars().take(400).collect::<String>()
                                        )
                                    } else if req.kind == "docker" {
                                        if detail.starts_with("Docker requires a scope")
                                            || detail.contains("does not enable Docker")
                                        {
                                            detail.clone()
                                        } else {
                                            "Docker activation failed; see host diagnostics".into()
                                        }
                                    } else if detail.starts_with("cannot resolve package") {
                                        format!(
                                            "cannot resolve package '{}' from pinned nixpkgs; see request details in host TUI",
                                            req.package
                                        )
                                    } else if detail.starts_with("could not build package") {
                                        format!(
                                            "could not build package '{}'; see request details in host TUI",
                                            req.package
                                        )
                                    } else if detail.starts_with("could not mount") {
                                        // The sandbox keeps running; say why.
                                        detail.chars().take(400).collect()
                                    } else {
                                        "grant failed; see request details in host TUI".into()
                                    };
                                    (
                                        Reply::new(Some(req.id), "error", Some(category)),
                                        Some(detail),
                                    )
                                }
                            };
                            if results
                                .send(Event::Granted {
                                    reply,
                                    detail,
                                    output,
                                })
                                .is_err()
                            {
                                break;
                            }
                        }
                        // The dispatcher keeps everything else.
                        Ok(_) => (),
                        Err(mpsc::RecvTimeoutError::Timeout) => (),
                        Err(mpsc::RecvTimeoutError::Disconnected) => break,
                    }
                }
                // Cancellation may have come from the controller observing a
                // normal payload exit while this worker was resolving a package.
                session.alive();
                Ok(session.exit_code)
            };
            let outcome = run();
            let queued = finish_commands(&commands_open, commands);
            // run() owned Session. It has now released the sandbox before
            // retained completion/final-event delivery can wait for capacity.
            if results.finish_held().is_err() {
                return;
            }
            for (command, _) in queued {
                if results.final_event(cancelled(command)).is_err() {
                    return;
                }
            }
            if failed.load(std::sync::atomic::Ordering::SeqCst) {
                let _ = results.final_event(Event::Stopped { exit_code: Some(1) });
                return;
            }
            match outcome {
                Ok(exit_code) => {
                    let _ = results.final_event(Event::Stopped { exit_code });
                }
                Err(e) => {
                    if token.check().is_ok() {
                        let _ = results.final_event(Event::Failed(e.to_string()));
                    }
                    let _ = results.final_event(Event::Stopped { exit_code: None });
                }
            }
        });
        Self {
            cancel,
            failed_scope,
            scope_failures: scope_failure_rx,
            commands: tx,
            memberships,
            results: rx,
            endpoints: handed,
            thread: Some(thread),
        }
    }
}
/// A host dev shell launch's working directory. Without one the sandbox sees
/// no host files, so evaluation may see none either.
fn dev_shell_cwd(cwd: Option<std::path::PathBuf>) -> Result<std::path::PathBuf> {
    Ok(cwd.ok_or("a dev shell needs a host working directory containing the flake")?)
}
/// What no goblin may bind: the session history store and the daemon's
/// state, which stays last (the scope registry takes it as its realm).
fn protected_paths(
    history: Option<std::path::PathBuf>,
    state: &std::path::Path,
) -> Result<Vec<std::path::PathBuf>> {
    let mut protected: Vec<_> = history.into_iter().collect();
    protected.push(std::fs::canonicalize(state)?);
    Ok(protected)
}
/// Stage a recreated goblin's record before it starts: Docker, the recorded
/// grants resolved against the current catalog, and its resume arguments.
fn prepare_recreate(session: &mut Session, recreate: Recreate) -> Result<Recreated> {
    // Best effort: what cannot be restored is skipped with a reason.
    let mut skipped = vec![];
    if recreate.docker
        && let Err(reason) = session.attach_docker_at_start()
    {
        skipped.push(goblins_runtime::history::Skipped::new(
            "docker",
            &reason.to_string(),
        ));
    }
    let (mut packages, unresolved) = session.resolve_recorded(&recreate.packages)?;
    skipped.extend(unresolved);
    // Resolved and rooted like grants, but only mounted once requested.
    let (auto_approve, unresolved) = session.resolve_recorded(&recreate.auto_approve)?;
    skipped.extend(unresolved);
    let room = 128usize.saturating_sub(session.launch.initial_packages.len());
    if packages.len() > room {
        for (name, _) in packages.split_off(room) {
            skipped.push(goblins_runtime::history::Skipped::new(
                &name,
                "initial package limit is 128, resumed grants included",
            ));
        }
    }
    session.recreated = packages.clone();
    // The recorded worktree, if this launch's folder still has it. A record
    // without one worked in the main checkout, even if it was launched in a
    // worktree: the launch directory does not make it one again.
    if recreate.worktree.is_none() {
        session.settle_layout()?;
        session.worktrees.worktree = None;
    }
    if let Some(recorded) = &recreate.worktree {
        session.settle_layout()?;
        match session.layout() {
            Some(layout)
                if goblins_runtime::worktrees::is_worktree(&layout.folder, &recorded.name) =>
            {
                session.worktrees.worktree = Some(recorded.name.clone());
            }
            Some(layout) => skipped.push(goblins_runtime::history::Skipped::new(
                "worktree",
                &format!(
                    "{} no longer exists in {}",
                    recorded.name,
                    layout.folder.display()
                ),
            )),
            None => skipped.push(goblins_runtime::history::Skipped::new(
                "worktree",
                session
                    .worktrees
                    .detail
                    .as_deref()
                    .unwrap_or("this launch has no worktree folder"),
            )),
        }
    }
    let fresh = match &session.launch.resume_command {
        None => None,
        Some(template) => {
            match goblins_runtime::config::resume_arguments(template, &recreate.variables) {
                Ok(arguments) => {
                    session.arguments = Some(arguments);
                    None
                }
                Err(reason) => Some(reason),
            }
        }
    };
    session.event(
        "recreate",
        serde_json::json!({"packages":packages,"skipped":skipped,"fresh":fresh}),
    );
    Ok(Recreated {
        packages,
        auto_approve,
        skipped,
        fresh,
    })
}
impl Worker {
    pub fn finished(&self) -> bool {
        self.thread.as_ref().is_none_or(|t| t.is_finished())
    }
}
impl Drop for Worker {
    fn drop(&mut self) {
        self.cancel.cancel();
        // Dropping a runtime means its caller is gone. Disconnect the event
        // receiver before joining, unblocking any final worker output and
        // dropping queued ownership guards instead of deadlocking shutdown.
        let (_, closed) = mpsc::sync_channel(1);
        drop(std::mem::replace(&mut self.results, closed));
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

#[cfg(test)]
mod output_tests {
    use super::*;
    #[test]
    fn closing_work_keeps_every_accepted_request_and_refuses_later_sends() {
        let (sender, receiver) = mpsc::sync_channel(4);
        let sender = WorkSender::new(sender);
        for serial in 0..4 {
            sender
                .try_send((
                    Command::Run {
                        serial,
                        app: "app".into(),
                    },
                    Cancel::default(),
                ))
                .unwrap_or_else(|_| panic!("capacity"));
        }
        let queued = finish_commands(&sender.open, receiver);
        assert_eq!(queued.len(), 4);
        let serials: Vec<_> = queued
            .into_iter()
            .map(|(command, _)| match cancelled(command) {
                Event::Call {
                    serial,
                    result: Err(_),
                } => serial,
                _ => panic!("cancelled response"),
            })
            .collect();
        assert_eq!(serials, [0, 1, 2, 3]);
        assert!(matches!(
            sender.try_send((
                Command::Run {
                    serial: 4,
                    app: "app".into()
                },
                Cancel::default()
            )),
            Err(mpsc::TrySendError::Disconnected((
                Command::Run { serial: 4, .. },
                _
            )))
        ));
    }
    #[test]
    fn the_history_store_and_the_state_are_protected_from_binds() {
        let root = std::fs::canonicalize(goblins_unix::temp_directory().unwrap()).unwrap();
        let history = root.join("xdg-state/goblins/sessions");
        let state = root.join("state");
        std::fs::create_dir_all(&history).unwrap();
        std::fs::create_dir_all(&state).unwrap();
        let protected = protected_paths(Some(history.clone()), &state).unwrap();
        assert_eq!(protected, [history.clone(), state.clone()]);
        // Neither they nor a directory holding them can be bound.
        for source in [&history, &root.join("xdg-state"), &state] {
            assert!(
                crate::binds::validate(source, &protected).is_err(),
                "{source:?}"
            );
        }
        assert!(crate::binds::validate(&root.join("project"), &protected).is_ok());
        std::fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn a_resume_restores_what_still_exists_and_skips_the_rest() {
        use goblins_runtime::worktrees::Worktree;
        let root = std::fs::canonicalize(goblins_unix::temp_directory().unwrap()).unwrap();
        let checkout = root.join("code/api");
        let folder = root.join("trees/api");
        let worktree = folder.join("fix");
        let admin = checkout.join(".git/worktrees/fix");
        std::fs::create_dir_all(&admin).unwrap();
        std::fs::create_dir_all(&worktree).unwrap();
        std::fs::write(
            worktree.join(".git"),
            format!("gitdir: {}\n", admin.display()),
        )
        .unwrap();
        std::fs::write(
            admin.join("gitdir"),
            format!("{}\n", worktree.join(".git").display()),
        )
        .unwrap();
        let session = |cwd: &std::path::Path| {
            let launch: crate::config::Launch = serde_json::from_value(serde_json::json!({"shell":"/unused", "posix_shell":"/unused", "helper":"/unused", "bwrap":"/unused", "flake":"unused", "client_package":"/unused"})).unwrap();
            let mut session = Session::new(launch, None, Cancel::default()).unwrap();
            session.launch.cwd = Some(cwd.to_path_buf());
            session.launch.worktrees_directory = Some(root.join("trees"));
            session.launch.resume_command = Some(vec!["--resume".into(), "{session_id}".into()]);
            session
        };
        let recreate = |worktree: Option<&str>, docker: bool, session_id: Option<&str>| Recreate {
            packages: vec![],
            auto_approve: vec![],
            docker,
            variables: session_id
                .map(|id| ("session_id".to_string(), id.to_string()))
                .into_iter()
                .collect(),
            worktree: worktree.map(|name| Worktree {
                name: name.into(),
                path: folder.join(name),
            }),
        };
        // The worktree it worked in still exists: it works there again, and
        // the agent resumes its conversation.
        let mut restored = session(&checkout);
        let staged =
            prepare_recreate(&mut restored, recreate(Some("fix"), false, Some("c819"))).unwrap();
        assert!(staged.skipped.is_empty() && staged.fresh.is_none());
        assert_eq!(restored.worktrees.worktree.as_deref(), Some("fix"));
        assert_eq!(
            restored.arguments,
            Some(vec!["--resume".into(), "c819".into()])
        );
        // It worked in the main checkout: relaunched in the worktree it was
        // first launched in, it does not work in that worktree again.
        let mut main = session(&worktree);
        prepare_recreate(&mut main, recreate(None, false, Some("c819"))).unwrap();
        assert_eq!(main.worktrees.worktree, None);
        assert_eq!(main.worktrees.checkout.as_deref(), Some(checkout.as_path()));
        // What is gone is skipped with why: a removed worktree, Docker the
        // configuration no longer offers, a conversation it cannot resume.
        let mut gone = session(&checkout);
        let staged = prepare_recreate(&mut gone, recreate(Some("removed"), true, None)).unwrap();
        let skipped: Vec<_> = staged
            .skipped
            .iter()
            .map(|s| (s.name.as_str(), s.reason.as_str()))
            .collect();
        assert_eq!(
            skipped,
            [
                (
                    "docker",
                    "the configuration runs without a scope, so without Docker"
                ),
                (
                    "worktree",
                    format!("removed no longer exists in {}", folder.display()).as_str()
                ),
            ]
        );
        assert_eq!(gone.worktrees.worktree, None);
        assert_eq!(
            staged.fresh.as_deref(),
            Some("variable 'session_id' is not recorded")
        );
        assert_eq!(gone.arguments, None);
        std::fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn a_dev_shell_launch_needs_a_host_working_directory() {
        assert_eq!(
            dev_shell_cwd(None).unwrap_err().to_string(),
            "a dev shell needs a host working directory containing the flake"
        );
        assert_eq!(
            dev_shell_cwd(Some("/work".into())).unwrap(),
            std::path::Path::new("/work")
        );
    }
}
