use std::{fs::OpenOptions, io::Write, path::Path};

// Transitional daemon writer; phase 6 replaces daemon writes with an event.
pub fn log_event(directory: &Path, event: &str, fields: serde_json::Value) {
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
