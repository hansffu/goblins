//! `--host`: run this same command on another machine through ssh. Nothing
//! else here knows about the network; only the client's standard streams cross.
use std::{io, os::unix::process::CommandExt, process::Command};

/// Options that belong to this machine and are not sent to the other one.
const LOCAL: [&str; 3] = ["--host", "--remote-program", "--runtime"];

/// One word as sh, bash and fish all read it back. ssh joins its arguments
/// with spaces for the remote login shell, and fish treats a backslash
/// inside single quotes specially, so quotes and backslashes stay outside.
pub fn quote(word: &str) -> String {
    if word.is_empty() {
        return "''".into();
    }
    let mut quoted = String::new();
    let mut open = false;
    for character in word.chars() {
        let bare = matches!(character, '\'' | '\\');
        if bare == open {
            quoted.push('\'');
            open = !open;
        }
        if bare {
            quoted.push('\\');
        }
        quoted.push(character);
    }
    if open {
        quoted.push('\'');
    }
    quoted
}

/// The command line for the other machine: this process's own arguments
/// without the options in `LOCAL` and their values. Everything from the first
/// `--` on belongs to the command being run and passes through untouched.
pub fn forwarded(args: &[String]) -> Vec<String> {
    let mut kept = Vec::new();
    let mut rest = args.iter();
    while let Some(arg) = rest.next() {
        if arg == "--" {
            kept.push(arg.clone());
            kept.extend(rest.cloned());
            break;
        }
        if LOCAL.contains(&arg.as_str()) {
            rest.next();
        } else if !LOCAL
            .iter()
            .any(|option| arg.strip_prefix(option).is_some_and(|value| value.starts_with('=')))
        {
            kept.push(arg.clone());
        }
    }
    kept
}

/// The arguments for ssh. A destination ssh would read as an option is refused.
pub fn command(
    destination: &str,
    program: &str,
    args: &[String],
    terminal: bool,
) -> io::Result<Vec<String>> {
    if destination.is_empty() || destination.starts_with('-') {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("--host needs an ssh destination, not '{destination}'"),
        ));
    }
    let mut command = vec![
        if terminal { "-t" } else { "-T" }.to_string(),
        destination.to_string(),
        quote(program),
    ];
    command.extend(args.iter().map(|arg| quote(arg)));
    Ok(command)
}

/// Replace this process with ssh running `program` on `destination` with this
/// process's own arguments `args`. Returns only when that failed.
pub fn run(destination: &str, program: &str, args: &[String]) -> io::Error {
    let terminal = unsafe { libc::isatty(0) == 1 && libc::isatty(1) == 1 };
    let command = match command(destination, program, &forwarded(args), terminal) {
        Ok(command) => command,
        Err(error) => return error,
    };
    let ssh = std::env::var_os("GOBLINS_SSH").unwrap_or_else(|| "ssh".into());
    let error = Command::new(&ssh).args(command).exec();
    io::Error::new(
        error.kind(),
        format!("cannot run {}: {error}", ssh.to_string_lossy()),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn words(words: &[&str]) -> Vec<String> {
        words.iter().map(|word| word.to_string()).collect()
    }

    #[test]
    fn quote_survives_posix_shells_and_fish() {
        assert_eq!(quote(""), "''");
        assert_eq!(quote("list"), "'list'");
        assert_eq!(quote(r"a'b\c d"), r"'a'\''b'\\'c d'");
        assert_eq!(quote("'"), r"\'");
        assert_eq!(quote(r"\\x"), r"\\\\'x'");
    }

    #[test]
    fn forwarded_drops_the_local_options() {
        assert_eq!(
            forwarded(&words(&[
                "--runtime", "/nix/local.json", "--host", "box", "--state-dir", "/s",
                "--remote-program", "/bin/goblins", "list",
            ])),
            words(&["--state-dir", "/s", "list"])
        );
        assert_eq!(
            forwarded(&words(&[
                "--runtime=/nix/local.json", "--host=box", "--remote-program=/bin/goblins",
                "--state-dir=/s", "list",
            ])),
            words(&["--state-dir=/s", "list"])
        );
        // Not an option of ours just because it starts like one.
        assert_eq!(forwarded(&words(&["--hostile", "x"])), words(&["--hostile", "x"]));
    }

    #[test]
    fn forwarded_leaves_the_command_after_the_separator_alone() {
        assert_eq!(
            forwarded(&words(&[
                "exec", "--host", "box", "grub", "--", "ssh", "--host", "other", "--runtime=x", "--",
            ])),
            words(&["exec", "grub", "--", "ssh", "--host", "other", "--runtime=x", "--"])
        );
    }

    #[test]
    fn command_is_what_ssh_receives() {
        let args = words(&["exec", "grub", "--", "echo", "it's"]);
        assert_eq!(
            command("hans@box", "goblins", &args, true).unwrap(),
            words(&["-t", "hans@box", "'goblins'", "'exec'", "'grub'", "'--'", "'echo'", r"'it'\''s'"])
        );
        assert_eq!(command("box", "/opt/my goblins", &[], false).unwrap(), words(&["-T", "box", "'/opt/my goblins'"]));
    }

    #[test]
    fn command_refuses_a_destination_ssh_would_read_as_an_option() {
        for destination in ["", "-oProxyCommand=evil"] {
            let error = command(destination, "goblins", &[], false).unwrap_err();
            assert!(error.to_string().contains("--host needs an ssh destination"), "{error}");
        }
    }
}
