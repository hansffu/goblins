//! Source patches of the dev shell preview, split by file for frontends.

/// One file of a source patch, for frontends.
#[derive(Debug, PartialEq, Eq)]
pub struct PatchFile<'a> {
    pub path: &'a str,
    /// `modified`, `new file` or `deleted`.
    pub status: &'static str,
    /// Hunk headers and lines, without Git's file header lines.
    pub lines: Vec<&'a str>,
}
impl PatchFile<'_> {
    /// Nix files are shown by default; other source files are hidden.
    pub fn nix(&self) -> bool {
        self.path.ends_with(".nix")
    }
}

/// Split a `git diff` patch by file.
pub fn patch_files(patch: &str) -> Vec<PatchFile<'_>> {
    let mut files: Vec<PatchFile> = Vec::new();
    let mut header = false;
    for line in patch.lines() {
        if let Some(rest) = line.strip_prefix("diff --git ") {
            let path = rest
                .rsplit_once(" b/")
                .map(|(_, b)| b)
                .unwrap_or(rest.trim_start_matches("a/"));
            files.push(PatchFile {
                path,
                status: "modified",
                lines: vec![],
            });
            header = true;
        } else if let Some(file) = files.last_mut() {
            if header && line.starts_with("new file") {
                file.status = "new file";
            } else if header && line.starts_with("deleted file") {
                file.status = "deleted";
            } else if line.starts_with("@@") {
                header = false;
                file.lines.push(line);
            } else if !header {
                file.lines.push(line);
            }
        }
    }
    files
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn patches_split_by_file_without_git_headers() {
        let patch = "diff --git a/flake.nix b/flake.nix\nindex 1..2 100644\n--- a/flake.nix\n+++ b/flake.nix\n@@ -1 +1 @@\n-a\n+b\ndiff --git a/src/main.rs b/src/main.rs\nnew file mode 100644\nindex 0..3\n--- /dev/null\n+++ b/src/main.rs\n@@ -0,0 +1 @@\n+fn main() {}\n";
        let files = patch_files(patch);
        assert_eq!(
            files,
            vec![
                PatchFile {
                    path: "flake.nix",
                    status: "modified",
                    lines: vec!["@@ -1 +1 @@", "-a", "+b"]
                },
                PatchFile {
                    path: "src/main.rs",
                    status: "new file",
                    lines: vec!["@@ -0,0 +1 @@", "+fn main() {}"]
                },
            ]
        );
        assert!(files[0].nix() && !files[1].nix());
    }
}
