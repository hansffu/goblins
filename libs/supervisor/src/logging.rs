use std::{fs::OpenOptions, io::Write, path::Path};

// Runtime-owned diagnostic writer. Never creates the resource directory.
pub(crate) fn log_event(directory: &Path, event: &str, fields: serde_json::Value) {
    // Logging must never interrupt teardown or turn a completed grant into
    // an unreported mount failure. Directory is host-only, mode 0700.
    if let Ok(mut file) = OpenOptions::new()
        .create(true)
        .append(true)
        .open(directory.join("events.jsonl"))
    {
        if file.metadata().is_ok_and(|m| m.len() >= 1024 * 1024) {
            return;
        }
        let time = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs_f64();
        let _ = writeln!(
            file,
            "{}",
            serde_json::json!({"time":time,"event":event,"fields":fields})
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn capped_or_absent_resource_log_never_changes_lifecycle_or_recreates_directories() {
        let _no_forks = goblins_unix::no_forks();
        let root = goblins_unix::temp_directory().unwrap();
        let path = root.join("events.jsonl");
        std::fs::write(&path, vec![b'x'; 1024 * 1024]).unwrap();
        log_event(&root, "discarded", serde_json::json!({"exec":1}));
        assert_eq!(std::fs::metadata(&path).unwrap().len(), 1024 * 1024);
        std::fs::remove_dir_all(&root).unwrap();
        log_event(&root, "late", serde_json::json!({"exec":2}));
        assert!(!root.exists());
    }
}
