use serde::Deserialize;

#[derive(Clone, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Declarations {
    pub rw_dirs: Vec<String>,
    pub rw_files: Vec<String>,
    pub ro_dirs: Vec<String>,
    pub ro_files: Vec<String>,
}

use crate::Result;
use std::{
    os::unix::ffi::OsStrExt,
    path::{Component, Path, PathBuf},
};
// Expand only the agent-sandbox path syntax, never a shell expression. Literal env
// values in mkGoblin.env do not pass through this function.
pub fn expand(value: &str, env: impl Fn(&str) -> Option<String>) -> Result<PathBuf> {
    let mut input = value;
    let mut output = String::new();
    if input.starts_with('~') {
        if input != "~" && !input.starts_with("~/") {
            return Err("host bind paths do not support ~user; use $HOME".into());
        }
        output.push_str(&env("HOME").ok_or("HOME is unset")?);
        input = &input[1..];
    }
    while let Some(index) = input.find('$') {
        output.push_str(&input[..index]);
        input = &input[index + 1..];
        let name;
        if input.starts_with('{') {
            let end = input
                .find('}')
                .ok_or("unclosed variable in host bind path")?;
            name = &input[1..end];
            input = &input[end + 1..];
        } else {
            let end = input
                .find(|c: char| !c.is_ascii_alphanumeric() && c != '_')
                .unwrap_or(input.len());
            name = &input[..end];
            input = &input[end..];
        }
        if name.is_empty()
            || !name
                .bytes()
                .enumerate()
                .all(|(i, c)| c.is_ascii_alphabetic() || c == b'_' || i > 0 && c.is_ascii_digit())
        {
            return Err("host bind paths accept $VAR or ${VAR}, not shell expressions".into());
        }
        output.push_str(&env(name).ok_or_else(|| format!("unset host bind variable: {name}"))?);
    }
    output.push_str(input);
    let path = PathBuf::from(output);
    if !path.is_absolute()
        || path.components().any(|c| matches!(c, Component::ParentDir))
        || path.as_os_str().as_bytes().contains(&0)
    {
        return Err(format!("host bind path must be absolute without '..': {value}").into());
    }
    Ok(path.components().collect())
}
fn overlaps(a: &Path, b: &Path) -> bool {
    a.starts_with(b) || b.starts_with(a)
}
pub fn validate(path: &Path, private: &[PathBuf]) -> Result<()> {
    // These trees establish the helper/control/store boundary. A startup grant
    // cannot replace them, expose their host counterparts, or cover an ancestor.
    for protected in [
        "/proc",
        "/sys",
        "/dev",
        "/nix",
        "/run",
        "/bin",
        "/workspace",
    ]
    .into_iter()
    .map(Path::new)
    .chain(private.iter().map(PathBuf::as_path))
    {
        if overlaps(path, protected) {
            return Err(format!(
                "host bind {} overlaps protected path {}",
                path.display(),
                protected.display()
            )
            .into());
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn path_expansion_is_not_shell_evaluation() {
        let env = |name: &str| {
            if name == "HOME" {
                Some("/home/test".into())
            } else {
                None
            }
        };
        for input in [
            "~/.config/fish",
            "$HOME/.config/fish",
            "${HOME}/.config/fish",
        ] {
            assert_eq!(
                expand(input, env).unwrap(),
                Path::new("/home/test/.config/fish")
            );
        }
        for input in [
            "~someone/file",
            "$MISSING/file",
            "$(id)",
            "${HOME:-/tmp}",
            "relative",
            "$HOME/../secret",
        ] {
            assert!(expand(input, env).is_err(), "{input}");
        }
    }
    #[test]
    fn reserved_trees_cannot_be_covered_or_exposed() {
        for input in [
            "/",
            "/nix",
            "/nix/var/nix/daemon-socket",
            "/run/goblins",
            "/proc",
            "/proc/self/root",
            "/workspace/subdir",
        ] {
            let error = validate(Path::new(input), &[]).unwrap_err().to_string();
            assert!(
                error.contains("overlaps protected path"),
                "{input}: {error}"
            );
        }
        // The daemon's state directory is private: itself, what is in it
        // and what holds it.
        let state = PathBuf::from("/home/test/.local/state/goblins");
        for input in [
            "/home/test/.local/state/goblins",
            "/home/test/.local/state/goblins/host.sock",
            "/home/test/.local",
        ] {
            let error = validate(Path::new(input), std::slice::from_ref(&state))
                .unwrap_err()
                .to_string();
            assert!(
                error.contains("overlaps protected path"),
                "{input}: {error}"
            );
        }
        assert!(validate(Path::new("/tmp"), &["/tmp/session-private".into()]).is_err());
        assert!(validate(Path::new("/home/test/.config/fish"), &[]).is_ok());
    }
}
