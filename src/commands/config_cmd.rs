//! `umsp config {path,show,validate}`.

use std::path::Path;

use serde_json::json;

use crate::cli::ConfigCmd;
use crate::config::Config;
use crate::error::{Result, UmspError};

pub fn run(path: &Path, cmd: ConfigCmd, json: bool) -> Result<i32> {
    match cmd {
        ConfigCmd::Path => {
            let value = json!({ "path": path, "exists": path.exists() });
            crate::commands::emit(json, value, || path.display().to_string())
        }
        ConfigCmd::Show { redact } => {
            let raw = std::fs::read_to_string(path).map_err(|e| {
                UmspError::config(format!("cannot read config {}: {e}", path.display()))
            })?;
            let mut value: serde_json::Value = {
                let cfg = Config::load(path)?;
                serde_json::to_value(&cfg).map_err(|e| {
                    UmspError::other("CONFIG", format!("cannot serialize config: {e}"))
                })?
            };
            if redact {
                if let Some(clients) = value.get_mut("clients").and_then(|c| c.as_array_mut()) {
                    for client in clients {
                        if let Some(obj) = client.as_object_mut() {
                            if let Some(hash) = obj.get("key_hash").and_then(|v| v.as_str()) {
                                let prefix: String = hash.chars().take(14).collect();
                                obj.insert("key_hash".to_string(), json!(format!("{prefix}…")));
                            }
                        }
                    }
                }
            }
            if json {
                crate::commands::emit_json(json!({"ok": true, "path": path, "config": value}));
                Ok(0)
            } else if redact {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&value).unwrap_or_default()
                );
                Ok(0)
            } else {
                print!("{raw}");
                Ok(0)
            }
        }
        ConfigCmd::Validate => {
            let cfg = Config::load(path)?;
            let issues = cfg.validate();
            let errors: Vec<_> = issues.iter().filter(|i| i.level == "error").collect();
            let ok = errors.is_empty();
            if json {
                crate::commands::emit_json(json!({
                    "ok": ok,
                    "path": path,
                    "providers": cfg.providers.len(),
                    "models": cfg.models.len(),
                    "clients": cfg.clients.len(),
                    "issues": issues,
                }));
            } else if ok {
                println!(
                    "config ok: {} providers, {} models, {} clients",
                    cfg.providers.len(),
                    cfg.models.len(),
                    cfg.clients.len()
                );
            } else {
                for issue in &issues {
                    println!("{:<7} {:<28} {}", issue.level, issue.code, issue.message);
                }
            }
            Ok(if ok { 0 } else { crate::error::EXIT_OTHER })
        }
    }
}
