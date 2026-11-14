//! `goblin worktree`: work in a Git worktree of this goblin's repository
//! (docs/specs/WORKTREES.org). Git runs here, in the sandbox, never in the
//! daemon; the daemon records which worktree the goblin works in.
use crate::call_typed;
use goblins_protocol::contract;
use std::{path::Path, process::Command};

const NO_FOLDER: &str = "this goblin has no worktree folder: it was not launched at a repository's root or in one of its worktrees, or worktrees are off (its session detail says why)";

pub fn run(name: Option<String>, base: Option<String>, main: bool) -> Result<i32, String> {
    let folder = std::env::var_os("GOBLINS_WORKTREES").ok_or(NO_FOLDER)?;
    let Some(name) = name.filter(|_| !main) else {
        // The folder and the checkout are set together.
        let checkout = std::env::var_os("GOBLINS_CHECKOUT").ok_or(NO_FOLDER)?;
        call_typed::<contract::sandbox::SessionsSetWorktree>(contract::SetWorktreeParams {
            name: None,
        })?;
        println!("{}", Path::new(&checkout).display());
        return Ok(0);
    };
    if !valid_name(&name) {
        return Err(format!(
            "'{name}' is not a valid worktree name: use letters, digits, '.', '_' and '-', starting with a letter or digit"
        ));
    }
    let target = Path::new(&folder).join(&name);
    if std::fs::symlink_metadata(&target).is_err() {
        create(&name, &target, base.as_deref())?;
    }
    // The daemon checks that it is a worktree in this goblin's folder.
    call_typed::<contract::sandbox::SessionsSetWorktree>(contract::SetWorktreeParams {
        name: Some(name),
    })?;
    println!("{}", target.display());
    Ok(0)
}

/// A worktree's name is its branch and its directory: the daemon refuses the
/// rest, and so does this, before running Git.
fn valid_name(name: &str) -> bool {
    (1..=100).contains(&name.len())
        && name.as_bytes()[0].is_ascii_alphanumeric()
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'))
}

/// `git worktree add` from the current directory, which is in the repository:
/// on the branch `name` if it exists, otherwise on a new one from `base`.
fn create(name: &str, target: &Path, base: Option<&str>) -> Result<(), String> {
    let target_text = target.to_string_lossy();
    let branch = format!("refs/heads/{name}");
    let exists = git(&["show-ref", "--verify", "--quiet", &branch])?
        .status
        .success();
    let output = git(&add_arguments(name, &target_text, base, exists)?)?;
    if output.status.success() {
        return Ok(());
    }
    let mut message = String::from_utf8_lossy(&output.stderr).trim().to_string();
    if message.is_empty() {
        message = String::from_utf8_lossy(&output.stdout).trim().to_string();
    }
    Err(format!("git worktree add: {message}"))
}

/// `git worktree add` on the existing branch `name`, or on a new one from
/// `base` (HEAD by default); a base for an existing branch is refused.
fn add_arguments<'a>(
    name: &'a str,
    target: &'a str,
    base: Option<&'a str>,
    exists: bool,
) -> Result<Vec<&'a str>, String> {
    if exists {
        if base.is_some() {
            return Err(format!(
                "branch '{name}' exists; --base only applies to a new branch"
            ));
        }
        return Ok(vec!["worktree", "add", target, name]);
    }
    let mut args = vec!["worktree", "add", "-b", name, target];
    args.extend(base);
    Ok(args)
}

fn git(args: &[&str]) -> Result<std::process::Output, String> {
    Command::new("git")
        .args(args)
        .output()
        .map_err(|e| format!("cannot run git: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_are_branch_and_directory_names() {
        for good in ["fix", "fix-2", "v1.2_x", "9lives"] {
            assert!(valid_name(good), "{good}");
        }
        for bad in [
            "",
            "a/b",
            "../x",
            ".git",
            "-x",
            "_x",
            "a b",
            &"a".repeat(101),
        ] {
            assert!(!valid_name(bad), "{bad}");
        }
    }

    #[test]
    fn an_existing_branch_is_checked_out_and_a_new_one_starts_at_its_base() {
        assert_eq!(
            add_arguments("fix", "/w/fix", None, true).unwrap(),
            ["worktree", "add", "/w/fix", "fix"]
        );
        assert_eq!(
            add_arguments("two", "/w/two", Some("main"), false).unwrap(),
            ["worktree", "add", "-b", "two", "/w/two", "main"]
        );
        assert_eq!(
            add_arguments("two", "/w/two", None, false).unwrap(),
            ["worktree", "add", "-b", "two", "/w/two"]
        );
        assert_eq!(
            add_arguments("third", "/w/third", Some("main"), true).unwrap_err(),
            "branch 'third' exists; --base only applies to a new branch"
        );
    }
}
