//! Headless daemon entry point, host API frontends and raw terminal client.
//! The controller library never opens the approval terminal.
use goblins_protocol::contract::Method;
mod attachment;
mod cli;
mod exec;
#[cfg(test)]
mod fake_daemon;
mod plain;
mod records;
mod remote;
mod server;
mod terminal;
mod tui;
use clap::{CommandFactory, Parser};
use cli::{Cli, Command, ServerCommand};
use goblins_controller::{Result, host::Client};
use std::{
    fs,
    path::PathBuf,
    sync::atomic::{AtomicBool, Ordering},
};
static STOP: AtomicBool = AtomicBool::new(false);
static RESIZE: AtomicBool = AtomicBool::new(true);
extern "C" fn interrupted(_: i32) {
    STOP.store(true, Ordering::Relaxed);
}
extern "C" fn resized(_: i32) {
    RESIZE.store(true, Ordering::Relaxed);
}
fn signals() {
    unsafe {
        libc::signal(libc::SIGINT, interrupted as *const () as _);
        libc::signal(libc::SIGTERM, interrupted as *const () as _);
        libc::signal(libc::SIGHUP, interrupted as *const () as _);
        libc::signal(libc::SIGWINCH, resized as *const () as _);
        libc::signal(libc::SIGPIPE, libc::SIG_IGN);
    }
}
fn execute(cli: Cli) -> Result<i32> {
    let Some(command) = cli.command else {
        Cli::command().print_help()?;
        println!();
        return Ok(0);
    };
    let uid = unsafe { libc::getuid() };
    let root = std::env::var_os("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| format!("/run/user/{uid}").into());
    let completion_default_allowed = completion_default_allowed(
        cli.state_dir.is_some(),
        std::env::var_os("GOBLINS_HOST_JOB").is_some(),
    );
    if let Some(refusal) = host_job_refusal(
        cli.state_dir.is_some(),
        std::env::var_os("GOBLINS_HOST_JOB").is_some(),
        &command,
    ) {
        return Err(refusal.into());
    }
    let state = cli.state_dir.unwrap_or_else(|| {
        if root.is_dir() {
            root.join("goblins")
        } else {
            format!("/tmp/goblins-control-{uid}").into()
        }
    });
    match command {
        Command::Scope { command } => match command {
            cli::ScopeCommand::Clear {
                name,
                all: _,
                dry_run,
            } => {
                goblins_controller::maintenance::clear_scope(
                    cli.runtime.as_deref(),
                    name.as_deref(),
                    dry_run,
                    || STOP.load(Ordering::Relaxed),
                )?;
            }
        },
        Command::Gc { dry_run } => {
            goblins_controller::maintenance::gc(cli.runtime.as_deref(), &state, dry_run, || {
                STOP.load(Ordering::Relaxed)
            })?;
        }
        Command::Send {
            recipient,
            source,
            key,
        } => {
            let body =
                goblins_protocol::messages::load_body(source.message, source.file.as_deref())?;
            let key = goblins_protocol::messages::operation_key(key)?;
            return mailbox_command(
                &state,
                goblins_protocol::contract::host::MessagesSend::NAME,
                goblins_protocol::messages::send_request(key, recipient, body)
                    .map_err(|e| e.to_string())?,
            );
        }
        Command::Reply {
            message_id,
            claim_generation,
            source,
            key,
        } => {
            let body =
                goblins_protocol::messages::load_body(source.message, source.file.as_deref())?;
            let key = goblins_protocol::messages::operation_key(key)?;
            return mailbox_command(
                &state,
                goblins_protocol::contract::host::MessagesReply::NAME,
                serde_json::to_value(goblins_protocol::messages::Reply {
                    key,
                    message: message_id,
                    claim_generation,
                    body,
                })
                .map_err(|e| e.to_string())?,
            );
        }
        Command::Inbox { command } => {
            let (method, params) = command.request()?;
            return mailbox_command(&state, method, params);
        }
        Command::Raid { command } => {
            use cli::RaidCommand::*;
            let mut client = Client::connect(&state)?;
            match command {
                Invite { raid, goblins } => println!(
                    "{}",
                    client
                        .call_typed_with_wire::<goblins_protocol::contract::host::RaidsInvite>(
                            goblins_protocol::contract::RaidInviteParams {
                                raid,
                                sessions: goblins
                            }
                        )?
                        .1
                ),
                Destroy { raid, stop_members } => println!(
                    "{}",
                    client
                        .call_typed_with_wire::<goblins_protocol::contract::host::RaidsDestroy>(
                            goblins_protocol::contract::RaidDestroyParams {
                                raid,
                                id: None,
                                generation: None,
                                stop_members
                            }
                        )?
                        .1
                ),
                SetOwner { raid, goblin } => println!(
                    "{}",
                    client
                        .call_typed_with_wire::<goblins_protocol::contract::host::RaidsSetOwner>(
                            goblins_protocol::contract::RaidSetOwnerParams {
                                raid,
                                session: goblin,
                                id: None,
                                generation: None
                            }
                        )?
                        .1
                ),
                Members { raid, json } => {
                    let (display, record) = client
                        .call_typed_with_wire::<goblins_protocol::contract::host::RaidsMembers>(
                            goblins_protocol::contract::RaidSelectorParams { raid },
                        )?;
                    if json {
                        println!("{record}");
                    } else {
                        print!("{}", goblins_protocol::raid_members_typed(&display));
                    }
                }
                SetRole { raid, goblin, role } => println!(
                    "{}",
                    client
                        .call_typed_with_wire::<goblins_protocol::contract::host::RaidsSetRole>(
                            goblins_protocol::contract::RaidSetRoleParams {
                                raid,
                                session: goblin,
                                role
                            }
                        )?
                        .1
                ),
            }
        }
        Command::CommunicationsLog {
            log,
            session,
            follow,
            json,
        } => {
            if let Some(path) = log {
                for event in goblins_controller::messaging::offline_log(&path, session)? {
                    print_event(&event, json)?;
                }
                return Ok(0);
            }
            let mut session = session;
            let mut cursor = 0;
            let mut instance = None;
            loop {
                let mut client = Client::connect(&state)?;
                if instance.as_ref().is_some_and(|id| *id != client.instance) {
                    return Err(
                        "daemon instance changed; choose the intended log explicitly".into(),
                    );
                }
                instance = Some(client.instance.clone());
                let page = client.call_profile::<goblins_protocol::contract::host_param_profiles::CommunicationsCli>(goblins_protocol::contract::CommunicationsCliParams {session,after:cursor})?;
                session = page.session;
                for event in page.events.as_ref().ok_or("invalid communications page")? {
                    print_event(event, json)?;
                }
                cursor = page.cursor.ok_or("invalid communications cursor")?;
                let caught_up = cursor >= page.latest.ok_or("invalid communications sequence")?;
                if STOP.load(Ordering::Relaxed) || (caught_up && !follow) {
                    break;
                }
                if caught_up {
                    std::thread::sleep(std::time::Duration::from_millis(250));
                }
            }
        }
        Command::Server { command } => match command {
            ServerCommand::Start {
                workspace,
                foreground,
                keep_sessions,
            } => {
                // The manifest the wrapper passes is the first current one
                // raid owners run members from.
                let runtime = cli.runtime.as_deref().map(fs::canonicalize).transpose()?;
                if foreground {
                    server::foreground(&state, workspace, keep_sessions, runtime.as_deref())?;
                } else {
                    server::start(
                        &state,
                        workspace.as_deref(),
                        keep_sessions,
                        runtime.as_deref(),
                    )?;
                }
            }
            ServerCommand::Stop => server::stop(&state)?,
            ServerCommand::Status => return server::status(&state),
            ServerCommand::Logs => server::logs(&state)?,
        },
        Command::Tui { plain } => {
            if plain {
                plain::serve(state)?;
            } else {
                tui::serve(state)?;
            }
        }
        Command::List => println!("{}", sessions_list(&mut Client::connect(&state)?)?),
        Command::Stop { id_or_name } | Command::Kill { id_or_name } => println!(
            "{}",
            sessions_stop(&mut Client::connect(&state)?, id_or_name)?
        ),
        Command::Detach { id_or_name } => println!(
            "{}",
            sessions_detach(&mut Client::connect(&state)?, id_or_name)?
        ),
        Command::Host { command } => {
            let mut client = Client::connect(&state)?;
            println!(
                "{}",
                match command {
                    cli::HostCommand::List =>
                        client
                            .call_typed_with_wire::<goblins_protocol::contract::host::HostList>(
                                goblins_protocol::contract::EmptyParams {}
                            )?
                            .1,
                    cli::HostCommand::Cancel { job } =>
                        client
                            .call_typed_with_wire::<goblins_protocol::contract::host::HostCancel>(
                                goblins_protocol::contract::HostJobParams { request: job }
                            )?
                            .1,
                }
            );
        }
        Command::Rpc { stdio: true, .. } => {
            // The relay blocks in reads: let a signal end it, as it ends
            // any filter.
            for signal in [libc::SIGINT, libc::SIGTERM, libc::SIGHUP] {
                unsafe { libc::signal(signal, libc::SIG_DFL) };
            }
            rpc_stdio(&state, std::io::stdin(), std::io::stdout())?;
        }
        Command::Rpc {
            method,
            params_json,
            ..
        } => println!(
            "{}",
            Client::connect(&state)?.call(
                &method.ok_or("rpc requires a method")?,
                serde_json::from_str(&params_json.ok_or("rpc requires parameters")?)?
            )?
        ),
        Command::Run {
            config,
            name,
            scope,
            dev_shell,
            detatched,
            raid,
        } => {
            let runtime = cli
                .runtime
                .ok_or("run requires --runtime (use the Nix-built goblins command)")?;
            let configuration = fs::canonicalize(&runtime)?.display().to_string();
            let manifest = goblins_runtime::config::read(&runtime)?;
            if !manifest.goblins.contains_key(&config) {
                return Err(format!(
                    "unknown configuration '{config}'; available: {}",
                    manifest
                        .goblins
                        .keys()
                        .cloned()
                        .collect::<Vec<_>>()
                        .join(", ")
                )
                .into());
            }
            // Report an unknown or disallowed scope before starting.
            goblins_runtime::config::select(manifest, &config, scope.as_deref(), |name| {
                std::env::var(name).ok()
            })?;
            // Resolve a relative flake path against the caller's directory.
            let dev_shell = dev_shell
                .map(|reference| -> goblins_controller::Result<String> {
                    let (path, attr) = reference
                        .split_once('#')
                        .map_or((reference.as_str(), None), |(p, a)| (p, Some(a)));
                    let path = fs::canonicalize(path)
                        .map_err(|e| format!("dev shell flake {path}: {e}"))?;
                    let reference = match attr {
                        Some(attr) => format!("{}#{attr}", path.display()),
                        None => path.display().to_string(),
                    };
                    goblins_runtime::devshell::parse_reference(&reference)?;
                    Ok(reference)
                })
                .transpose()?;
            return attachment::run(
                state,
                attachment::Start {
                    name: config,
                    configuration,
                    agent_name: name,
                    scope,
                    dev_shell,
                    raid,
                },
                detatched,
            );
        }
        Command::Resume { id, detached } => {
            let runtime = cli
                .runtime
                .ok_or("resume requires --runtime (use the Nix-built goblins command)")?;
            let configuration = fs::canonicalize(&runtime)?.display().to_string();
            return attachment::resume(state, configuration, id, detached);
        }
        Command::History {
            command:
                Some(cli::HistoryCommand::Prune {
                    older_than,
                    before,
                    keep,
                    dry_run,
                }),
            ..
        } => {
            return records::prune(
                &state,
                goblins_controller::history::Prune {
                    older_than,
                    before,
                    keep,
                    dry_run,
                },
            );
        }
        Command::History {
            json,
            no_pager,
            command: None,
        } => return records::list(&state, json, !no_pager),
        Command::Configurations => {
            let runtime = cli
                .runtime
                .ok_or("configurations requires --runtime (use the Nix-built goblins command)")?;
            let manifest = goblins_runtime::config::read(&runtime)?;
            // Configurations with allowNix use Nix rather than a Goblins dev
            // shell, so frontends don't offer one for them.
            let mut nix = Vec::new();
            for (name, config) in &manifest.goblins {
                if goblins_runtime::config::allows_nix(config)? {
                    nix.push(name);
                }
            }
            println!(
                "{}",
                serde_json::json!({
                    "configuration": fs::canonicalize(runtime)?,
                    "names": manifest.goblins.keys().collect::<Vec<_>>(),
                    "descriptions": manifest.goblins.iter().map(|(name, config)| {
                        (name.clone(), config.description.clone())
                    }).collect::<std::collections::BTreeMap<_, _>>(),
                    "nix": nix,
                })
            );
        }
        Command::Exec {
            tty,
            env,
            workdir,
            instance,
            session,
            goblin,
            command,
        } => {
            return exec::run(
                state,
                exec::Options {
                    goblin,
                    command,
                    tty,
                    env,
                    workdir,
                    instance,
                    session,
                },
            );
        }
        Command::Attach { session, instance } => {
            return attachment::attach(state, session, instance);
        }
        Command::Terminal { command } => return terminal::run(state, command),
        Command::Completions { shell } => cli::completions(shell, cli.runtime.as_deref())?,
        Command::CompleteNames { words } => {
            return Ok(cli::complete_names(
                completion_default_allowed.then_some(state.as_path()),
                words,
            ));
        }
    }
    Ok(0)
}
fn main() {
    let cli = Cli::parse();
    // Before anything is set up here: ssh inherits this process as it is.
    let result = match &cli.host {
        Some(host) => Err(remote(host, cli.remote_program.as_deref()).into()),
        None => {
            goblins_unix::raise_descriptor_limit();
            signals();
            execute(cli)
        }
    };
    let code = match result {
        Ok(code) => code,
        Err(e) => {
            eprintln!("goblins: {e}");
            1
        }
    };
    std::process::exit(code);
}

/// `--host`: hand this command line to the other machine. Returns only when
/// that failed.
fn remote(host: &str, program: Option<&str>) -> std::io::Error {
    let args: std::result::Result<Vec<String>, _> =
        std::env::args_os().skip(1).map(|arg| arg.into_string()).collect();
    match args {
        Ok(args) => remote::run(host, program.unwrap_or("goblins"), &args),
        Err(_) => std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "--host needs arguments that are valid UTF-8",
        ),
    }
}

