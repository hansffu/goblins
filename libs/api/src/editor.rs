//! `editor.open`: files a sandbox asks the host editor to open. The daemon
//! builds every editor file name itself from these sandbox paths.
use std::path::{Component, Path, PathBuf};

pub const FEATURE: &str = "editor-open";
pub const MAX_FILES: usize = 32;
const MAX_PATH: usize = 4096;

pub use crate::contract::{EditorFile as File, EditorOpenParams as Open};

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

/// `goblin editor open` arguments: `[--no-wait] [+LINE[:COL]] FILE ...`, where
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
        return Err("usage: goblin editor open [--no-wait] [+LINE[:COL]] FILE...".into());
    }
    Ok((files, wait))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn generated_editor_requests_preserve_original_nested_decoding() {
        #[derive(serde::Deserialize)]
        #[serde(deny_unknown_fields)]
        struct OldFile {
            path: String,
            #[serde(default)]
            line: Option<u32>,
            #[serde(default)]
            column: Option<u32>,
        }
        #[derive(serde::Deserialize)]
        #[serde(deny_unknown_fields)]
        struct OldOpen {
            files: Vec<OldFile>,
            wait: bool,
        }
        let fixtures: serde_json::Value =
            serde_json::from_str(include_str!("../schema/fixtures.json")).unwrap();
        for case in fixtures["editor_parameter_cases"].as_array().unwrap() {
            let old = serde_json::from_value::<OldOpen>(case["params"].clone());
            let new = crate::contract::decode_params::<crate::contract::sandbox::EditorOpen>(
                case["params"].clone(),
            );
            assert_eq!(old.is_ok(), new.is_ok(), "{case}");
            if let (Ok(old), Ok(new)) = (old, new) {
                assert_eq!(old.wait, new.wait);
                assert_eq!(old.files.len(), new.files.len());
                for (old, new) in old.files.iter().zip(&new.files) {
                    assert_eq!(old.path, new.path);
                    assert_eq!(old.line, new.line);
                    assert_eq!(old.column, new.column);
                }
                let valid = (1..=32).contains(&old.files.len())
                    && old.files.iter().all(|f| {
                        normalized(&f.path)
                            && f.line != Some(0)
                            && f.column != Some(0)
                            && !(f.column.is_some() && f.line.is_none())
                    });
                assert_eq!(new.validate().is_ok(), valid, "{case}");
                assert_eq!(valid, case["valid"].as_bool().unwrap(), "{case}");
            } else {
                assert_eq!(case["valid"], false);
            }
        }
    }
    #[test]
    fn editor_reply_keeps_independent_i64_and_message_fallbacks() {
        use crate::contract::{EditorOpenReply, Response};
        use serde_json::{Value, json};
        for value in [
            Value::Null,
            json!([]),
            json!({}),
            json!({"code":true,"message":7}),
            json!({"code":1.0}),
            json!({"code":i64::MAX,"message":"refused"}),
            json!({"code":4294967296_u64}),
        ] {
            let reply = EditorOpenReply::decode(value.clone()).unwrap();
            assert_eq!(reply.code, value.get("code").and_then(Value::as_i64));
            assert_eq!(
                reply.message.as_deref(),
                value.get("message").and_then(Value::as_str)
            );
            assert_eq!(
                reply.code.map_or(1, |n| n as i32),
                value
                    .get("code")
                    .and_then(Value::as_i64)
                    .map_or(1, |n| n as i32)
            );
        }
    }

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
    }
}
