//! Plain-text rendering of a dev shell refresh preview for `goblins devshell
//! diff`. The daemon sends facts only; wording follows the TUI approval
//! summary. Every string from the preview is escaped before it is printed.
use goblins_protocol::{patch::patch_files, plain_text};
use serde_json::Value;
use std::fmt::Write;

/// How many incoming packages are listed, as in the TUI.
const INCOMING: usize = 50;

fn text_of(v: &Value) -> String {
    plain_text(v.as_str().unwrap_or_default())
}
fn count(v: &Value) -> String {
    v.as_u64().map_or("?".into(), |n| n.to_string())
}
/// Old → new; either may be absent.
fn old_new(old: Option<String>, new: Option<String>) -> String {
    match (old, new) {
        (Some(a), Some(b)) => format!("{a} → {b}"),
        (None, Some(v)) | (Some(v), None) => v,
        (None, None) => String::new(),
    }
}
fn locked(v: &Value) -> Option<String> {
    v.as_object()?;
    Some(format!(
        "{}{}",
        text_of(&v["id"]),
        v["date"]
            .as_str()
            .map(|d| format!(" ({})", plain_text(d)))
            .unwrap_or_default()
    ))
}
fn versions(v: &Value) -> Option<String> {
    Some(
        v.as_array()?
            .iter()
            .map(text_of)
            .map(|s| if s.is_empty() { "?".into() } else { s })
            .collect::<Vec<_>>()
            .join(", "),
    )
}
fn tag(change: &str) -> &'static str {
    match change {
        "added" => "[A] ",
        "removed" => "[R] ",
        _ => "[C] ",
    }
}
fn attention(note: &Value) -> String {
    match note["kind"].as_str().unwrap_or_default() {
        "input-source" => format!(
            "input '{}' now comes from {} (was {})",
            text_of(&note["input"]),
            text_of(&note["new"]),
            text_of(&note["old"])
        ),
        "new-input" => format!(
            "new input '{}' from {}",
            text_of(&note["input"]),
            text_of(&note["source"])
        ),
        "nix-config" => "flake declares nixConfig (ignored, never applied)".into(),
        "local-builds" => format!(
            "{} derivation(s) build on this machine",
            count(&note["count"])
        ),
        "shell-hook" => "shellHook changed; it runs in the sandbox".into(),
        other => plain_text(other),
    }
}

/// Labelled rows: the label on the first row of a section only.
struct Rows(String);
impl Rows {
    fn section(&mut self, label: &str, rows: impl IntoIterator<Item = String>) {
        for (i, row) in rows.into_iter().enumerate() {
            let label = if i == 0 { label } else { "" };
            let _ = writeln!(self.0, "{label:<10}{row}");
        }
    }
}