/// `rpc --stdio`: the caller speaks JSON-RPC itself, `initialize` included,
/// over this process's standard input and output.
fn rpc_stdio(
    state: &std::path::Path,
    input: impl std::io::Read + Send + 'static,
    output: impl std::io::Write + Send + 'static,
) -> Result<()> {
    Ok(goblins_controller::host::relay(
        goblins_controller::host::socket(state)?,
        input,
        output,
    )?)
}

fn mailbox_command(
    state: &std::path::Path,
    method: &str,
    params: serde_json::Value,
) -> Result<i32> {
    if method != "inbox.status" {
        goblins_protocol::messages::Mutation::parse(method, params.clone())?;
    }
    let mut client = Client::connect(state)?;
    if !client
        .features
        .iter()
        .any(|f| f == goblins_protocol::messages::FEATURE)
    {
        return Err("daemon does not support agent inboxes".into());
    }
    let key = params["key"].as_str().map(String::from);
    if let Some(key) = &key {
        eprintln!("{}", serde_json::json!({"key":key}));
    }
    let result = client.mailbox_call(method, params);
    Ok(goblins_protocol::messages::print_outcome(
        result,
        key.as_deref(),
    ))
}

/// `goblins list` prints the reply as the daemon sent it, once it reads as
/// session records.
fn sessions_list(client: &mut Client) -> Result<serde_json::Value> {
    Ok(client
        .call_typed_with_wire::<goblins_protocol::contract::host::SessionsList>(
            goblins_protocol::contract::EmptyParams {},
        )?
        .1)
}

