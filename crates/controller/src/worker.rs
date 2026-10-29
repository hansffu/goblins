//! One session worker: blocking host work is kept off the controller event pump.
use crate::{
    Result,
    config::Manifest,
    controller::runtime::{Command, Event},
    process::WORKER_TICK,
    session::{Cancel, Inheritance, Session},
    unix,
};
use goblins_protocol::Reply;
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
/// What a recreated goblin takes from its record.
pub(super) struct Recreate {
    /// Approved package names, in grant order.
    pub packages: Vec<String>,
    /// Auto-approved names, resolved again but not mounted.
    pub auto_approve: Vec<String>,
    pub docker: bool,
    pub variables: std::collections::BTreeMap<String, String>,
}
/// What recreating staged, for the controller's hand-over.
pub(super) struct Recreated {
    pub packages: Vec<(String, PathBuf)>,
    /// Recorded auto-approved names, resolved and rooted.
    pub auto_approve: Vec<(String, PathBuf)>,
    /// What could not be restored, and why.
    pub skipped: Vec<crate::history::Skipped>,
    /// Why the agent starts a fresh conversation despite a resume command.
    pub fresh: Option<String>,
}
/// The auto-approve set a session rooted for itself before it started.
#[derive(Default)]
pub(super) struct Auto {
    pub rooted: Vec<(String, PathBuf)>,
    pub skipped: Vec<crate::history::Skipped>,
}
pub(super) struct Worker {
    pub cancel: Cancel,
    pub commands: SyncSender<Work>,
    pub results: Receiver<Event>,
    /// The running sandbox's endpoints, sent just before `Event::Started`.
    pub endpoints: Receiver<Box<Endpoints>>,
    pub thread: Option<JoinHandle<()>>,
}
/// What the runtime keeps of a started sandbox.
pub(super) struct Endpoints {
    /// The private channel to the sandbox's goblins-init.
    pub control: OwnedFd,
    pub listener: UnixListener,
    /// The helper's pidfd.
    pub exit_watch: OwnedFd,
}
pub(super) enum Source {
    Host {
        configuration: PathBuf,
        name: String,
        scope: Option<String>,
        dev_shell: Option<String>,
        workspace: Option<PathBuf>,
        cwd: Option<PathBuf>,
        state: PathBuf,
        /// The history store, protected like the daemon's state.
        history: Option<PathBuf>,
        recreate: Option<Recreate>,
        /// Order of this acceptance, for the daemon's current manifest.
        sequence: u64,
    },
    /// A goblin a raid owner runs, from the daemon's current manifest.
    Member {
        owner: Inheritance,
        /// Held from acceptance, so the owner stopping cannot take it away.
        scope: Option<std::sync::Arc<crate::scope::Shared>>,
        manifest: std::sync::Arc<crate::config::ManifestLease>,
        configuration: String,
        /// Approvals copied from the owner, by exact output.
        auto: std::collections::BTreeMap<String, PathBuf>,
    },
}
impl Worker {
    pub fn start(source: Source, directory: PathBuf, dimensions: (u16, u16)) -> Self {
        let cancel = Cancel::default();
        let token = cancel.clone();
        let (tx, commands) = mpsc::sync_channel(4);
        let (results, rx) = mpsc::sync_channel(16);
        let (endpoints, handed) = mpsc::sync_channel(1);
        let thread = thread::spawn(move || {
            let run = || -> Result<Option<i32>> {
                // The private host attachment chooses this launch's manifest.
                // Disk reads and launch adaptation stay off the event pump.
                token.check()?;
                let (master, slave) = unix::pty(dimensions.0, dimensions.1)?;
                let mut recreated: Option<Recreated> = None;
                let mut copied = std::collections::BTreeMap::new();
                // Held until the session rooted what it needs: the owner's
                // directory keeps its grants, the lease the manifest.
                let mut held = None;
                let mut held_lease = None;
                let mut session = match source {
                    Source::Member {
                        owner,
                        scope,
                        manifest,
                        configuration,
                        auto,
                    } => {
                        copied = auto;
                        let session = Session::member(
                            owner.clone(),
                            scope,
                            &manifest.path,
                            &configuration,
                            token.clone(),
                            directory,
                        )?;
                        held = Some((owner, manifest));
                        results.try_send(Event::Mode {
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
                        Manifest::validate(&configuration)?;
                        // Before the slow work, rooting the manifest included:
                        // whether older frontends may attach the terminal. A
                        // configuration that cannot be selected fails below.
                        if let Ok(config) = Manifest::read(&configuration)
                            .and_then(|m| m.select(&name, scope.as_deref()))
                        {
                            results.try_send(Event::Mode {
                                server: config.on_attach.is_some(),
                            })?;
                        }
                        let unrooted = match crate::config::ManifestLease::create(
                            &configuration,
                            &state.join("manifests"),
                            sequence,
                            &token,
                        ) {
                            Ok(lease) => {
                                let lease = std::sync::Arc::new(lease);
                                results.try_send(Event::Manifest {
                                    sequence,
                                    lease: lease.clone(),
                                })?;
                                // Kept until it started, even once a newer manifest
                                // replaces it as the daemon's current one.
                                held_lease = Some(lease);
                                None
                            }
                            Err(error) => {
                                token.check()?;
                                Some(error.to_string())
                            }
                        };
                        let manifest = Manifest::read(&configuration)?;
                        let editor = manifest.editor()?;
                        let run_targets: std::sync::Arc<[String]> =
                            manifest.run_targets(&name).into();
                        let config = manifest.select(&name, scope.as_deref())?;
                        token.check()?;
                        let mut launch = config.launch()?;
                        launch.editor = editor;
                        launch.cwd = cwd;
                        if launch.initial_packages.len() > 128 {
                            return Err("initial package limit is 128".into());
                        }
                        // The scope registry takes the last entry, the daemon's
                        // state, as its realm.
                        launch.protected_paths.extend(history);
                        launch.protected_paths.push(std::fs::canonicalize(state)?);
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
                        if let Some(reference) = dev_shell {
                            let cwd = session.launch.cwd.clone().ok_or(
                                "a dev shell needs a host working directory containing the flake",
                            )?;
                            let evaluator = crate::evaluator::Evaluator::new(
                                &session.directory,
                                &session.launch.bwrap,
                                session.launch.env.get("SSL_CERT_FILE").map(String::as_str),
                                crate::evaluator::Network::Host,
                                // The host chose this flake; launch may use IFD.
                                true,
                            )?;
                            session.launch.dev_shell = Some(crate::devshell::prepare(
                                &reference,
                                &cwd,
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
                let mut auto = Auto::default();
                for (name, output) in copied {
                    token.check()?;
                    match session.root(&output) {
                        Ok(output) => auto.rooted.push((name, output)),
                        Err(error) => auto.skipped.push(crate::history::Skipped::new(
                            &format!("auto-approve {name}"),
                            &error.to_string(),
                        )),
                    }
                }
                if let Some(recreated) = &mut recreated {
                    auto.rooted.append(&mut recreated.auto_approve);
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
                drop(held);
                drop(held_lease);
                endpoints.try_send(Box::new(Endpoints {
                    control: session.control.take().unwrap(),
                    listener: session.listener.take().unwrap(),
                    exit_watch: session.exit_watch()?,
                }))?;
                results.try_send(Event::Started {
                    docker_enabled: session.docker_enabled(),
                    scope: session.scope_name(),
                    dev_shell: session
                        .launch
                        .dev_shell
                        .as_ref()
                        .map(|d| d.reference.clone()),
                    dev_identity: session.launch.dev_shell.as_ref().map(|d| d.identity()),
                    initial_packages: session
                        .launch
                        .initial_packages
                        .iter()
                        .filter_map(|p| p.file_name()?.to_str()?.get(33..).map(str::to_string))
                        .collect(),
                    master,
                    handlers: handlers.map(Box::new),
                    identity: session.identity.clone().unwrap(),
                    inheritance: session.inheritance(),
                    recreated: recreated.map(Box::new),
                    auto: Box::new(auto),
                })?;
                loop {
                    if token.check().is_err() || !session.alive() {
                        break;
                    }
                    match commands.recv_timeout(WORKER_TICK) {
                        Ok((Command::Preview { approval, package }, cancel)) => {
                            let result = crate::catalog::preview(
                                &session.launch.flake,
                                &package,
                                &session.directory,
                                &cancel,
                            )
                            .map_err(|e| e.to_string());
                            if results
                                .try_send(Event::Preview { approval, result })
                                .is_err()
                            {
                                break;
                            }
                        }
                        Ok((Command::Refresh { approval, changes }, cancel)) => {
                            let result = session
                                .prepare_refresh(&changes, &cancel)
                                .map_err(|e| e.to_string());
                            if results
                                .try_send(Event::Refresh { approval, result })
                                .is_err()
                            {
                                break;
                            }
                        }
                        Ok((Command::Run { serial, app }, cancel)) => {
                            // The program to execute and the generation it came from.
                            let result = session
                                .run_app(&app, &cancel)
                                .map(|(program, generation)| {
                                    serde_json::json!({"program":program,"generation":generation})
                                })
                                .map_err(|e| e.to_string());
                            if results.try_send(Event::Call { serial, result }).is_err() {
                                break;
                            }
                        }
                        Ok((Command::Diff { serial, changes }, cancel)) => {
                            let result = session
                                .diff_refresh(&changes, &cancel)
                                .map_err(|e| e.to_string());
                            if results.try_send(Event::Call { serial, result }).is_err() {
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
                                        let identity = session
                                            .launch
                                            .dev_shell
                                            .as_ref()
                                            .map(|d| d.identity())
                                            .unwrap_or_default();
                                        results.try_send(Event::DevShell {
                                            identity,
                                            inheritance: session.inheritance(),
                                        })?;
                                        Ok(())
                                    })
                                } else {
                                    session.grant_observed(
                                        &req.package,
                                        output.as_deref(),
                                        |_, _| Ok(()),
                                        |state| {
                                            results.try_send(Event::Progress {
                                                request: req.id.clone(),
                                                state,
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
                                .try_send(Event::Granted {
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
            match run() {
                Ok(exit_code) => {
                    let _ = results.try_send(Event::Stopped { exit_code });
                }
                Err(e) => {
                    if token.check().is_ok() {
                        let _ = results.try_send(Event::Failed(e.to_string()));
                    }
                    let _ = results.try_send(Event::Stopped { exit_code: None });
                }
            }
        });
        Self {
            cancel,
            commands: tx,
            results: rx,
            endpoints: handed,
            thread: Some(thread),
        }
    }
}
/// Stage a recreated goblin's record before it starts: Docker, the recorded
/// grants resolved against the current catalog, and its resume arguments.
fn prepare_recreate(session: &mut Session, recreate: Recreate) -> Result<Recreated> {
    // Best effort: what cannot be restored is skipped with a reason.
    let mut skipped = vec![];
    if recreate.docker
        && let Err(reason) = session.attach_docker_at_start()
    {
        skipped.push(crate::history::Skipped::new("docker", &reason.to_string()));
    }
    let (mut packages, unresolved) = session.resolve_recorded(&recreate.packages)?;
    skipped.extend(unresolved);
    // Resolved and rooted like grants, but only mounted once requested.
    let (auto_approve, unresolved) = session.resolve_recorded(&recreate.auto_approve)?;
    skipped.extend(unresolved);
    let room = 128usize.saturating_sub(session.launch.initial_packages.len());
    if packages.len() > room {
        for (name, _) in packages.split_off(room) {
            skipped.push(crate::history::Skipped::new(
                &name,
                "initial package limit is 128, resumed grants included",
            ));
        }
    }
    session.recreated = packages.clone();
    let fresh = match &session.launch.resume_command {
        None => None,
        Some(template) => match crate::config::resume_arguments(template, &recreate.variables) {
            Ok(arguments) => {
                session.arguments = Some(arguments);
                None
            }
            Err(reason) => Some(reason),
        },
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
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}
