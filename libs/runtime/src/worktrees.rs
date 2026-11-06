//! Git worktrees (docs/specs/WORKTREES.org): names, finding a launch's
//! repository layout and the worktree a goblin works in. Nothing here runs
//! Git: it reads the small files Git writes.
use serde::{Deserialize, Serialize};
use std::{
    ffi::CString,
    fs::{self, File},
    io::Read,
    os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd},
    path::{Path, PathBuf},
};

/// The environment variable that holds the folder, in goblins that have one.
pub const ENV: &str = "GOBLINS_WORKTREES";
/// Bytes read from a `.git` file or a `gitdir` record.
const MAX_GIT_FILE: u64 = 4096;

/// A worktree's name: its branch and its directory name.
pub fn valid_name(name: &str) -> bool {
    let bytes = name.as_bytes();
    (1..=100).contains(&bytes.len())
        && bytes[0].is_ascii_alphanumeric()
        && bytes
            .iter()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'))
}

/// The worktree a goblin works in: `Session.worktree`, and what a history
/// record remembers.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Worktree {
    pub name: String,
    pub path: PathBuf,
}

/// What a launch's worktree support came to, reported in `Started`.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Report {
    /// The mounted folder, `GOBLINS_WORKTREES`; None without worktree support.
    pub folder: Option<PathBuf>,
    /// The worktree the goblin starts in: its launch directory is in one, or
    /// a resume restored the recorded one.
    pub worktree: Option<String>,
    /// Why the launch has no worktree support although it was expected to.
    pub detail: Option<String>,
}

/// Where a launch's repository, worktree folder and worktree are.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Layout {
    /// `DIRECTORY/REPO`, canonical.
    pub folder: PathBuf,
    /// The main checkout, canonical.
    pub checkout: PathBuf,
    /// The worktree the launch directory is in, if it is in one.
    pub worktree: Option<String>,
}

/// Look at a launch's working directory, pinned by the descriptor `cwd` (any
/// descriptor of the directory, `O_PATH` included): everything is read through
/// that object, so a pathname a goblin swaps for a symlink while the launch
/// starts cannot make another repository's worktree vouch for this one.
///
/// - the root of a checkout (it contains a `.git` directory) gets
///   `DIRECTORY/REPO`, created if it is missing;
/// - `DIRECTORY/REPO/NAME` or below, when `NAME` is a worktree whose `.git`
///   file and the checkout's `gitdir` record point at each other, gets the
///   folder and that checkout;
/// - anything else is None.
///
/// An Err says why a launch that looked like one gets no support.
pub fn detect(directory: &Path, cwd: &File) -> Result<Option<Layout>, String> {
    let here = path_of(cwd.as_raw_fd()).map_err(|e| format!("the working directory: {e}"))?;
    if open_beneath(cwd.as_raw_fd(), ".git", libc::O_PATH | libc::O_DIRECTORY).is_ok() {
        let repo = here
            .file_name()
            .ok_or("the repository root has no name to use for its worktree folder")?;
        let folder = directory.join(repo);
        fs::create_dir_all(&folder).map_err(|e| {
            format!(
                "cannot create the worktree folder {}: {e}",
                folder.display()
            )
        })?;
        return layout(&folder, &here, None).map(Some);
    }
    let Ok(directory) = fs::canonicalize(directory) else {
        return Ok(None);
    };
    let Ok(below) = here.strip_prefix(&directory) else {
        return Ok(None);
    };
    let names: Vec<_> = below.components().map(|c| c.as_os_str()).collect();
    let [repo, name, rest @ ..] = &names[..] else {
        return Ok(None);
    };
    // The worktree's root: this directory, or its ancestor `rest.len()` levels
    // up, by the objects themselves; the path it has now must be the one named.
    let mut root = cwd.try_clone().map_err(|e| e.to_string())?;
    for _ in rest {
        root = File::from(
            open_beneath_parent(root.as_raw_fd()).map_err(|e| format!("the worktree: {e}"))?,
        );
    }
    let worktree = directory.join(repo).join(name);
    if path_of(root.as_raw_fd()).ok().as_deref() != Some(worktree.as_path()) {
        return Err(format!(
            "{} changed while the goblin was starting",
            worktree.display()
        ));
    }
    let git = match open_beneath(root.as_raw_fd(), ".git", libc::O_RDONLY) {
        Ok(git) => File::from(git),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => {
            return Err(format!(
                "{}: its .git is not a plain file ({error})",
                worktree.display()
            ));
        }
    };
    let name = name
        .to_str()
        .filter(|n| valid_name(n))
        .ok_or_else(|| format!("{} is not a valid worktree name", name.to_string_lossy()))?;
    let checkout = checkout_of(&worktree, git).map_err(|e| {
        format!(
            "{} is not a worktree of a repository: {e}",
            worktree.display()
        )
    })?;
    layout(&directory.join(repo), &checkout, Some(name.to_string())).map(Some)
}