/// `goblins stop` and `kill` print the acknowledgement as the daemon sent it.
fn sessions_stop(client: &mut Client, session: String) -> Result<serde_json::Value> {
    Ok(client
        .call_typed_with_wire::<goblins_protocol::contract::host::SessionsStop>(
            goblins_protocol::contract::SessionSelectorParams { session },
        )?
        .1)
}

/// `goblins detach` prints the acknowledgement as the daemon sent it.
fn sessions_detach(client: &mut Client, session: String) -> Result<serde_json::Value> {
    Ok(client
        .call_typed_with_wire::<goblins_protocol::contract::host::SessionsDetach>(
            goblins_protocol::contract::SessionSelectorParams { session },
        )?
        .1)
}

fn print_event(event: &serde_json::Value, json: bool) -> Result<()> {
    if json {
        println!("{event}");
    } else {
        println!(
            "{} {} {} {}",
            event["sequence"],
            event["kind"].as_str().unwrap_or("unknown"),
            plain::escaped_json(&event["actor"])?,
            plain::escaped_json(&event["data"])?
        );
    }
    Ok(())
}

/// A host command (`goblin host run`) works for a goblin of the default
/// server. Tests it runs must name their own server, never reach or stop
/// that one by accident: everything but local help needs `--state-dir`.
fn completion_default_allowed(explicit_state: bool, host_job: bool) -> bool {
    explicit_state || !host_job
}

