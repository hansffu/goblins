//! `goblins history`: recorded goblins, through the daemon that owns the
//! store, or straight from the store when no daemon runs here.
use goblins_controller::{Result, history, host::Client};
use serde_json::{Value, json};
use std::path::Path;

/// The records, newest first, each with `running` (null when unknown).
fn records(state: &Path) -> Result<Vec<Value>> {
    if let Some(mut client) = Client::connect_optional(state)? {
        match client.call("history.list", json!({})) {
            Ok(list) => {
                if list["truncated"] == true {
                    eprintln!("goblins: showing only the newest records that fit one response");
                }
                return Ok(list["records"].as_array().cloned().unwrap_or_default());
            }
            // This daemon runs without history; read the store itself.
            Err(e) if e.to_string().contains("no session history") => (),
            Err(e) => return Err(e),
        }
    }
    let directory = history::Store::default_directory()?;
    if !directory.exists() {
        return Ok(vec![]);
    }
    // Holding the lock proves no daemon runs any of them.
    let (records, running) = match history::Store::open(&directory) {
        Ok(store) => (store.records.clone(), json!(false)),
        Err(_) => (history::load(&directory)?, Value::Null),
    };
    Ok(history::newest_first(&records)
        .into_iter()
        .map(|record| {
            let mut value = json!(record);
            value["running"] = running.clone();
            value
        })
        .collect())
}

pub fn list(state: &Path, as_json: bool) -> Result<i32> {
    let records = records(state)?;
    if as_json {
        println!("{}", Value::Array(records));
        return Ok(0);
    }
    let text = |v: &Value| goblins_protocol::plain_text(v.as_str().unwrap_or("-"));
    let mut rows = vec![
        [
            "ID",
            "NAME",
            "CONFIGURATION",
            "SESSION",
            "DIRECTORY",
            "UPDATED",
            "RUNNING",
            "SKIPPED",
        ]
        .map(String::from),
    ];
    for r in &records {
        let skipped: Vec<_> = r["skipped"]
            .as_array()
            .into_iter()
            .flatten()
            .map(text)
            .collect();
        rows.push([
            text(&r["id"]),
            text(&r["agent_name"]),
            text(&r["configuration"]),
            text(&r["variables"]["session_name"]),
            text(&r["cwd"]),
            r["updated"]
                .as_u64()
                .map_or("-".into(), history::local_time),
            match r["running"].as_bool() {
                Some(true) => "yes",
                Some(false) => "-",
                None => "?",
            }
            .into(),
            if skipped.is_empty() {
                "-".into()
            } else {
                skipped.join(",")
            },
        ]);
    }
    let widths: Vec<_> = (0..8)
        .map(|i| rows.iter().map(|r| r[i].chars().count()).max().unwrap_or(0))
        .collect();
    for row in rows {
        let line: Vec<_> = row
            .iter()
            .zip(&widths)
            .map(|(cell, width)| format!("{cell:width$}"))
            .collect();
        println!("{}", line.join("  ").trim_end());
    }
    Ok(0)
}
