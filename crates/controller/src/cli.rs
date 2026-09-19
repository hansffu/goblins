//! One command tree supplies parsing, help and packaged shell completions.
use clap::{CommandFactory, Parser, Subcommand, ValueHint};
use goblins_controller::{Result, config::Manifest};
use std::path::{Path, PathBuf};

#[derive(Parser)]
#[command(
    name = "goblins",
    version,
    about = "Run named sandbox agents with approved package grants"
)]
pub struct Cli {
    /// Runtime manifest (supplied by the Nix package)
    #[arg(long, global = true, value_name = "MANIFEST")]
    pub runtime: Option<PathBuf>,
    /// Private server state directory (default: $XDG_RUNTIME_DIR/goblins)
    #[arg(long, global = true, value_name = "DIRECTORY", value_hint = ValueHint::DirPath)]
    pub state_dir: Option<PathBuf>,
    #[command(subcommand)]
    pub command: Option<Command>,
}

#[derive(Subcommand)]
pub enum Command {
    /// Start, stop or inspect the background server
    Server {
        #[command(subcommand)]
        command: ServerCommand,
    },
    /// Open the approval interface; quitting leaves agents running
    Tui {
        /// Use the line-oriented approval interface
        #[arg(long)]
        plain: bool,
    },
    /// Launch a Nix configuration with an explicit or automatic agent name
    Run {
        /// Reusable Nix configuration key (for example, shell)
        config: String,
        /// Agent name; omitted names are chosen from the goblin name list
        #[arg(long, value_name = "NAME")]
        name: Option<String>,
    },
    /// Print configured goblin names and their manifest as JSON (no server needed)
    Configurations,
    /// Attach to a sandbox by live name or immutable session ID
    Attach {
        session: String,
        /// Require this daemon instance and an exact session ID (for editors)
        #[arg(long)]
        instance: Option<String>,
    },
    /// Detach a sandbox's terminal by live name or immutable session ID
    #[command(visible_alias = "detach")]
    Detatch { id_or_name: String },
    /// List agent names, session IDs, configurations and lifecycle state
    List,
    /// Stop one agent by live name or immutable session ID
    Stop { id_or_name: String },
    /// Kill one sandbox by live name or immutable session ID
    Kill { id_or_name: String },
    /// Call a trusted host JSON-RPC method
    Rpc { method: String, params_json: String },
    /// Print a shell completion script (Nix packages install Bash/Fish/Zsh scripts)
    Completions {
        #[arg(value_enum)]
        shell: clap_complete::Shell,
    },
    #[command(name = "__complete-names", hide = true)]
    CompleteNames {
        #[arg(last = true)]
        words: Vec<String>,
    },
}

#[derive(Subcommand)]
pub enum ServerCommand {
    /// Start the server in the background; repeated starts leave it running
    Start {
        /// Snapshot this workspace separately for each agent
        #[arg(long, value_name = "DIRECTORY", value_hint = ValueHint::DirPath)]
        workspace: Option<PathBuf>,
        /// Run in this terminal instead, with output on stdout/stderr
        #[arg(long)]
        foreground: bool,
    },
    /// Stop the server and all its agents, waiting for cleanup
    Stop,
    /// Show server state and live agent count (exit 1 when stopped)
    Status,
    /// Print captured background server stdout/stderr
    Logs,
}

// Return 1 outside a name argument so Bash/Zsh can retain their generated
// option/path completion. A missing server is a valid, empty candidate list.
pub fn complete_names(default_state: &Path, words: Vec<String>) -> i32 {
    let Some(state) = completion_state(default_state, words) else {
        return 1;
    };
    let Ok(mut client) = goblins_controller::host::Client::connect(&state) else {
        return 0;
    };
    let Ok(records) = client.call("sessions.list", serde_json::json!({})) else {
        return 0;
    };
    if let Some(records) = records.as_array() {
        for record in records {
            if !matches!(
                record["state"].as_str(),
                Some("starting" | "running" | "stopping")
            ) {
                continue;
            }
            if let Some(name) = record["agent_name"].as_str()
                && (1..=32).contains(&name.len())
                && name
                    .bytes()
                    .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
                && name.as_bytes()[0].is_ascii_lowercase()
            {
                println!("{name}");
            }
        }
    }
    0
}

fn completion_state(default_state: &Path, words: Vec<String>) -> Option<PathBuf> {
    // Bash may split --option=value at '='. Restore it without evaluating any
    // shell text, then let the actual CLI parser identify the argument slot.
    let mut normalized = Vec::<String>::new();
    let mut words = words.into_iter();
    while let Some(word) = words.next() {
        if word == "=" && normalized.last().is_some_and(|last| last.starts_with("--")) {
            let value = words.next()?;
            normalized.last_mut()?.push_str(&format!("={value}"));
        } else {
            normalized.push(word);
        }
    }
    const TARGET: &str = "__goblins_completion_target__";
    normalized.push(TARGET.into());
    let cli = Cli::try_parse_from(normalized).ok()?;
    match cli.command? {
        Command::Attach {
            session,
            instance: None,
        } if session == TARGET => (),
        Command::Detatch { id_or_name } | Command::Kill { id_or_name } if id_or_name == TARGET => {}
        _ => return None,
    }
    Some(cli.state_dir.unwrap_or_else(|| default_state.into()))
}