fn host_job_refusal(state_dir: bool, host_job: bool, command: &Command) -> Option<&'static str> {
    (!state_dir
        && host_job
        && !matches!(
            command,
            Command::Configurations | Command::Completions { .. } | Command::CompleteNames { .. }
        ))
    .then_some("inside a Goblins host command (GOBLINS_HOST_JOB), pass --state-dir: the default server runs the goblin that asked for this command")
}

#[cfg(test)]
mod tests {
    use crate::fake_daemon::{Daemon, fixtures};
    use goblins_controller::host::Client;
    use serde_json::{Value, json};

    #[test]
    fn a_host_command_must_name_its_own_server() {
        use clap::Parser;
        let command = |args: &[&str]| {
            crate::cli::Cli::try_parse_from([&["goblins"], args].concat())
                .unwrap()
                .command
                .unwrap()
        };
        let status = command(&["server", "status"]);
        assert!(
            super::host_job_refusal(false, true, &status)
                .unwrap()
                .contains("pass --state-dir")
        );
        assert!(super::host_job_refusal(true, true, &status).is_none());
        assert!(super::host_job_refusal(false, false, &status).is_none());
        for local in [&["configurations"][..], &["completions", "bash"]] {
            assert!(super::host_job_refusal(false, true, &command(local)).is_none());
        }
    }

