//! Restricted CLI. Authorization always lives in the daemon, not this parser.
use clap::{CommandFactory, Parser, Subcommand};
use goblins_jsonrpc as rpc;
use serde_json::json;

#[derive(Parser)]
#[command(
    name = "goblin",
    about = "Message goblins in this raid, run raid members and request packages"
)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Command,
}
#[derive(Subcommand)]
pub enum DevshellCommand {
    /// Ask the host to switch this sandbox to the workspace flake's dev shell
    Refresh {
        /// Update these flake inputs first (all inputs when none are named)
        #[arg(long, num_args = 0.., value_name = "INPUT")]
        update: Option<Vec<String>>,
        /// Lock inputs flake.nix declares but flake.lock lacks
        #[arg(long, conflicts_with = "update")]
        lock: bool,
        #[arg(long)]
        reason: Option<String>,
    },
    /// Show what a refresh would change, without asking the host or changing anything
    Diff {
        /// Update these flake inputs first (all inputs when none are named)
        #[arg(long, num_args = 0.., value_name = "INPUT")]
        update: Option<Vec<String>>,
        /// Lock inputs flake.nix declares but flake.lock lacks
        #[arg(long, conflicts_with = "update")]
        lock: bool,
    },
    /// Write this sandbox's trusted flake.lock back into the workspace
    RestoreLock,
}
#[derive(Subcommand)]
pub enum FlakeCommand {
    /// Run an app of this sandbox's dev shell flake, like nix run
    Run {
        /// [FLAKE]#APP, where FLAKE must be the dev shell's flake (default: .#default)
        installable: Option<String>,
        /// Arguments for the app, after --
        #[arg(last = true)]
        args: Vec<String>,
    },
}
#[derive(Subcommand)]
pub enum EditorCommand {
    /// Open files in the host's editor; waits until it is done with all of them
    #[command(
        override_usage = "goblin editor open [--no-wait] [+LINE[:COL]] FILE [[+LINE[:COL]] FILE]..."
    )]
    Open {
        /// [--no-wait] [+LINE[:COL]] FILE...; a position applies to the next file
        #[arg(required = true, trailing_var_arg = true, allow_hyphen_values = true)]
        args: Vec<String>,
    },
}
#[derive(Subcommand)]
pub enum RaidCommand {
    /// Create a raid and join it as its owner
    Create { raid: String },
    /// Leave this goblin's raid; the owner leaving pauses it
    Leave,
    /// List the raid's members, their roles and its owner
    Members {
        #[arg(long)]
        json: bool,
    },
    /// Set or change this goblin's descriptive role (roles cannot be cleared)
    SetRole { role: String },
}
#[derive(Subcommand)]
pub enum VariableCommand {
    /// Record values for resuming this goblin; an empty VALUE removes KEY
    Set {
        #[arg(required = true, value_name = "KEY=VALUE")]
        assignments: Vec<String>,
    },
}
#[derive(Subcommand)]
pub enum Command {
    /// Report this sandbox's current agent activity
    SetState {
        #[arg(value_parser = ["unknown", "idle", "working", "waiting-input", "waiting-approval"])]
        state: String,
    },
    /// Values this goblin records for being resumed (used by agent hooks)
    Variable {
        #[command(subcommand)]
        command: VariableCommand,
    },
    /// Send text contents through the daemon to another agent
    Send {
        recipient: String,
        #[command(flatten)]
        source: MessageSource,
        #[arg(long)]
        key: Option<String>,
    },
    /// Reply to and complete a claimed inbox item
    Reply {
        message_id: String,
        #[arg(long)]
        claim_generation: u64,
        #[command(flatten)]
        source: MessageSource,
        #[arg(long)]
        key: Option<String>,
    },
    /// Fetch or acknowledge this caller's daemon inbox
    Inbox {
        #[command(subcommand)]
        command: InboxCommand,
    },

    /// Request a package through the host approval policy
    RequestPackage {
        package: String,
        #[arg(long)]
        reason: Option<String>,
    },
    /// Request host approval to attach to this scope's shared Docker engine
    EnableDocker {
        #[arg(long)]
        reason: Option<String>,
    },
    /// Run commands on the host, outside this sandbox, after approval
    Host {
        #[command(subcommand)]
        command: HostCommand,
    },
    /// This sandbox's host-evaluated flake dev shell
    Devshell {
        #[command(subcommand)]
        command: DevshellCommand,
    },
    /// Files in the host's editor (also the goblins-editor command)
    Editor {
        #[command(subcommand)]
        command: EditorCommand,
    },
    /// Collaborate with other goblins: find and message members
    Raid {
        #[command(subcommand)]
        command: RaidCommand,
    },
    /// Flake apps from this sandbox's trusted dev shell generation
    Flake {
        #[command(subcommand)]
        command: FlakeCommand,
    },
    /// Run a raid member with a configuration this goblin may run (raid owners only)
    Run {
        config: String,
        #[arg(long)]
        name: Option<String>,
        /// Return once it is starting (the only mode: goblins do not attach)
        #[arg(long, visible_alias = "detached")]
        detatched: bool,
    },
    /// Show the name and identity of this sandbox
    Status,
    /// List the members created in the raid this goblin owns
    List,
    /// Stop a member created in the raid this goblin owns
    Kill { session: String },
    /// Stop a member created in the raid this goblin owns (same as kill)
    Stop { session: String },
    /// Detach this sandbox's terminal
    Detach,
    Completions {
        #[arg(value_enum)]
        shell: clap_complete::Shell,
    },
    #[command(name = "__complete-names", hide = true)]
    Complete {
        #[arg(last = true)]
        words: Vec<String>,
    },
}

