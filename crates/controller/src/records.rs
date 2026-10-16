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
    // Read only, without the lock: a server starting now must not find its
    // store held, and the owner's files are left alone. Whether a record
    // runs in some other server is unknown here.
    let directory = history::Store::default_directory()?;
    if !directory.exists() {
        return Ok(vec![]);
    }
    let records = history::load(&directory)?;
    Ok(history::newest_first(&records)
        .into_iter()
        .map(|record| {
            let mut value = json!(record);
            value["running"] = Value::Null;
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
            .map(|s| text(&s["name"]))
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

/// Removed record ids, and those that could not be removed with why.
type Outcome = (Vec<String>, Vec<(String, String)>);
/// `goblins history prune`, through the owning daemon or, when the server
/// here has no history or none runs, under the store's own lock.
pub fn prune(state: &Path, prune: history::Prune) -> Result<i32> {
    let mut outcome = None;
    if let Some(mut client) = Client::connect_optional(state)? {
        match client.call("history.prune", serde_json::to_value(&prune)?) {
            Ok(result) => {
                let failed: Vec<Value> = serde_json::from_value(result["failed"].clone())?;
                outcome = Some((
                    serde_json::from_value(result["removed"].clone())?,
                    failed
                        .iter()
                        .map(|f| {
                            (
                                f["id"].as_str().unwrap_or("-").to_string(),
                                f["reason"].as_str().unwrap_or("-").to_string(),
                            )
                        })
                        .collect(),
                ));
            }
            Err(e) if e.to_string().contains("no session history") => (),
            Err(e) => return Err(e),
        }
    }
    let (removed, failed): Outcome = match outcome {
        Some(outcome) => outcome,
        None => offline_prune(&prune)?,
    };
    let verb = if prune.dry_run {
        "Would remove"
    } else {
        "Removed"
    };
    println!(
        "{verb} {} record{}",
        removed.len(),
        if removed.len() == 1 { "" } else { "s" }
    );
    for id in removed {
        println!("{id}");
    }
    if failed.is_empty() {
        return Ok(0);
    }
    eprintln!(
        "Could not remove {} record{}:",
        failed.len(),
        if failed.len() == 1 { "" } else { "s" }
    );
    for (id, reason) in failed {
        eprintln!("{id}: {}", goblins_protocol::plain_text(&reason));
    }
    Ok(1)
}

/// Prune under the store's lock; refuses, naming the owner, when another
/// process holds it. With the lock ours, none of its goblins is running.
fn offline_prune(prune: &history::Prune) -> Result<Outcome> {
    let directory = history::Store::default_directory()?;
    if !directory.exists() {
        return Ok((Vec::new(), Vec::new()));
    }
    let owner = format!("goblins history prune (pid {})", std::process::id());
    let mut store = history::Store::open(&directory, &owner).map_err(|e| {
        if e.is::<history::Held>() {
            format!("{e}; prune through that server with its --state-dir").into()
        } else {
            e
        }
    })?;
    let removed = prune.select(
        &store.records,
        &Default::default(),
        goblins_controller::messaging::now(),
    );
    if prune.dry_run {
        return Ok((removed, Vec::new()));
    }
    let result = store.delete_now(&removed);
    Ok((result.removed, result.failed))
}