    #[test]
    fn rpc_stdio_carries_the_callers_own_session() {
        use goblins_jsonrpc as rpc;
        use std::{io::Write, os::unix::net::UnixStream, time::Duration};
        let daemon = Daemon::start(&[], json!({"answer": 42}));
        let (mut stdin, input) = UnixStream::pair().unwrap();
        let (output, mut stdout) = UnixStream::pair().unwrap();
        stdout
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        let state = daemon.state.clone();
        let relay = std::thread::spawn(move || super::rpc_stdio(&state, input, output).unwrap());
        let mut call = |id: u64, method: &str, params: Value| {
            let request = json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params});
            stdin.write_all(&rpc::encode(&request).unwrap()).unwrap();
            rpc::read(&mut stdout).unwrap()
        };
        let init = call(1, "initialize", json!({"api": 1}));
        assert_eq!(init["result"], fixtures()["host_initialize"]);
        assert_eq!(
            call(2, "sessions.list", json!({}))["result"],
            json!({"answer": 42})
        );
        // The daemon closed after its one call: the relay ends by itself.
        relay.join().unwrap();
        assert_eq!(daemon.call()["method"], "sessions.list");
    }

    #[test]
    fn rpc_stdio_needs_a_running_server() {
        let state = goblins_unix::temp_directory().unwrap();
        let error = super::rpc_stdio(&state, std::io::empty(), std::io::sink()).unwrap_err();
        assert!(error.to_string().contains("goblins server start"));
        std::fs::remove_dir_all(state).unwrap();
    }

    #[test]
    fn rpc_takes_a_call_or_stdio_not_both() {
        use clap::Parser;
        let parse = |args: &[&str]| crate::cli::Cli::try_parse_from([&["goblins"], args].concat());
        assert!(parse(&["rpc", "--stdio"]).is_ok());
        assert!(parse(&["rpc", "sessions.list", "{}"]).is_ok());
        assert!(parse(&["rpc"]).is_err());
        assert!(parse(&["rpc", "sessions.list"]).is_err());
        assert!(parse(&["rpc", "--stdio", "sessions.list", "{}"]).is_err());
    }

    #[test]
    fn host_job_completions_query_only_explicit_servers() {
        use crate::cli::complete_names;
        use std::os::{fd::AsRawFd, unix::net::UnixListener};
        let _no_forks = goblins_unix::no_forks();
        let root = goblins_unix::temp_directory().unwrap();
        let sentinel = UnixListener::bind(root.join("host.sock")).unwrap();
        for (explicit, job, allowed) in [
            (false, true, false),
            (true, true, true),
            (false, false, true),
            (true, false, true),
        ] {
            assert_eq!(super::completion_default_allowed(explicit, job), allowed);
        }
        let words = |args: &[&str]| args.iter().map(|s| s.to_string()).collect();
        let default = super::completion_default_allowed(false, true).then_some(root.as_path());
        assert_eq!(complete_names(default, words(&["goblins", "attach"])), 0);
        assert!(!goblins_unix::readable(sentinel.as_raw_fd(), 0).unwrap());
        for position in 0..3 {
            let daemon = Daemon::start(&[], json!([]));
            let state = daemon.state.to_str().unwrap();
            let joined = format!("--state-dir={state}");
            let (default, args) = match position {
                0 => (None, words(&["goblins", "--state-dir", state, "attach"])),
                1 => (None, words(&["goblins", "attach", &joined])),
                _ => (Some(daemon.state.as_path()), words(&["goblins", "attach"])),
            };
            assert_eq!(complete_names(default, args), 0);
            let call = daemon.call();
            assert_eq!(call["method"], "sessions.list");
            assert_eq!(call["params"], json!({}));
        }
        drop(sentinel);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn list_prints_the_wire_records_and_refuses_malformed_ones() {
        let fixtures = fixtures();
        let mut future = fixtures["host_session"].clone();
        future["state"] = json!("finished");
        future["agent_state"] = json!("future-activity");
        future["terminals"][0]["origin"] = json!("remote");
        let good = json!([fixtures["forward_compatibility"]["host_session"], future]);
        let mut broken = good.clone();
        broken[0]["identity"] = json!([42, 1, 2, 3, 4]);
        let _no_forks = goblins_unix::no_forks();
        for (records, valid) in [(good, true), (broken, false)] {
            let daemon = Daemon::start(&[], records.clone());
            let shown = super::sessions_list(&mut Client::connect(&daemon.state).unwrap());
            let call = daemon.call();
            assert_eq!(
                (&call["method"], &call["params"]),
                (&json!("sessions.list"), &json!({}))
            );
            match shown {
                Ok(shown) if valid => {
                    assert_eq!(
                        serde_json::from_str::<Value>(&shown.to_string()).unwrap(),
                        records
                    )
                }
                Err(_) if !valid => (),
                other => panic!("{valid}: {other:?}"),
            }
        }
    }

    #[test]
    fn stop_and_detach_print_acknowledgements_with_their_extensions() {
        let result = json!({"accepted":true,"future":{"kept":true}});
        let _no_forks = goblins_unix::no_forks();
        for method in ["sessions.stop", "sessions.detach"] {
            let daemon = Daemon::start(&[], result.clone());
            let mut client = Client::connect(&daemon.state).unwrap();
            let shown = if method == "sessions.stop" {
                super::sessions_stop(&mut client, "goblin".into())
            } else {
                super::sessions_detach(&mut client, "goblin".into())
            }
            .unwrap();
            let call = daemon.call();
            assert_eq!(
                (&call["method"], &call["params"]),
                (&json!(method), &json!({"session":"goblin"}))
            );
            assert_eq!(
                serde_json::from_str::<Value>(&shown.to_string()).unwrap(),
                result
            );
        }
    }

    #[test]
    fn terminal_text_is_ascii_json_data() {
        let text = "\x1b[2J\n approve\u{009b}\u{202e}🐟";
        let shown = super::plain::escaped_json(&text).unwrap();
        assert!(shown.is_ascii());
        assert!(!shown.contains('\x1b'));
        assert_eq!(serde_json::from_str::<String>(&shown).unwrap(), text);
    }
}