/// The preview as plain text: summary, then the source diff with Nix files
/// first.
pub fn render(v: &Value) -> String {
    let list = |key: &str| v[key].as_array().cloned().unwrap_or_default();
    let mut out = Rows(format!(
        "Dev shell {} · generation {} → {} (preview only; nothing was requested or changed)\n",
        text_of(&v["reference"]),
        count(&v["from"]),
        count(&v["to"])
    ));
    let (attention_notes, files) = (list("attention"), list("files"));
    let changes = ["inputs", "packages", "env"].map(list);
    if attention_notes.is_empty() && files.is_empty() && changes.iter().all(Vec::is_empty) {
        let _ = writeln!(
            out.0,
            "No changes: the workspace flake matches dev shell generation {}.",
            count(&v["from"])
        );
        return out.0;
    }
    out.section(
        "ATTENTION",
        if attention_notes.is_empty() {
            vec!["nothing unusual".into()]
        } else {
            attention_notes
                .iter()
                .map(|n| format!("! {}", attention(n)))
                .collect()
        },
    );
    let (nix, other): (Vec<_>, Vec<_>) = files
        .iter()
        .partition(|f| f["path"].as_str().unwrap_or_default().ends_with(".nix"));
    out.section(
        "SOURCE",
        if files.is_empty() {
            vec!["unchanged".into()]
        } else {
            nix.iter()
                .chain(&other)
                .map(|f| {
                    format!(
                        "{} +{} -{}",
                        text_of(&f["path"]),
                        count(&f["added"]),
                        count(&f["removed"])
                    )
                })
                .collect()
        },
    );
    for ((key, name), changes) in [
        ("inputs", "INPUTS"),
        ("packages", "PACKAGES"),
        ("env", "ENV"),
    ]
    .into_iter()
    .zip(&changes)
    {
        let rows = changes.iter().map(|c| {
            let mut row = format!(
                "{}{} ",
                tag(c["change"].as_str().unwrap_or_default()),
                text_of(&c["name"])
            );
            row.push_str(&match key {
                "inputs" => {
                    let source = |side: &str| c[side]["source"].as_str().map(plain_text);
                    let (old, new) = (source("old"), source("new"));
                    let source = if old.is_some() && new.is_some() && old != new {
                        old_new(old, new)
                    } else {
                        new.or(old).unwrap_or_default()
                    };
                    format!(
                        "{source}  {}",
                        old_new(locked(&c["old"]), locked(&c["new"]))
                    )
                }
                "packages" => old_new(versions(&c["old"]), versions(&c["new"])),
                _ if c["long"] == true => "(long value; see source diff)".into(),
                _ => old_new(
                    c["old"].as_str().map(plain_text),
                    c["new"].as_str().map(plain_text),
                ),
            });
            row.trim_end().to_string()
        });
        let rows: Vec<_> = rows.collect();
        out.section(
            name,
            if rows.is_empty() {
                vec!["unchanged".into()]
            } else {
                rows
            },
        );
    }
    out.section(
        "COST",
        [format!(
            "{} to fetch{} · {} to build",
            count(&v["fetch"]),
            v["download"]
                .as_str()
                .map(|d| format!(" ({})", plain_text(d)))
                .unwrap_or_default(),
            count(&v["build"])
        )],
    );
    let incoming = list("incoming");
    let mut rows: Vec<_> = incoming
        .iter()
        .take(INCOMING)
        .map(|item| {
            format!(
                "[{}] {} {}",
                if item["action"] == "build" { "B" } else { "F" },
                text_of(&item["name"]),
                versions(&item["versions"]).unwrap_or_default()
            )
            .trim_end()
            .to_string()
        })
        .collect();
    if incoming.len() > INCOMING {
        rows.push(format!("… and {} more", incoming.len() - INCOMING));
    }
    out.section("INCOMING", rows);
    out.0
        .push_str(&diff(v["diff"].as_str().unwrap_or_default()));
    out.0
}

