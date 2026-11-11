//! Restricted CLI. Authorization always lives in the daemon, not this parser.
use clap::{CommandFactory, Parser, Subcommand};
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
pub enum TerminalCommand {
    /// Start a terminal running a program (the sandbox's shell by default); prints its ID
    Create {
        /// Name it, to use instead of its ID: lowercase letters, digits, - and _
        #[arg(long)]
        name: Option<String>,
        /// Working directory (default: this goblin's)
        #[arg(long, value_name = "DIR")]
        cwd: Option<std::path::PathBuf>,
        /// Set an environment variable for the program
        #[arg(long = "env", value_name = "NAME=VALUE")]
        env: Vec<String>,
        /// Size to start with, until the human attaches
        #[arg(long, value_name = "ROWSxCOLS", value_parser = parse_size)]
        size: Option<(u16, u16)>,
        /// Put one notice in this goblin's inbox when the program exits
        #[arg(long)]
        notify: bool,
        /// Program and arguments, after --
        #[arg(last = true)]
        argv: Vec<String>,
    },
    /// List this goblin's terminals, the human's included
    List,
    /// Print what a terminal shows
    Read {
        /// ID or name
        terminal: String,
        /// The screen, or its recent lines with their scrollback
        #[arg(long, value_parser = ["visible", "recent"])]
        source: Option<String>,
        /// Lines of recent output (default 100, at most 5000)
        #[arg(long)]
        lines: Option<usize>,
    },
    /// Type text and keys into a terminal
    Send {
        terminal: String,
        /// Text to type first
        #[arg(long)]
        text: Option<String>,
        /// Keys to press then: enter, tab, esc, up, ctrl+c, f1, … or one character such as g, A or :
        #[arg(long, num_args = 1.., value_name = "KEY")]
        keys: Vec<String>,
    },
    /// Type a command line into a terminal and press enter
    Run { terminal: String, command: String },
    /// Wait for a line to appear, or for the program to exit; exits 1 on a timeout
    #[command(group = clap::ArgGroup::new("condition").required(true))]
    Wait {
        terminal: String,
        /// A line containing this text
        #[arg(long = "match", value_name = "TEXT", group = "condition")]
        text: Option<String>,
        /// A line matching this regular expression
        #[arg(long, value_name = "RE", group = "condition")]
        regex: Option<String>,
        /// The program exiting
        #[arg(long, group = "condition")]
        exit: bool,
        /// Milliseconds to wait (default 30000)
        #[arg(long, value_name = "MS")]
        timeout: Option<u64>,
        /// Lines of recent output to look at (default 100)
        #[arg(long)]
        lines: Option<usize>,
    },
    /// End a terminal's program, if this goblin created it (the human's only they can close)
    Close { terminal: String },
}
fn parse_size(text: &str) -> std::result::Result<(u16, u16), String> {
    let (rows, cols) = text
        .split_once('x')
        .ok_or("a size is ROWSxCOLS, such as 40x120")?;
    Ok((
        rows.parse::<u16>().map_err(|e| e.to_string())?,
        cols.parse::<u16>().map_err(|e| e.to_string())?,
    ))
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
    /// Work in a Git worktree of this goblin's repository, creating it if needed
    ///
    /// Creates $GOBLINS_WORKTREES/NAME on a branch NAME (an existing branch is
    /// checked out; a new one starts at HEAD or --base), makes it this goblin's
    /// worktree and prints its path. Run it inside the repository, then work in
    /// the printed path. --main goes back to the main checkout, which has the
    /// branch it has checked out (such as main or master).
    Worktree {
        #[arg(required_unless_present = "main", conflicts_with = "main")]
        name: Option<String>,
        /// The commit or branch a new branch starts from (default: HEAD)
        #[arg(long, conflicts_with = "main")]
        base: Option<String>,
        /// Clear this goblin's worktree: it works in the main checkout, whose
        /// path is printed
        #[arg(long)]
        main: bool,
    },
    /// Run a raid member with a configuration this goblin may run (raid owners only)
    Run {
        config: String,
        #[arg(long)]
        name: Option<String>,
        /// A launch option of the configuration, NAME=CHOICE (see goblin targets)
        #[arg(short = 'o', long = "option", value_name = "NAME=CHOICE")]
        options: Vec<String>,
        /// Start the member in this worktree of this goblin's worktree folder
        #[arg(long)]
        worktree: Option<String>,
        /// Return once it is starting (the only mode: goblins do not attach)
        #[arg(long, visible_alias = "detached")]
        detatched: bool,
    },
    /// Named terminals in this sandbox, which keep running with nobody attached
    ///
    /// The human sees them in their sidebar and can attach to them. Name a
    /// terminal, read it before typing into it, and wait with an anchored
    /// regular expression instead of polling.
    Terminal {
        #[command(subcommand)]
        command: TerminalCommand,
    },
    /// The configurations this goblin may run, with their launch options
    Targets,
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

/// What a completion position takes.
#[derive(Debug, PartialEq)]
enum Slot {
    /// A configuration this goblin may run.
    Configuration,
    /// A member it may stop.
    Member,
    /// A launch option of this configuration, as NAME=CHOICE; or, once
    /// Bash has split the word at `=`, a choice of the named option.
    Option(String, Option<String>),
}
fn slot(words: Vec<String>) -> Option<Slot> {
    const TARGET: &str = "__goblins_completion_target__";
    let mut words = words;
    // Bash splits `-o NAME=` into `-o`, `NAME` and `=`: complete the choice.
    let mut option = None;
    if let [.., flag, name, equals] = words.as_slice()
        && (flag == "-o" || flag == "--option")
        && equals == "="
    {
        option = Some(name.clone());
        words.truncate(words.len() - 2);
    }
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
        // `--name = value`, and `-o NAME = CHOICE` of an earlier option.
        let option_value = matches!(
            normalized.iter().rev().nth(1).map(String::as_str),
            Some("-o" | "--option")
        );
        if word == "=" && (option_value || normalized.last().is_some_and(|w| w.starts_with("--"))) {
            let value = words.next()?;
            normalized.last_mut()?.push_str(&format!("={value}"));
        } else {
            normalized.push(word);
        }
    }
    match Cli::try_parse_from(normalized).ok()?.command {
        Command::Run { config, .. } if config == TARGET => Some(Slot::Configuration),
        Command::Run {
            config, options, ..
        } if options.last().is_some_and(|o| o == TARGET) => Some(Slot::Option(config, option)),
        Command::Kill { session } | Command::Stop { session } if session == TARGET => {
            Some(Slot::Member)
        }
        _ => None,
    }
}
pub fn complete(words: Vec<String>) -> Result<i32, String> {
    let Some(slot) = slot(words) else {
        return Ok(1);
    };
    let Ok((mut socket, _)) = super::connect() else {
        return Ok(0);
    };
    if let Slot::Option(config, named) = slot {
        // Every NAME=CHOICE of the configuration, or the named option's
        // choices after Bash split at `=`; the shell filters them.
        let targets = goblins_protocol::contract::exchange::<
            goblins_protocol::contract::sandbox::TargetsList,
        >(
            &mut socket,
            json!(1),
            goblins_protocol::contract::EmptyParams {},
        )
        .map_err(|e| e.to_string())?;
        let target = targets.0.iter().find(|t| t.name == config);
        for (option, declared) in target
            .and_then(|t| t.options.as_ref())
            .into_iter()
            .flatten()
        {
            for choice in declared.choices.iter().flatten().map(|(c, _)| c) {
                if !goblins_protocol::launch_option_name(option)
                    || !goblins_protocol::launch_choice(choice)
                {
                    continue;
                }
                match &named {
                    None => println!("{option}={choice}"),
                    Some(name) if name == option => println!("{choice}"),
                    Some(_) => (),
                }
            }
        }
        return Ok(0);
    }
    if slot == Slot::Configuration {
        let configuration = |name: &str| {
            !name.is_empty()
                && name
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b"_-".contains(&b))
        };
        // The configurations this goblin may run.
        let status = goblins_protocol::contract::exchange::<
            goblins_protocol::contract::sandbox::SessionsStatus,
        >(
            &mut socket,
            json!(1),
            goblins_protocol::contract::EmptyParams {},
        )
        .map_err(|e| e.to_string())?;
        status
            .allowed_run_targets
            .as_ref()
            .into_iter()
            .flatten()
            .map(String::as_str)
            .filter(|n| configuration(n))
            .for_each(|n| println!("{n}"));
        return Ok(0);
    }
    let records =
        goblins_protocol::contract::exchange::<goblins_protocol::contract::sandbox::SessionsList>(
            &mut socket,
            json!(1),
            goblins_protocol::contract::EmptyParams {},
        )
        .map_err(|e| e.to_string())?;
    for record in records.0 {
        if matches!(record.state.as_str(), "starting" | "running" | "stopping")
            && !record.agent_name.is_empty()
            && record
                .agent_name
                .bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
        {
            println!("{}", record.agent_name);
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
    local goblins_names goblins_current="${COMP_WORDS[COMP_CWORD]}"
    local goblins_words=("${COMP_WORDS[@]:0:COMP_CWORD}")
    # Bash makes the `=` of `-o NAME=` a word of its own: complete after it.
    if [[ "$goblins_current" == "=" ]]; then
        goblins_words+=("=")
        goblins_current=
    fi
    if [[ "$goblins_current" != -* ]] &&
       goblins_names=$("${COMP_WORDS[0]}" __complete-names -- "${goblins_words[@]}" 2>/dev/null); then
        COMPREPLY=( $(compgen -W "$goblins_names" -- "$goblins_current") )
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
    #[test]
    fn inbox_mutation_requests_preserve_original_method_fields_and_key() {
        use serde_json::json;
        let key = Some("fixed-key".to_owned());
        for (command, method, params) in [
            (
                super::InboxCommand::Next { key: key.clone() },
                "inbox.next",
                json!({"key":"fixed-key"}),
            ),
            (
                super::InboxCommand::Complete {
                    message: "m".into(),
                    claim_generation: 7,
                    key: key.clone(),
                },
                "inbox.complete",
                json!({"key":"fixed-key","message":"m","claim_generation":7}),
            ),
            (
                super::InboxCommand::Requeue {
                    message: "m".into(),
                    claim_generation: 7,
                    key,
                    reason: "later".into(),
                },
                "inbox.requeue",
                json!({"key":"fixed-key","message":"m","claim_generation":7,"reason":"later"}),
            ),
        ] {
            assert_eq!(command.request().unwrap(), (method, params));
        }
    }

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
            Some(Slot::Configuration)
        );
        for args in [
            &["goblin", "run", "claude", "-o"][..],
            &["goblin", "run", "claude", "--option"],
            &["goblin", "run", "claude", "-o", "model=opus", "--option"],
            &["goblin", "run", "--name", "x", "claude", "-o"],
        ] {
            assert_eq!(
                slot(args.iter().map(|a| a.to_string()).collect()),
                Some(Slot::Option("claude".into(), None)),
                "{args:?}"
            );
        }
        // Bash splits an earlier `-o model=opus` too.
        assert_eq!(
            slot(
                ["goblin", "run", "claude", "-o", "model", "=", "opus", "-o"]
                    .map(String::from)
                    .to_vec()
            ),
            Some(Slot::Option("claude".into(), None))
        );
        // Bash's words at `-o model=<TAB>` and `-o model=so<TAB>`.
        assert_eq!(
            slot(
                ["goblin", "run", "claude", "-o", "model", "="]
                    .map(String::from)
                    .to_vec()
            ),
            Some(Slot::Option("claude".into(), Some("model".into())))
        );
        assert_eq!(
            slot(["goblin", "kill"].map(String::from).to_vec()),
            Some(Slot::Member)
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
        // Terminals are the caller's own: no goblin, no attachment.
        for args in [
            vec!["goblin", "terminal", "list", "other"],
            vec!["goblin", "terminal", "attach", "t1"],
            vec!["goblin", "terminal", "detach", "t1"],
            vec!["goblin", "terminal", "wait", "t1"],
            vec!["goblin", "terminal", "create", "--size", "40"],
            vec!["goblin", "terminal", "create", "--session", "x"],
        ] {
            assert!(Cli::try_parse_from(&args).is_err(), "{args:?}");
        }
        assert!(matches!(
            Cli::try_parse_from(["goblin", "terminal", "create", "--name", "app", "--notify",
                                 "--size", "30x100", "--", "gradle", "run"]).unwrap().command,
            Command::Terminal { command: TerminalCommand::Create { name: Some(name), notify: true,
                size: Some((30, 100)), argv, .. } } if name == "app" && argv == ["gradle", "run"]
        ));
        assert!(matches!(
            Cli::try_parse_from(["goblin", "terminal", "send", "app", "--text", "y",
                                 "--keys", "enter", "ctrl+c"]).unwrap().command,
            Command::Terminal { command: TerminalCommand::Send { text: Some(_), keys, .. } }
                if keys == ["enter", "ctrl+c"]
        ));
        assert!(matches!(
            Cli::try_parse_from([
                "goblin",
                "terminal",
                "wait",
                "t1",
                "--regex",
                "^done$",
                "--timeout",
                "5"
            ])
            .unwrap()
            .command,
            Command::Terminal {
                command: TerminalCommand::Wait {
                    regex: Some(_),
                    timeout: Some(5),
                    exit: false,
                    ..
                }
            }
        ));
        assert!(Cli::try_parse_from(["goblin", "terminal", "run", "t1", "ls"]).is_ok());
        assert!(Cli::try_parse_from(["goblin", "terminal", "close", "t1"]).is_ok());
        assert!(Cli::try_parse_from(["goblin", "detach"]).is_ok());
        assert!(Cli::try_parse_from(["goblin", "detatch"]).is_err());
        assert_eq!(
            slot(["goblin", "stop"].map(String::from).to_vec()),
            Some(Slot::Member)
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
        /// A short label the human recognises in lists and approvals, for
        /// example docker-tests (1-32 characters: letters, digits, space, . _ - :)
        #[arg(long)]
        name: String,
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
        use goblins_protocol::contract::{self, Method};
        use goblins_protocol::messages::{Claim, Next, Requeue, operation_key};
        use serde_json::json;
        Ok(match self {
            Self::Status => ("inbox.status", json!({})),
            Self::Next { key } => (
                contract::sandbox::InboxNext::NAME,
                serde_json::to_value(Next {
                    key: operation_key(key)?,
                })?,
            ),
            Self::Complete {
                message,
                claim_generation,
                key,
            } => (
                contract::sandbox::InboxComplete::NAME,
                serde_json::to_value(Claim {
                    message,
                    claim_generation,
                    key: operation_key(key)?,
                })?,
            ),
            Self::Requeue {
                message,
                claim_generation,
                key,
                reason,
            } => (
                contract::sandbox::InboxRequeue::NAME,
                serde_json::to_value(Requeue {
                    message,
                    claim_generation,
                    key: operation_key(key)?,
                    reason,
                })?,
            ),
        })
    }
}
