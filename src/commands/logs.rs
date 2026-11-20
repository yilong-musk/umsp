//! `umsp logs tail|export`.

use std::collections::HashMap;
use std::collections::hash_map::Entry;
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde_json::json;

use crate::cli::LogsCmd;
use crate::error::{Result, UmspError};
use crate::usage::LogStore;
use crate::util;

pub fn run(path: &Path, cmd: LogsCmd, json: bool) -> Result<i32> {
    let cfg = crate::commands::load_config(path)?;
    let store = LogStore::new(&cfg.server.state_path());
    match cmd {
        LogsCmd::Tail { lines, follow } => tail(&store, lines, follow, json),
        LogsCmd::Export { since, out } => export(&store, &since, &out, json),
    }
}

fn tail(store: &LogStore, lines: usize, follow: bool, json: bool) -> Result<i32> {
    let mut printed: HashMap<PathBuf, usize> = HashMap::new();
    let mut first_pass = true;
    loop {
        let mut batch: Vec<String> = Vec::new();
        for file in store.files() {
            let Ok(raw) = std::fs::read_to_string(&file) else {
                continue;
            };
            let all: Vec<&str> = raw.lines().filter(|l| !l.trim().is_empty()).collect();
            let seen = match printed.entry(file.clone()) {
                Entry::Occupied(e) => *e.get(),
                Entry::Vacant(e) => {
                    e.insert(0);
                    0
                }
            };
            let start = if first_pass {
                all.len().saturating_sub(lines)
            } else {
                seen.min(all.len())
            };
            for line in &all[start..] {
                batch.push((*line).to_string());
            }
            printed.insert(file, all.len());
        }

        if json {
            if !batch.is_empty() || (first_pass && !follow) {
                crate::commands::emit_json(json!({"ok": true, "lines": batch}));
            }
        } else {
            for line in &batch {
                println!("{line}");
            }
        }

        if !follow {
            break;
        }
        first_pass = false;
        std::thread::sleep(Duration::from_millis(500));
    }
    Ok(0)
}

fn export(store: &LogStore, since: &str, out: &Path, json: bool) -> Result<i32> {
    let since_dt = util::parse_since(since)?;
    let records = store.read_since(since_dt);
    let mut text = String::new();
    for record in &records {
        let line = serde_json::to_string(record)
            .map_err(|e| UmspError::other("EXPORT", format!("cannot serialize record: {e}")))?;
        text.push_str(&line);
        text.push('\n');
    }
    if let Some(parent) = out.parent() {
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent).map_err(|e| {
                UmspError::other("EXPORT", format!("cannot create {}: {e}", parent.display()))
            })?;
        }
    }
    std::fs::write(out, text)
        .map_err(|e| UmspError::other("EXPORT", format!("cannot write {}: {e}", out.display())))?;
    let value = json!({
        "out": out,
        "records": records.len(),
        "since": since_dt.to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
    });
    crate::commands::emit(json, value, || {
        format!("wrote {} record(s) to {}", records.len(), out.display())
    })?;
    Ok(0)
}