fn slot(words: Vec<String>) -> Option<bool> {
    const TARGET: &str = "__goblins_completion_target__";
    let mut words = words;
    if words
        .last()
        .is_some_and(|word| word.starts_with("--") && word.ends_with('='))
    {
        words.last_mut()?.push_str(TARGET);
    } else {
        words.push(TARGET.into());
    }
    let mut normalized = Vec::<String>::new();
    let mut words = words.into_iter();
    while let Some(word) = words.next() {
        if word == "=" && normalized.last().is_some_and(|w| w.starts_with("--")) {
            let value = words.next()?;
            normalized.last_mut()?.push_str(&format!("={value}"));
        } else {
            normalized.push(word);
        }
    }
    match Cli::try_parse_from(normalized).ok()?.command {
        Command::Run { config, .. } if config == TARGET => Some(false),
        Command::Kill { session } | Command::Stop { session } if session == TARGET => Some(true),
        _ => None,
    }
}
pub fn complete(words: Vec<String>) -> Result<i32, String> {
    let Some(names) = slot(words) else {
        return Ok(1);
    };
    let Ok((mut socket, _)) = super::connect() else {
        return Ok(0);
    };
    if !names {
        let configuration = |name: &str| {
            !name.is_empty()
                && name
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b"_-".contains(&b))
        };
        // The configurations this goblin may run.
        let status = rpc::exchange(&mut socket, json!(1), "sessions.status", json!({}))
            .map_err(|e| e.to_string())?;
        status["allowed_run_targets"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|n| n.as_str())
            .filter(|n| configuration(n))
            .for_each(|n| println!("{n}"));
        return Ok(0);
    }
    let records = rpc::exchange(&mut socket, json!(1), "sessions.list", json!({}))
        .map_err(|e| e.to_string())?;
    if let Some(records) = records.as_array() {
        for record in records {
            if matches!(
                record["state"].as_str(),
                Some("starting" | "running" | "stopping")
            ) && let Some(name) = record["agent_name"].as_str()
                && !name.is_empty()
                && name
                    .bytes()
                    .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
            {
                println!("{name}");
            }
        }
    }
    Ok(0)
}
pub fn completions(shell: clap_complete::Shell) {
    let mut bytes = Vec::new();
    clap_complete::generate(shell, &mut Cli::command(), "goblin", &mut bytes);
    let mut script = String::from_utf8(bytes).unwrap();
    let hook = match shell {
        clap_complete::Shell::Bash => Some(
            r#"
    local goblins_names
    if [[ "${COMP_WORDS[COMP_CWORD]}" != -* ]] &&
       goblins_names=$("${COMP_WORDS[0]}" __complete-names -- "${COMP_WORDS[@]:0:COMP_CWORD}" 2>/dev/null); then
        COMPREPLY=( $(compgen -W "$goblins_names" -- "${COMP_WORDS[COMP_CWORD]}") )
        return
    fi
"#,
        ),
        clap_complete::Shell::Zsh => Some(
            r#"
    local goblins_names
    if [[ "${words[CURRENT]}" != -* ]] &&
       goblins_names=$("${words[1]}" __complete-names -- "${words[@]:0:$((CURRENT - 1))}" 2>/dev/null); then
        if [[ -n "$goblins_names" ]]; then
            compadd -- "${(@f)goblins_names}"
        fi
        return 0
    fi
"#,
        ),
        _ => None,
    };
    if let Some(hook) = hook {
        script = script.replacen("_goblin() {", &format!("_goblin() {{{hook}"), 1);
    }
    print!("{script}");
    if shell == clap_complete::Shell::Fish {
        println!(
            r#"
function __fish_goblin_candidates
    set -l words (commandline -opc)
    command $words[1] __complete-names -- $words 2>/dev/null
end
complete -c goblin -f -a '(__fish_goblin_candidates)'
"#
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn restricted_commands_and_completion_positions() {
        Cli::command().debug_assert();
        for args in [
            vec!["goblin", "rpc", "server.stop", "{}"],
            vec!["goblin", "run", "shell", "--runtime", "/tmp/manifest"],
        ] {
            assert!(Cli::try_parse_from(args).is_err());
        }
        assert_eq!(
            slot(["goblin", "run"].map(String::from).to_vec()),
            Some(false)
        );
        assert_eq!(
            slot(
                ["goblin", "run", "shell", "--name"]
                    .map(String::from)
                    .to_vec()
            ),
            None
        );
        assert!(Cli::try_parse_from(["goblin", "run", "shell", "--detatched"]).is_ok());
        assert!(Cli::try_parse_from(["goblin", "attach", "member"]).is_err());
        assert!(Cli::try_parse_from(["goblin", "status"]).is_ok());
        assert!(Cli::try_parse_from(["goblin", "detach"]).is_ok());
        assert!(Cli::try_parse_from(["goblin", "detatch"]).is_err());
        assert_eq!(
            slot(["goblin", "stop"].map(String::from).to_vec()),
            Some(true)
        );
        assert!(Cli::try_parse_from(["goblin", "raid", "invite", "x"]).is_err());
        assert!(Cli::try_parse_from(["goblin", "raid", "set-role"]).is_err());
        assert!(Cli::try_parse_from(["goblin", "run", "shell", "--inherit-raid"]).is_err());
        assert!(Cli::try_parse_from(["goblin", "run", "shell", "--parent", "."]).is_err());
        assert!(Cli::try_parse_from(["goblin", "variable", "set"]).is_err());
        assert!(
            Cli::try_parse_from(["goblin", "variable", "set", "a=1", "name=two words"]).is_ok()
        );
        assert!(matches!(
            Cli::try_parse_from(["goblin", "editor", "open", "--no-wait", "+3:4", "a"])
                .unwrap()
                .command,
            Command::Editor { command: EditorCommand::Open { args } } if args.len() == 3
        ));
    }
}

#[derive(clap::Args)]
pub struct MessageSource {
    /// Literal message text
    #[arg(long, conflicts_with = "file", required_unless_present = "file")]
    pub message: Option<String>,
    /// Sender-local UTF-8 file, or - for stdin (contents are sent)
    #[arg(long, conflicts_with = "message", required_unless_present = "message")]
    pub file: Option<std::path::PathBuf>,
}
#[derive(Subcommand)]
pub enum HostCommand {
    /// Ask the host to run COMMAND; waits for approval and the job unless --detach
    Run {
        /// Why the task needs it, shown to the approver
        #[arg(long)]
        reason: String,
        /// Return once submitted; the outcome arrives as an inbox notice
        #[arg(long)]
        detach: bool,
        /// Directory to run in (default: the current one); must be shared with the host
        #[arg(long)]
        cwd: Option<std::path::PathBuf>,
        /// Add or replace a variable in the host environment
        #[arg(long = "env", value_name = "KEY=VALUE")]
        env: Vec<String>,
        /// Cancel the job after this many seconds
        #[arg(long, value_name = "SECONDS")]
        timeout: Option<u64>,
        #[arg(last = true, required = true, value_name = "COMMAND")]
        command: Vec<String>,
    },
    /// A host command's state, and its exit code once finished
    Status { job: String },
    /// A host command's output so far
    Logs {
        job: String,
        /// Keep printing until the job ends
        #[arg(long)]
        follow: bool,
    },
    /// Wait for a host command and exit like an attached run (124: still running)
    Wait {
        job: String,
        #[arg(long, value_name = "SECONDS", default_value_t = 540)]
        timeout: u64,
    },
    /// Withdraw a pending host command or stop a running one
    Cancel { job: String },
    /// This sandbox's host commands
    List,
}
#[derive(Subcommand)]
pub enum InboxCommand {
    Status,
    Next {
        #[arg(long)]
        key: Option<String>,
    },
    Complete {
        message: String,
        #[arg(long)]
        claim_generation: u64,
        #[arg(long)]
        key: Option<String>,
    },
    Requeue {
        message: String,
        #[arg(long)]
        claim_generation: u64,
        #[arg(long)]
        reason: String,
        #[arg(long)]
        key: Option<String>,
    },
}
impl InboxCommand {
    pub fn request(self) -> std::io::Result<(&'static str, serde_json::Value)> {
        use goblins_protocol::messages::operation_key;
        use serde_json::json;
        Ok(match self {
            Self::Status => ("inbox.status", json!({})),
            Self::Next { key } => ("inbox.next", json!({"key":operation_key(key)?})),
            Self::Complete {
                message,
                claim_generation,
                key,
            } => (
                "inbox.complete",
                json!({"message":message,"claim_generation":claim_generation,"key":operation_key(key)?}),
            ),
            Self::Requeue {
                message,
                claim_generation,
                key,
                reason,
            } => (
                "inbox.requeue",
                json!({"message":message,"claim_generation":claim_generation,"key":operation_key(key)?,"reason":reason}),
            ),
        })
    }
}
