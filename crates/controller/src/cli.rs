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
    Serve {
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
    /// List agent names, session IDs, configurations and lifecycle state
    List,
    /// Stop one agent by live name or immutable session ID
    Stop { id_or_name: String },
    /// Call a trusted host JSON-RPC method
    Rpc { method: String, params_json: String },
    /// Print a shell completion script (Nix packages install Bash/Fish/Zsh scripts)
    Completions {
        #[arg(value_enum)]
        shell: clap_complete::Shell,
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
    clap_complete::generate(shell, &mut command, "goblins", &mut std::io::stdout());
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
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn command_tree_rejects_removed_aliases_and_misplaced_options() {
        Cli::command().debug_assert();
        for args in [
            vec!["goblins", "shell"],
            vec!["goblins", "daemon"],
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
