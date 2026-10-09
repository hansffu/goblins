//! `editor.open`: files a sandbox asks the host editor to open. The daemon
//! builds every editor file name itself from these sandbox paths.
use serde::{Deserialize, Serialize};
use std::path::{Component, Path, PathBuf};

pub const FEATURE: &str = "editor-open";
pub const MAX_FILES: usize = 32;
const MAX_PATH: usize = 4096;

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct File {
    pub path: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub line: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub column: Option<u32>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Open {
    pub files: Vec<File>,
    /// Block until the editor is done with every file, like emacsclient.
    pub wait: bool,
}

/// An absolute, normalized path: no `.`, `..` or empty components, no
/// trailing slash and no control characters.
pub fn normalized(path: &str) -> bool {
    path.len() <= MAX_PATH
        && path.starts_with('/')
        && !path.chars().any(char::is_control)
        && (path == "/"
            || path[1..]
                .split('/')
                .all(|part| !part.is_empty() && part != "." && part != ".."))
}

impl Open {
    pub fn validate(&self) -> Result<(), String> {
        if !(1..=MAX_FILES).contains(&self.files.len()) {
            return Err(format!("open 1 to {MAX_FILES} files"));
        }
        for file in &self.files {
            if !normalized(&file.path) {
                return Err(format!(
                    "not an absolute, normalized path: {}",
                    crate::plain_text(&file.path)
                ));
            }
            if file.line == Some(0) || file.column == Some(0) {
                return Err("lines and columns start at 1".into());
            }
            if file.column.is_some() && file.line.is_none() {
                return Err("a column needs a line".into());
            }
        }
        Ok(())
    }
}

/// `path` made absolute against `cwd` and normalized lexically, as an editor
/// would read the name. Symlinks are not resolved.
pub fn absolute(cwd: &Path, path: &Path) -> PathBuf {
    let mut result = PathBuf::from("/");
    for component in cwd.join(path).components() {
        match component {
            Component::Normal(part) => result.push(part),
            Component::ParentDir => {
                result.pop();
            }
            Component::RootDir | Component::CurDir | Component::Prefix(_) => (),
        }
    }
    result
}

/// `goblins editor open` arguments: `[--no-wait] [+LINE[:COL]] FILE ...`, where
/// a position applies to the file after it, as with emacsclient. Returns the
/// files, still relative, and whether to wait.
pub fn parse(args: &[String]) -> Result<(Vec<File>, bool), String> {
    let mut wait = true;
    let mut files = Vec::new();
    let mut position: Option<(u32, Option<u32>)> = None;
    let mut options = true;
    for arg in args {
        if options && arg == "--" {
            options = false;
        } else if options && (arg == "--no-wait" || arg == "-n") {
            wait = false;
        } else if options && let Some(spec) = arg.strip_prefix('+') {
            let number = |s: &str| s.parse::<u32>().ok().filter(|n| *n > 0);
            position = Some(match spec.split_once(':') {
                Some((line, column)) => (
                    number(line).ok_or_else(|| format!("invalid position {arg}"))?,
                    Some(number(column).ok_or_else(|| format!("invalid position {arg}"))?),
                ),
                None => (
                    number(spec).ok_or_else(|| format!("invalid position {arg}"))?,
                    None,
                ),
            });
        } else if options && arg.starts_with('-') && arg != "-" {
            return Err(format!("unknown option {arg}"));
        } else {
            let (line, column) = match position.take() {
                Some((line, column)) => (Some(line), column),
                None => (None, None),
            };
            files.push(File {
                path: arg.clone(),
                line,
                column,
            });
        }
    }
    if position.is_some() {
        return Err("a position must be followed by a file".into());
    }
    if files.is_empty() {
        return Err("usage: goblins editor open [--no-wait] [+LINE[:COL]] FILE...".into());
    }
    Ok((files, wait))
}

/// The TRAMP host for a goblin's tree path: `/` is not allowed in hosts.
pub fn tramp_host(path: &str) -> String {
    path.replace('/', ".")
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn arguments_positions_and_paths() {
        let args = |a: &[&str]| a.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        let (files, wait) = parse(&args(&["+3:4", "a.txt", "-n", "/tmp/b", "+9", "c"])).unwrap();
        assert!(!wait);
        assert_eq!(files[0].line, Some(3));
        assert_eq!(files[0].column, Some(4));
        assert_eq!(files[1].line, None);
        assert_eq!(files[2].line, Some(9));
        assert_eq!(files[2].column, None);
        for bad in [
            &["+0", "a"][..],
            &["+1"],
            &[],
            &["+x", "a"],
            &["--wait", "a"],
        ] {
            assert!(parse(&args(bad)).is_err(), "{bad:?}");
        }
        let (files, _) = parse(&args(&["--", "-dash"])).unwrap();
        assert_eq!(files[0].path, "-dash");
        assert_eq!(
            absolute(Path::new("/work/x"), Path::new("../y/./z")),
            PathBuf::from("/work/y/z")
        );
        assert_eq!(
            absolute(Path::new("/w"), Path::new("/a/../..")),
            PathBuf::from("/")
        );
        for ok in ["/", "/tmp/x", "/a b/c"] {
            assert!(normalized(ok), "{ok}");
        }
        for bad in ["", "x", "/tmp/", "//x", "/a/./b", "/a/../b", "/a\nb"] {
            assert!(!normalized(bad), "{bad}");
        }
        let open = |files: Vec<File>| Open { files, wait: true }.validate();
        let file = |path: &str, line, column| File {
            path: path.into(),
            line,
            column,
        };
        assert!(open(vec![file("/a", Some(1), Some(2))]).is_ok());
        assert!(open(vec![file("/a", None, Some(2))]).is_err());
        assert!(open(vec![]).is_err());
        assert!(open(vec![file("/a", None, None); 33]).is_err());
        assert_eq!(tramp_host("gribble/snaga"), "gribble.snaga");
    }
}
