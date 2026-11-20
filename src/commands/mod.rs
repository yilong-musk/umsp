//! CLI dispatch.

mod check;
mod config_cmd;
pub mod config_edit;
pub mod env_cmd;
mod init;
mod logs;
mod registry_cmds;
mod run_cmds;
mod usage_cmd;
mod version;

use std::path::{Path, PathBuf};

use serde_json::Value;

use crate::cli::{Cli, Command};
use crate::config::Config;
use crate::error::Result;

pub fn run(cli: Cli) -> Result<i32> {
    let config_path = Config::resolve_path(cli.config.as_deref());
    let json = cli.json;
    let no_reload = cli.no_reload;

    match cli.command {
        Command::Serve(args) => {
            let rt = tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .build()
                .map_err(|e| crate::error::UmspError::other("RUNTIME", format!("tokio: {e}")))?;
            rt.block_on(crate::server::serve(&config_path, args.listen))?;
            Ok(0)
        }
        Command::Init(args) => init::run(&config_path, args, json),
        Command::Config { cmd } => config_cmd::run(&config_path, cmd, json),
        Command::Env { cmd } => env_cmd::run(cmd, json),
        Command::Check(_) => check::run_check(&config_path, json),
        Command::Doctor(args) => check::run_doctor(&config_path, args, json),
        Command::Provider { cmd } => {
            registry_cmds::run_provider(&config_path, cmd, json, no_reload)
        }
        Command::Vendor { cmd } => registry_cmds::run_vendor(&config_path, cmd, json),
        Command::Model { cmd } => registry_cmds::run_model(&config_path, cmd, json, no_reload),
        Command::Client { cmd } => registry_cmds::run_client(&config_path, cmd, json, no_reload),
        Command::Key { cmd } => registry_cmds::run_key(&config_path, cmd, json, no_reload),
        Command::Usage { cmd } => usage_cmd::run(&config_path, cmd, json),
        Command::Probe(args) => run_cmds::run_probe(&config_path, args, json),
        Command::Call(args) => run_cmds::run_call(&config_path, args, json),
        Command::Logs { cmd } => logs::run(&config_path, cmd, json),
        Command::Version => version::run(json),
        Command::Completion { shell } => {
            use clap::CommandFactory;
            use std::io::Write;
            let mut cmd = Cli::command();
            let mut out: Vec<u8> = Vec::new();
            clap_complete::generate(shell, &mut cmd, "umsp", &mut out);
            // Piping into `head` closes the pipe early; treat EPIPE as success
            // instead of panicking on unwrap inside clap_complete.
            match std::io::stdout().write_all(&out) {
                Ok(()) => {}
                Err(err) if err.kind() == std::io::ErrorKind::BrokenPipe => {}
                Err(err) => {
                    return Err(crate::error::UmspError::other(
                        "COMPLETION",
                        format!("cannot write completion script: {err}"),
                    ));
                }
            }
            Ok(0)
        }
    }
}

pub fn load_config(path: &Path) -> Result<Config> {
    Config::load_or_default(path)
}

pub fn emit_json(value: Value) {
    println!(
        "{}",
        serde_json::to_string(&value).unwrap_or_else(|_| "{}".to_string())
    );
}

pub fn emit(json: bool, value: Value, text: impl FnOnce() -> String) -> Result<i32> {
    if json {
        emit_json(crate::server::success_json(value));
    } else {
        println!("{}", text());
    }
    Ok(0)
}

pub fn resolve_state_dir(cfg: &Config) -> PathBuf {
    cfg.server.state_path()
}
