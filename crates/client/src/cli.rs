//! Restricted CLI. Authorization always lives in the daemon, not this parser.
use clap::{CommandFactory, Parser, Subcommand};
use goblins_protocol::rpc;
use serde_json::json;

#[derive(Parser)]
#[command(
    name = "goblins",
    about = "Manage descendant sandboxes and request packages"
)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Command,
}
#[derive(Subcommand)]
pub enum Command {
    /// Sandbox integration lifecycle (used by the mounted notifier and hooks)
    Integration {
        #[command(subcommand)]
        command: IntegrationCommand,
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
    /// Launch a child using the same configuration and attach its terminal
    Run {
        config: String,
        #[arg(long)]
        name: Option<String>,
        /// Parent within this subtree; defaults to this sandbox (.)
        #[arg(long)]
        parent: Option<String>,
        /// Start without attaching
        #[arg(long, visible_alias = "detached")]
        detatched: bool,
    },
    /// Attach to a descendant's terminal
    Attach { session: String },
    /// Show the name and identity of this sandbox
    Status,
    /// List only descendants of this sandbox
    List,
    /// Kill a descendant; requires --kill-children if it has live descendants
    Kill {
        session: String,
        #[arg(long)]
        kill_children: bool,
    },
    /// Stop a descendant (same subtree rules as kill)
    Stop {
        session: String,
        #[arg(long)]
        kill_children: bool,
    },
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
        Command::Run {
            parent: Some(parent),
            ..
        } if parent == TARGET => Some(true),
        Command::Kill { session, .. }
        | Command::Stop { session, .. }
        | Command::Attach { session }
            if session == TARGET =>
        {
            Some(true)
        }
        _ => None,
    }
}
pub fn complete(words: Vec<String>) -> Result<i32, String> {
    let parent = words
        .iter()
        .rev()
        .take(2)
        .any(|w| w == "--parent" || w == "--parent=");
    let Some(names) = slot(words) else {
        return Ok(1);
    };
    let Ok((mut socket, init)) = super::connect() else {
        return Ok(0);
    };
    if !names {
        if let Some(name) = init["configuration"].as_str()
            && name
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"_-".contains(&b))
        {
            println!("{name}");
        }
        return Ok(0);
    }
    if parent {
        println!(".");
    }
    let records = rpc::exchange(&mut socket, json!(1), "sessions.list", json!({}))
        .map_err(|e| e.to_string())?;
    if let Some(records) = records.as_array() {
        for record in records {
            if (if parent {
                record["state"] == "running"
            } else {
                matches!(
                    record["state"].as_str(),
                    Some("starting" | "running" | "stopping")
                )
            }) && let Some(path) = record["path"].as_str()
                && !path.is_empty()
                && path
                    .bytes()
                    .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b"-/".contains(&b))
            {
                println!("{path}");
            }
        }
    }
    Ok(0)
}
pub fn completions(shell: clap_complete::Shell) {
    let mut bytes = Vec::new();
    clap_complete::generate(shell, &mut Cli::command(), "goblins", &mut bytes);
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
        script = script.replacen("_goblins() {", &format!("_goblins() {{{hook}"), 1);
    }
    print!("{script}");
    if shell == clap_complete::Shell::Fish {
        println!(
            r#"
function __fish_goblins_subtree_candidates
    set -l words (commandline -opc)
    command $words[1] __complete-names -- $words 2>/dev/null
end
complete -c goblins -f -a '(__fish_goblins_subtree_candidates)'
complete -c goblins -n '__fish_goblins_using_subcommand run' -l parent -r -f -a '(__fish_goblins_subtree_candidates)'
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
            vec!["goblins", "rpc", "server.stop", "{}"],
            vec!["goblins", "run", "shell", "--runtime", "/tmp/manifest"],
        ] {
            assert!(Cli::try_parse_from(args).is_err());
        }
        assert_eq!(
            slot(["goblins", "run"].map(String::from).to_vec()),
            Some(false)
        );
        assert_eq!(
            slot(
                ["goblins", "run", "shell", "--parent"]
                    .map(String::from)
                    .to_vec()
            ),
            Some(true)
        );
        assert_eq!(
            slot(
                ["goblins", "run", "shell", "--name"]
                    .map(String::from)
                    .to_vec()
            ),
            None
        );
        assert!(Cli::try_parse_from(["goblins", "run", "shell", "--detatched"]).is_ok());
        assert!(Cli::try_parse_from(["goblins", "attach", "child"]).is_ok());
        assert!(Cli::try_parse_from(["goblins", "status"]).is_ok());
        assert!(Cli::try_parse_from(["goblins", "detach"]).is_ok());
        assert!(Cli::try_parse_from(["goblins", "detatch"]).is_err());
        assert_eq!(
            slot(["goblins", "attach"].map(String::from).to_vec()),
            Some(true)
        );
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

#[derive(Subcommand)]
pub enum IntegrationCommand {
    Register {
        #[arg(long)]
        driver: String,
    },
    Watch {
        #[arg(long)]
        epoch: u64,
        #[arg(long, default_value = "terminal", value_parser = ["terminal", "notification"])]
        delivery: String,
    },
    Hook {
        #[arg(long)]
        event: String,
        #[arg(long)]
        active: bool,
    },
    Status,
}