pub fn completions(shell: clap_complete::Shell, runtime: Option<&Path>) -> Result<()> {
    let mut command = Cli::command();
    let configurations: Vec<_> = runtime
        .map(Manifest::read)
        .transpose()?
        .map(|m| m.goblins.into_keys().collect())
        .unwrap_or_default();
    // These are the character set accepted by the Nix configuration API;
    // never interpolate shell expressions from a hand-written manifest.
    if configurations.iter().any(|name: &String| {
        name.is_empty()
            || !name
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"_-".contains(&b))
    }) {
        return Err("completion configuration keys must be simple identifiers".into());
    }
    if !configurations.is_empty() {
        command = command.mut_subcommand("run", |run| {
            run.mut_arg("config", |arg| {
                arg.value_parser(clap::builder::PossibleValuesParser::new(
                    configurations.clone(),
                ))
            })
        });
    }
    let mut generated = Vec::new();
    clap_complete::generate(shell, &mut command, "goblins", &mut generated);
    let mut generated = String::from_utf8(generated)?;
    // Intercept only name positions before the generated function adjusts its
    // word context. Everything else still uses clap's command/option tree.
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
        if !generated.contains("_goblins() {") {
            return Err("unexpected shell completion function".into());
        }
        generated = generated.replacen("_goblins() {", &format!("_goblins() {{{hook}"), 1);
    }
    print!("{generated}");
    if shell == clap_complete::Shell::Fish {
        // clap_complete's static Fish generator currently omits positional
        // values. Add only those candidates, retaining its command/option tree.
        println!(
            r#"
function __fish_goblins_needs_positional
    set -l subcommand $argv[1]
    set -l words (commandline -opc)
    set -e words[1]
    argparse -s (__fish_goblins_global_optspecs) -- $words 2>/dev/null; or return 1
    test "$argv[1]" = "$subcommand"; or return 1
    set -e argv[1]
    argparse (__fish_goblins_global_optspecs) name= -- $argv 2>/dev/null; or return 1
    test (count $argv) -eq 0
end
complete -c goblins -n '__fish_goblins_needs_positional run' -f -a '{}'
complete -c goblins -n '__fish_goblins_needs_positional completions' -f -a 'bash elvish fish powershell zsh'
"#,
            configurations.join(" ")
        );
    }
    if shell == clap_complete::Shell::Fish {
        println!(
            r#"
function __fish_goblins_agent_names
    set -l words (commandline -opc)
    command $words[1] __complete-names -- $words 2>/dev/null
end
complete -c goblins -n '__fish_goblins_using_subcommand attach detatch detach kill' -f -a '(__fish_goblins_agent_names)'
"#
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn completion_uses_cli_argument_positions_and_state_directory() {
        let default = Path::new("/tmp/default");
        for words in [
            vec!["goblins", "attach"],
            vec!["goblins", "detatch"],
            vec!["goblins", "detach"],
            vec!["goblins", "kill"],
        ] {
            assert_eq!(
                completion_state(default, words.into_iter().map(String::from).collect()),
                Some(default.into())
            );
        }
        for words in [
            vec!["goblins", "--state-dir", "/tmp/custom state", "attach"],
            vec!["goblins", "detach", "--state-dir=/tmp/custom state"],
            vec![
                "goblins",
                "--state-dir",
                "=",
                "/tmp/custom state",
                "detatch",
            ],
        ] {
            assert_eq!(
                completion_state(default, words.into_iter().map(String::from).collect()),
                Some("/tmp/custom state".into())
            );
        }
        for words in [
            vec!["goblins"],
            vec!["goblins", "run"],
            vec!["goblins", "help", "attach"],
            vec!["goblins", "attach", "snikk"],
            vec!["goblins", "attach", "--state-dir"],
            vec!["goblins", "attach", "--runtime"],
            vec!["goblins", "attach", "--instance"],
            vec!["goblins", "attach", "--instance", "exact-daemon"],
        ] {
            assert_eq!(
                completion_state(default, words.into_iter().map(String::from).collect()),
                None
            );
        }
    }
    #[test]
    fn terminal_lifecycle_host_commands() {
        assert!(
            matches!(Cli::try_parse_from(["goblins", "attach", "snikk"]).unwrap().command,
            Some(Command::Attach { session, instance: None }) if session == "snikk")
        );
        assert!(
            matches!(Cli::try_parse_from(["goblins", "kill", "snikk"]).unwrap().command,
            Some(Command::Kill { id_or_name }) if id_or_name == "snikk")
        );
        for command in ["detatch", "detach"] {
            assert!(
                matches!(Cli::try_parse_from(["goblins", command, "snikk"]).unwrap().command,
                Some(Command::Detatch { id_or_name }) if id_or_name == "snikk")
            );
        }
        for args in [
            vec!["goblins", "attach"],
            vec!["goblins", "kill"],
            vec!["goblins", "detatch"],
            vec!["goblins", "detach"],
        ] {
            assert!(Cli::try_parse_from(args).is_err());
        }
    }
    #[test]
    fn command_tree_rejects_removed_aliases_and_misplaced_options() {
        Cli::command().debug_assert();
        for args in [
            vec!["goblins", "shell"],
            vec!["goblins", "daemon"],
            vec!["goblins", "serve"],
            vec!["goblins", "list", "--name", "snikk"],
            vec!["goblins", "server", "status", "--workspace", "/tmp"],
            vec!["goblins", "run", "shell", "--name", "a", "--name", "b"],
        ] {
            assert!(Cli::try_parse_from(args).is_err());
        }
        let cli = Cli::try_parse_from([
            "goblins",
            "run",
            "shell",
            "--name",
            "snikk",
            "--state-dir",
            "/tmp/test",
        ])
        .unwrap();
        assert_eq!(cli.state_dir, Some("/tmp/test".into()));
        assert!(
            matches!(cli.command, Some(Command::Run { config, name: Some(name) }) if config == "shell" && name == "snikk")
        );
    }
}