/// The source patch, Nix files first, then every other file under its own
/// heading. Each line is escaped; tabs become spaces.
fn diff(patch: &str) -> String {
    let mut files = patch_files(patch);
    if files.is_empty() {
        return "\nNo source changes\n".into();
    }
    files.sort_by_key(|f| !f.nix());
    let others = files.iter().filter(|f| !f.nix()).count();
    let mut out = String::new();
    for (i, file) in files.iter().enumerate() {
        if i == 0 && file.nix() {
            out.push_str("\nNix changes\n");
        }
        if !file.nix() && (i == 0 || files[i - 1].nix()) {
            let _ = writeln!(out, "\nOther changes: {others} non-Nix file(s)");
        }
        let _ = writeln!(out, "\n{:<10} {}", file.status, plain_text(file.path));
        for line in &file.lines {
            let _ = writeln!(out, "{}", plain_text(&line.replace('\t', "    ")));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn preview_text_is_assembled_and_escaped() {
        let preview = json!({
            "kind": "devshell", "reference": "path:/p\u{1b}[2J", "from": 1, "to": 2,
            "attention": [
                {"kind": "new-input", "input": "extra", "source": "path:/x\u{202e}"},
                {"kind": "local-builds", "count": 2},
            ],
            "files": [
                {"path": "notes.txt", "added": 1, "removed": 0},
                {"path": "flake.nix", "added": 1, "removed": 1},
                {"path": "logo.png", "added": null, "removed": null},
            ],
            "inputs": [{"name": "extra", "change": "added", "old": null,
                        "new": {"source": "path:/x", "id": "abc", "date": "2026-09-28"}}],
            "packages": [{"name": "hello", "change": "changed", "old": ["2.12"], "new": ["2.13"]}],
            "env": [
                {"name": "DEV_MARKER", "change": "changed", "old": "original", "new": "ed\u{7}ited", "long": false},
                {"name": "BIG", "change": "added", "old": null, "new": null, "long": true},
            ],
            "fetch": 3, "build": 2, "download": "1.5 MiB",
            "incoming": [{"action": "build", "name": "tool", "versions": ["1.2"]},
                         {"action": "fetch", "name": "glibc", "versions": []}],
            "diff": "diff --git a/notes.txt b/notes.txt\nnew file mode 100644\n--- /dev/null\n+++ b/notes.txt\n@@ -0,0 +1 @@\n+new\u{1b}]0;x\u{7}\ndiff --git a/flake.nix b/flake.nix\n--- a/flake.nix\n+++ b/flake.nix\n@@ -1 +1 @@\n-\ta\n+\tb\n",
        });
        let text = render(&preview);
        assert!(
            !text
                .chars()
                .any(|c| (c.is_control() && c != '\n') || ('\u{202a}'..='\u{202e}').contains(&c))
        );
        for line in [
            "Dev shell path:/p\\u001b[2J · generation 1 → 2 (preview only; nothing was requested or changed)",
            "ATTENTION ! new input 'extra' from path:/x\\u202e",
            "          ! 2 derivation(s) build on this machine",
            "SOURCE    flake.nix +1 -1",
            "          notes.txt +1 -0",
            "          logo.png +? -?",
            "INPUTS    [A] extra path:/x  abc (2026-09-28)",
            "PACKAGES  [C] hello 2.12 → 2.13",
            "ENV       [C] DEV_MARKER original → ed\\u0007ited",
            "          [A] BIG (long value; see source diff)",
            "COST      3 to fetch (1.5 MiB) · 2 to build",
            "INCOMING  [B] tool 1.2",
            "          [F] glibc",
            "Nix changes",
            "modified   flake.nix",
            "-    a",
            "Other changes: 1 non-Nix file(s)",
            "new file   notes.txt",
            "+new\\u001b]0;x\\u0007",
        ] {
            assert!(
                text.lines().any(|l| l == line),
                "{line:?} missing in\n{text}"
            );
        }
        assert!(text.find("flake.nix +1").unwrap() < text.find("notes.txt +1").unwrap());
        assert!(
            text.find("modified   flake.nix").unwrap() < text.find("new file   notes.txt").unwrap()
        );
    }

    #[test]
    fn identical_generation_says_so() {
        let text = render(&json!({
            "kind": "devshell", "reference": "path:/p", "from": 3, "to": 4,
            "attention": [], "files": [], "inputs": [], "packages": [], "env": [],
            "fetch": 0, "build": 0, "download": null, "incoming": [], "diff": "",
        }));
        assert!(text.contains("No changes: the workspace flake matches dev shell generation 3."));
        assert!(!text.contains("SOURCE"));
    }

    #[test]
    fn unchanged_sections_and_long_incoming_lists() {
        let incoming: Vec<_> = (0..60)
            .map(|i| json!({"action": "fetch", "name": format!("p{i}"), "versions": ["1"]}))
            .collect();
        let text = render(&json!({
            "kind": "devshell", "reference": "path:/p", "from": 1, "to": 2,
            "attention": [], "files": [], "inputs": [],
            "packages": [{"name": "hello", "change": "removed", "old": ["2.12"], "new": null}],
            "env": [], "fetch": 60, "build": 0, "incoming": incoming, "diff": "",
        }));
        for line in [
            "ATTENTION nothing unusual",
            "SOURCE    unchanged",
            "INPUTS    unchanged",
            "PACKAGES  [R] hello 2.12",
            "ENV       unchanged",
            "COST      60 to fetch · 0 to build",
            "          … and 10 more",
            "No source changes",
        ] {
            assert!(
                text.lines().any(|l| l == line),
                "{line:?} missing in\n{text}"
            );
        }
    }
}
