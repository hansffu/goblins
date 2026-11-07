//! What a launch's repository layout needs from the supervisor
//! (docs/specs/WORKTREES.org).
use crate::Result;
use goblins_runtime::worktrees::Layout;
use std::{
    fs::{File, OpenOptions},
    os::unix::fs::OpenOptionsExt,
    path::{Path, PathBuf},
};

/// A flake in a worktree is a Git flake whose common directory is the main
/// `.git`: the evaluator gets it read-only. Opened without following a
/// symlink and bound by descriptor, so what the evaluator sees is the object
/// opened here. None for a flake outside the layout's worktree folder.
pub(crate) fn evaluator_git(layout: &Layout, tree: &Path) -> Result<Option<(File, PathBuf)>> {
    if !tree.starts_with(&layout.folder) {
        return Ok(None);
    }
    let git = layout.checkout.join(".git");
    let directory = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_PATH | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(&git)?;
    Ok(Some((directory, git)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::fd::AsRawFd;

    #[test]
    fn a_flake_in_the_folder_gets_the_checkouts_git_and_others_do_not() {
        let root = std::fs::canonicalize(goblins_unix::temp_directory().unwrap()).unwrap();
        let (folder, checkout) = (root.join("trees/api"), root.join("code/api"));
        std::fs::create_dir_all(&folder).unwrap();
        std::fs::create_dir_all(checkout.join(".git")).unwrap();
        let layout = Layout {
            folder: folder.clone(),
            checkout: checkout.clone(),
            worktree: Some("fix".into()),
        };
        let (git, destination) = evaluator_git(&layout, &folder.join("fix"))
            .unwrap()
            .unwrap();
        assert_eq!(destination, checkout.join(".git"));
        assert!(git.as_raw_fd() >= 0);
        // A launch at the checkout's root needs it for a flake in a worktree too.
        let root_layout = Layout {
            worktree: None,
            ..layout.clone()
        };
        assert!(
            evaluator_git(&root_layout, &folder.join("fix"))
                .unwrap()
                .is_some()
        );
        // A flake elsewhere, and a checkout whose .git is a symlink, get nothing.
        assert!(evaluator_git(&layout, &checkout).unwrap().is_none());
        std::fs::remove_dir_all(checkout.join(".git")).unwrap();
        std::os::unix::fs::symlink(root.join("elsewhere"), checkout.join(".git")).unwrap();
        assert!(evaluator_git(&layout, &folder.join("fix")).is_err());
        std::fs::remove_dir_all(root).unwrap();
    }
}