/// The folder and checkout must exist apart from each other, as canonical
/// paths.
fn layout(folder: &Path, checkout: &Path, worktree: Option<String>) -> Result<Layout, String> {
    let folder = fs::canonicalize(folder)
        .map_err(|e| format!("the worktree folder {}: {e}", folder.display()))?;
    if folder.starts_with(checkout) || checkout.starts_with(&folder) {
        return Err(format!(
            "the worktree folder {} and the checkout {} must not contain each other",
            folder.display(),
            checkout.display()
        ));
    }
    Ok(Layout {
        folder,
        checkout: checkout.to_path_buf(),
        worktree,
    })
}

/// The main checkout of the worktree at `worktree` whose `.git` file is `git`,
/// opened beneath the pinned worktree: the file must name
/// `CHECKOUT/.git/worktrees/ID`, and that directory's `gitdir` record must
/// name this very file. A goblin that can write the worktree cannot point the
/// mount at another repository without also writing that repository.
fn checkout_of(worktree: &Path, mut git: File) -> Result<PathBuf, String> {
    let text = read_small(&mut git, Path::new(".git"))?;
    let admin = text
        .lines()
        .next()
        .and_then(|line| line.strip_prefix("gitdir:"))
        .map(|rest| PathBuf::from(rest.trim()))
        .filter(|path| path.is_absolute())
        .ok_or("its .git file does not name an absolute gitdir")?;
    let admin = fs::canonicalize(&admin).map_err(|e| format!("{}: {e}", admin.display()))?;
    let dot_git = admin
        .parent()
        .filter(|parent| parent.file_name().is_some_and(|n| n == "worktrees"))
        .and_then(Path::parent)
        .filter(|dot_git| dot_git.file_name().is_some_and(|n| n == ".git"))
        .ok_or("its gitdir is not CHECKOUT/.git/worktrees/ID")?;
    let checkout = dot_git.parent().ok_or("its gitdir has no checkout")?;
    if !fs::symlink_metadata(dot_git).is_ok_and(|m| m.is_dir()) {
        return Err(format!("{} is not a directory", dot_git.display()));
    }
    let record = admin.join("gitdir");
    let recorded = PathBuf::from(
        read_small(
            &mut File::open(&record).map_err(|e| format!("{}: {e}", record.display()))?,
            &record,
        )?
        .trim(),
    );
    // Compared with the pinned worktree's own path, which no goblin can
    // choose: another repository's record names that repository's worktree.
    if fs::canonicalize(&recorded).ok().as_deref() != Some(worktree.join(".git").as_path()) {
        return Err("the checkout's record of the worktree does not point back to it".into());
    }
    Ok(checkout.to_path_buf())
}

fn read_small(file: &mut File, path: &Path) -> Result<String, String> {
    let mut text = String::new();
    file.take(MAX_GIT_FILE)
        .read_to_string(&mut text)
        .map_err(|e| format!("{}: {e}", path.display()))?;
    Ok(text)
}

const RESOLVE_NO_MAGICLINKS: u64 = 0x02;
const RESOLVE_NO_SYMLINKS: u64 = 0x04;
const RESOLVE_BENEATH: u64 = 0x08;
#[repr(C)]
struct OpenHow {
    flags: u64,
    mode: u64,
    resolve: u64,
}
/// Open `relative` beneath the directory `directory` without following any
/// symlink.
pub fn open_beneath(directory: RawFd, relative: &str, flags: i32) -> std::io::Result<OwnedFd> {
    let name = CString::new(relative)
        .map_err(|_| std::io::Error::from(std::io::ErrorKind::InvalidInput))?;
    let how = OpenHow {
        flags: (flags | libc::O_CLOEXEC) as u64,
        mode: 0,
        resolve: RESOLVE_BENEATH | RESOLVE_NO_SYMLINKS | RESOLVE_NO_MAGICLINKS,
    };
    // SAFETY: a plain syscall with a valid NUL-terminated name and `how`.
    let fd = unsafe {
        libc::syscall(
            libc::SYS_openat2,
            directory,
            name.as_ptr(),
            &how,
            size_of::<OpenHow>(),
        )
    };
    if fd < 0 {
        return Err(std::io::Error::last_os_error());
    }
    // SAFETY: the syscall returned a new descriptor that nothing else owns.
    Ok(unsafe { OwnedFd::from_raw_fd(fd as i32) })
}
/// The directory above the directory `directory`, by the object itself.
fn open_beneath_parent(directory: RawFd) -> std::io::Result<OwnedFd> {
    let name = CString::new("..").unwrap();
    // SAFETY: a plain syscall with a valid NUL-terminated name.
    let fd = unsafe {
        libc::openat(
            directory,
            name.as_ptr(),
            libc::O_PATH | libc::O_DIRECTORY | libc::O_CLOEXEC,
        )
    };
    if fd < 0 {
        return Err(std::io::Error::last_os_error());
    }
    // SAFETY: the call returned a new descriptor that nothing else owns.
    Ok(unsafe { OwnedFd::from_raw_fd(fd) })
}
/// The path an opened object has now.
pub fn path_of(fd: RawFd) -> std::io::Result<PathBuf> {
    fs::read_link(format!("/proc/self/fd/{fd}"))
}

