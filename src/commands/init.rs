//! `umsp init` — create config.toml, env file and state directory.

use std::path::Path;

use serde_json::json;

use crate::cli::InitArgs;
use crate::config::{CONFIG_TEMPLATE, Config};
use crate::envfile::EnvFile;
use crate::error::{Result, UmspError};

pub fn run(path: &Path, args: InitArgs, json: bool) -> Result<i32> {
    let mut template = CONFIG_TEMPLATE.to_string();
    if let Some(state_dir) = &args.state_dir {
        template = template.replace(
            "state_dir = \"~/.local/share/umsp\"",
            &format!("state_dir = \"{}\"", state_dir.display()),
        );
    }

    let existed = path.exists();
    let mut created: Vec<&str> = Vec::new();
    if !existed || args.force {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| {
                UmspError::other("INIT", format!("cannot create {}: {e}", parent.display()))
            })?;
        }
        std::fs::write(path, &template).map_err(|e| {
            UmspError::other("INIT", format!("cannot write {}: {e}", path.display()))
        })?;
        // The config carries client key hashes and prefixes; keep it owner-only.
        crate::envfile::set_mode_600(path)?;
        created.push("config");
    }

    let env_path = Config::resolve_env_path();
    if EnvFile::ensure_file(&env_path)? {
        created.push("env");
    }

    let cfg = Config::load(path)?;
    let state_dir = cfg.server.state_path();
    std::fs::create_dir_all(state_dir.join("logs")).map_err(|e| {
        UmspError::other(
            "INIT",
            format!("cannot create {}: {e}", state_dir.display()),
        )
    })?;

    let value = json!({
        "config": path,
        "env": env_path,
        "state_dir": state_dir,
        "created": created,
        "config_existed": existed,
        "next": [
            format!("set upstream keys in {}", env_path.display()),
            "umsp client add --name <name>".to_string(),
            "umsp check --json".to_string(),
            "umsp serve".to_string(),
        ],
    });

    if json {
        crate::commands::emit_json(crate::server::success_json(value));
    } else {
        println!("config:    {}", path.display());
        println!("env:       {}", env_path.display());
        println!("state dir: {}", state_dir.display());
        if created.is_empty() {
            println!("nothing created (config already existed; use --force to overwrite)");
        } else {
            println!("created:   {}", created.join(", "));
        }
    }
    Ok(0)
}
