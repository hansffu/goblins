//! `goblin worktree`: work in a Git worktree of this goblin's repository
//! (docs/specs/WORKTREES.org). Git runs here, in the sandbox, never in the
//! daemon; the daemon records which worktree the goblin works in.
use crate::call;
use serde_json::json;
use std::{path::Path, process::Command};

const NO_FOLDER: &str = "this goblin has no worktree folder: it was not launched at a repository's root or in one of its worktrees, or worktrees are off (its session detail says why)";

pub fn run(name: Option<String>, base: Option<String>, main: bool) -> Result<i32, String> {
    let folder = std::env::var_os("GOBLINS_WORKTREES").ok_or(NO_FOLDER)?;
    let Some(name) = name.filter(|_| !main) else {
        call("sessions.set-worktree", json!({"name":null}))?;
        println!("main checkout");
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
    call("sessions.set-worktree", json!({"name":name}))?;
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
    let output = if exists {
        if base.is_some() {
            return Err(format!(
                "branch '{name}' exists; --base only applies to a new branch"
            ));
        }
        git(&["worktree", "add", &target_text, name])?
    } else {
        let mut args = vec!["worktree", "add", "-b", name, &target_text];
        args.extend(base);
        git(&args)?
    };
    if output.status.success() {
        return Ok(());
    }
    let mut message = String::from_utf8_lossy(&output.stderr).trim().to_string();
    if message.is_empty() {
        message = String::from_utf8_lossy(&output.stdout).trim().to_string();
    }
    Err(format!("git worktree add: {message}"))
}

fn git(args: &[&str]) -> Result<std::process::Output, String> {
    Command::new("git")
        .args(args)
        .output()
        .map_err(|e| format!("cannot run git: {e}"))
}
