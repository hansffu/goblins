//! `goblins history`: recorded goblins, through the daemon that owns the
//! store, or straight from the store when no daemon runs here.
use goblins_controller::{Result, history, host::Client};
use goblins_controller::history::PruneExt;
use goblins_protocol::contract;
use serde_json::{Value, json};
use std::path::Path;

/// The records, newest first, each with `running` (null when unknown).
pub(crate) fn records(state: &Path) -> Result<Vec<Value>> {
    if let Some(mut client) = Client::connect_optional(state)? {
        match client.call_typed::<contract::host::HistoryList>(contract::MailboxStatusParams {}) {
            Ok(list) => {
                if list.truncated {
                    eprintln!("goblins: showing only the newest records that fit one response");
                }
                return Ok(list.records);
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
        .map(|record| json!(history::wire(record, None)))
        .collect())
}

pub fn list(state: &Path, as_json: bool, pager: bool) -> Result<i32> {
    let records = records(state)?;
    if as_json {
        println!("{}", Value::Array(records));
        return Ok(0);
    }
    let text = |v: Option<&str>| goblins_protocol::plain_text(v.unwrap_or("-"));
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
    for record in &records {
        let r = contract::HistoryDisplayRecord::project(record);
        let skipped: Vec<_> = r.skipped.iter().map(|s| text(s.as_deref())).collect();
        rows.push([
            text(r.id.as_deref()),
            text(r.agent_name.as_deref()),
            text(r.configuration.as_deref()),
            text(r.session_name.as_deref()),
            text(r.cwd.as_deref()),
            r.updated.map_or("-".into(), history::local_time),
            match r.running {
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
    let mut table = String::new();
    for row in rows {
        let line: Vec<_> = row
            .iter()
            .zip(&widths)
            .map(|(cell, width)| format!("{cell:width$}"))
            .collect();
        table.push_str(line.join("  ").trim_end());
        table.push('\n');
    }
    if !pager || !page(&table) {
        print!("{table}");
    }
    Ok(0)
}

/// Show `text` through $PAGER (default `less`) when stdout is a terminal,
/// like git log. False when it was not paged.
fn page(text: &str) -> bool {
    use std::io::Write;
    if unsafe { libc::isatty(1) } != 1 {
        return false;
    }
    let pager = std::env::var("PAGER").unwrap_or_else(|_| "less".into());
    if pager.trim().is_empty() || pager.trim() == "cat" {
        return false;
    }
    let Ok(mut child) = std::process::Command::new("/bin/sh")
        .args(["-c", &pager])
        .env(
            "LESS",
            std::env::var("LESS").unwrap_or_else(|_| "FRX".into()),
        )
        .stdin(std::process::Stdio::piped())
        .spawn()
    else {
        return false;
    };
    // A pager quit early closes its input; that is not an error here.
    let _ = child.stdin.take().unwrap().write_all(text.as_bytes());
    let _ = child.wait();
    true
}
/// Removed record ids, and those that could not be removed with why.
type Outcome = (Vec<String>, Vec<(String, String)>);
/// `goblins history prune`, through the owning daemon or, when the server
/// here has no history or none runs, under the store's own lock.
pub fn prune(state: &Path, prune: history::Prune) -> Result<i32> {
    let mut outcome = None;
    if let Some(mut client) = Client::connect_optional(state)? {
        match client.call_typed::<contract::host::HistoryPrune>(prune.clone()) {
            Ok(result) => {
                outcome = Some((
                    result.removed,
                    result
                        .failed
                        .iter()
                        .map(|value| {
                            let row = contract::HistoryPruneFailureRead::project(value);
                            (row.id, row.reason)
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

#[cfg(test)]
mod tests {
    use super::*;
    use goblins_jsonrpc as rpc;
    use std::{io::Write, os::fd::AsRawFd, os::unix::net::UnixListener};

    struct C14Root(std::path::PathBuf);
    impl Drop for C14Root {
        fn drop(&mut self) {
            std::fs::remove_dir_all(&self.0).unwrap();
        }
    }
    fn c14_peer<T>(
        method: &str,
        params: Value,
        response: std::result::Result<Value, (i32, &str)>,
        invoke: impl FnOnce(&Path) -> T,
    ) -> T {
        // Guard outlives both the invoking client and scoped peer cleanup.
        let _no_forks = goblins_unix::no_forks();
        let root = C14Root(goblins_unix::temp_directory().unwrap());
        let listener = UnixListener::bind(root.0.join("host.sock")).unwrap();
        listener.set_nonblocking(true).unwrap();
        let fixtures: Value =
            serde_json::from_str(include_str!("../../../libs/api/tests/fixtures.json")).unwrap();
        std::thread::scope(|scope| {
            let server = scope.spawn(move || {
                assert!(
                    goblins_unix::readable(listener.as_raw_fd(), 3000).unwrap(),
                    "client did not connect before deadline"
                );
                let (mut peer, _) = listener.accept().unwrap();
                peer.set_read_timeout(Some(rpc::TIMEOUT)).unwrap();
                peer.set_write_timeout(Some(rpc::TIMEOUT)).unwrap();
                let init = rpc::read(&mut peer).unwrap();
                assert_eq!(init["method"], "initialize");
                assert_eq!(init["params"], json!({"api":1}));
                peer.write_all(
                    &rpc::encode(&rpc::result(
                        init["id"].clone(),
                        fixtures["host_initialize"].clone(),
                    ))
                    .unwrap(),
                )
                .unwrap();
                let call = rpc::read(&mut peer).unwrap();
                assert_eq!(call["method"], method);
                assert_eq!(call["params"], params);
                let reply = match response {
                    Ok(value) => rpc::result(call["id"].clone(), value),
                    Err((code, message)) => rpc::error(call["id"].clone(), code, message),
                };
                peer.write_all(&rpc::encode(&reply).unwrap()).unwrap();
            });
            let result = invoke(&root.0);
            server.join().unwrap();
            result
        })
    }
    #[test]
    fn c14_online_records_keep_raw_partial_values_and_legacy_empty_fallbacks() {
        let raw = json!([{"id":"a","running":null,"version":99,"future":[1]},null,17]);
        for reply in [
            json!({"records":raw,"truncated":false}),
            json!({"records":raw,"truncated":true}),
            json!({"records":raw,"truncated":"future"}),
        ] {
            assert_eq!(
                c14_peer("history.list", json!({}), Ok(reply), |state| records(state)).unwrap(),
                raw.as_array().unwrap().clone()
            );
        }
        for reply in [
            json!({}),
            json!(null),
            json!([]),
            json!({"records":null}),
            json!({"records":17}),
        ] {
            assert!(
                c14_peer("history.list", json!({}), Ok(reply), |state| records(state))
                    .unwrap()
                    .is_empty()
            );
        }
        let error = c14_peer(
            "history.list",
            json!({}),
            Err((-32009, "access refused")),
            |state| records(state),
        )
        .unwrap_err();
        assert!(error.to_string().contains("access refused"));
    }
    #[test]
    fn c14_online_prune_keeps_request_defaults_partial_failures_and_decode_errors() {
        for dry_run in [false, true] {
            for (reply, exit) in [
                (json!({"removed":["a"],"failed":[],"dry_run":!dry_run}), 0),
                (
                    json!({"removed":[],"failed":[{"id":"a","reason":"permission refused"}],"future":true}),
                    1,
                ),
                (
                    json!({"removed":[],"failed":[null,17,{"id":17,"reason":false}]}),
                    1,
                ),
            ] {
                let request = history::Prune {
                    keep: Some(0),
                    dry_run,
                    ..Default::default()
                };
                assert_eq!(
                    c14_peer(
                        "history.prune",
                        json!({"older_than":null,"before":null,"keep":0,"dry_run":dry_run}),
                        Ok(reply),
                        |state| prune(state, request)
                    )
                    .unwrap(),
                    exit
                );
            }
        }
        for reply in [
            json!({}),
            json!({"removed":[],"failed":null}),
            json!({"removed":[],"failed":17}),
            json!({"removed":null,"failed":[]}),
            json!({"removed":[17],"failed":[]}),
        ] {
            let request = history::Prune {
                keep: Some(0),
                ..Default::default()
            };
            assert!(
                c14_peer(
                    "history.prune",
                    json!({"older_than":null,"before":null,"keep":0,"dry_run":false}),
                    Ok(reply),
                    |state| prune(state, request)
                )
                .is_err()
            );
        }
        let request = history::Prune {
            keep: Some(0),
            ..Default::default()
        };
        let error = c14_peer(
            "history.prune",
            json!({"older_than":null,"before":null,"keep":0,"dry_run":false}),
            Err((-32009, "access refused")),
            |state| prune(state, request),
        )
        .unwrap_err();
        assert!(error.to_string().contains("access refused"));
    }
}