/// Whether `name` is a directory in `folder` that looks like a worktree: a
/// real directory with a `.git` file, not a symlink.
pub fn is_worktree(folder: &Path, name: &str) -> bool {
    valid_name(name)
        && fs::symlink_metadata(folder.join(name)).is_ok_and(|m| m.is_dir())
        && fs::symlink_metadata(folder.join(name).join(".git")).is_ok_and(|m| m.is_file())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::symlink;

    /// A temporary directory, removed when dropped.
    struct Scratch(PathBuf);
    impl Scratch {
        fn new() -> Self {
            Self(fs::canonicalize(goblins_unix::temp_directory().unwrap()).unwrap())
        }
        fn path(&self) -> &Path {
            &self.0
        }
    }
    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn look(directory: &Path, cwd: &Path) -> Result<Option<Layout>, String> {
        detect(directory, &File::open(cwd).unwrap())
    }

    /// A checkout `base/code/api` with a linked worktree `fix` in `base/trees/api`.
    fn repository() -> (Scratch, PathBuf, PathBuf) {
        let base = Scratch::new();
        let root = base.path().to_path_buf();
        let checkout = root.join("code/api");
        fs::create_dir_all(checkout.join(".git")).unwrap();
        let trees = root.join("trees");
        link(&checkout, &trees.join("api/fix"), "fix");
        (base, checkout, trees)
    }
    fn link(checkout: &Path, path: &Path, id: &str) {
        let admin = checkout.join(".git/worktrees").join(id);
        fs::create_dir_all(&admin).unwrap();
        fs::create_dir_all(path).unwrap();
        fs::write(path.join(".git"), format!("gitdir: {}\n", admin.display())).unwrap();
        fs::write(
            admin.join("gitdir"),
            format!("{}\n", path.join(".git").display()),
        )
        .unwrap();
    }

    #[test]
    fn names_are_branch_and_directory_names() {
        for good in ["a", "fix-login", "v1.2_x", &"a".repeat(100)] {
            assert!(valid_name(good), "{good}");
        }
        for bad in ["", ".hidden", "-x", "a/b", "a b", "..", &"a".repeat(101)] {
            assert!(!valid_name(bad), "{bad}");
        }
    }

    #[test]
    fn a_checkouts_root_gets_its_folder_and_creates_it() {
        let (_base, checkout, trees) = repository();
        fs::remove_dir_all(trees.join("api")).unwrap();
        let found = look(&trees, &checkout).unwrap().unwrap();
        assert_eq!(found.folder, trees.join("api"));
        assert_eq!((found.checkout, found.worktree), (checkout, None));
        assert!(found.folder.is_dir());
    }

    #[test]
    fn a_worktree_or_a_directory_below_it_gets_the_checkout() {
        let (_base, checkout, trees) = repository();
        fs::create_dir_all(trees.join("api/fix/src")).unwrap();
        for cwd in [trees.join("api/fix"), trees.join("api/fix/src")] {
            let found = look(&trees, &cwd).unwrap().unwrap();
            assert_eq!(found.folder, trees.join("api"));
            assert_eq!(found.checkout, checkout);
            assert_eq!(found.worktree.as_deref(), Some("fix"));
        }
    }

    #[test]
    fn anything_else_changes_nothing() {
        let (base, checkout, trees) = repository();
        // Below a checkout's root, the folder and the directory themselves,
        // a plain directory in the folder and unrelated places.
        fs::create_dir_all(checkout.join("src")).unwrap();
        fs::create_dir_all(trees.join("api/plain")).unwrap();
        for cwd in [
            checkout.join("src"),
            trees.clone(),
            trees.join("api"),
            trees.join("api/plain"),
            base.path().to_path_buf(),
        ] {
            assert_eq!(look(&trees, &cwd), Ok(None), "{}", cwd.display());
        }
        // A directory that does not exist yet turns nothing on.
        assert_eq!(
            look(&base.path().join("absent"), &trees.join("api/fix")),
            Ok(None)
        );
    }

    #[test]
    fn a_forged_git_file_is_refused() {
        let (base, checkout, trees) = repository();
        // A second repository the goblin must not be handed.
        let other = base.path().join("code/secret");
        fs::create_dir_all(other.join(".git/worktrees/x")).unwrap();
        fs::write(other.join(".git/worktrees/x/gitdir"), "/nowhere/.git\n").unwrap();
        let forged = trees.join("api/fix/.git");
        fs::write(
            &forged,
            format!("gitdir: {}\n", other.join(".git/worktrees/x").display()),
        )
        .unwrap();
        let error = look(&trees, &trees.join("api/fix")).unwrap_err();
        assert!(error.contains("does not point back"), "{error}");
        // Neither a relative gitdir nor one outside CHECKOUT/.git/worktrees/ID is accepted.
        fs::write(&forged, "gitdir: ../../code/api/.git/worktrees/fix\n").unwrap();
        assert!(
            look(&trees, &trees.join("api/fix"))
                .unwrap_err()
                .contains("absolute")
        );
        fs::write(
            &forged,
            format!("gitdir: {}\n", checkout.join(".git").display()),
        )
        .unwrap();
        assert!(
            look(&trees, &trees.join("api/fix"))
                .unwrap_err()
                .contains("worktrees/ID")
        );
    }

    #[test]
    fn a_worktree_that_is_a_symlink_or_has_a_bad_name_is_refused_or_ignored() {
        let (_base, _checkout, trees) = repository();
        symlink(trees.join("api/fix"), trees.join("api/alias")).unwrap();
        // The canonical cwd of a symlinked directory is the real one.
        let real = fs::canonicalize(trees.join("api/alias")).unwrap();
        assert_eq!(
            look(&trees, &real).unwrap().unwrap().worktree.as_deref(),
            Some("fix")
        );
        assert!(!is_worktree(&trees.join("api"), "alias"));
        assert!(is_worktree(&trees.join("api"), "fix"));
        assert!(!is_worktree(&trees.join("api"), "../api/fix"));
    }

    #[test]
    fn a_path_swapped_after_the_directory_was_pinned_cannot_vouch_for_another_repository() {
        let (base, _checkout, trees) = repository();
        // Another repository with a genuine worktree of the same name, in its own folder.
        let other = base.path().join("code/b");
        fs::create_dir_all(other.join(".git")).unwrap();
        link(&other, &trees.join("b/fix"), "fix");
        // The goblin pins DIRECTORY/api/fix, then renames it and plants a symlink to b's worktree.
        let pinned = File::open(trees.join("api/fix")).unwrap();
        fs::rename(trees.join("api/fix"), trees.join("api/saved")).unwrap();
        symlink(trees.join("b/fix"), trees.join("api/fix")).unwrap();
        // The pinned object is still the first worktree, and b's back-pointer proves nothing for it.
        let error = detect(&trees, &pinned).unwrap_err();
        assert!(error.contains("does not point back"), "{error}");
        // A pinned directory below the worktree is followed up by the objects, not the names.
        fs::create_dir_all(trees.join("api/saved/src")).unwrap();
        let below = File::open(trees.join("api/saved/src")).unwrap();
        fs::rename(trees.join("api/saved"), trees.join("api/moved")).unwrap();
        assert!(detect(&trees, &below).is_err());
    }

    #[test]
    fn overlapping_layouts_are_refused() {
        let base = Scratch::new();
        let root = base.path().to_path_buf();
        // A directory inside the checkout: the folder would contain the checkout's own files.
        let checkout = root.join("api");
        fs::create_dir_all(checkout.join(".git")).unwrap();
        let error = look(&checkout.join("trees"), &checkout).unwrap_err();
        assert!(error.contains("must not contain each other"), "{error}");
        // A checkout inside the folder: folder = DIRECTORY/api, checkout = DIRECTORY/api.
        let inside = root.join("trees/api");
        fs::create_dir_all(inside.join(".git")).unwrap();
        assert!(
            look(&root.join("trees"), &inside)
                .unwrap_err()
                .contains("must not contain")
        );
    }
}
