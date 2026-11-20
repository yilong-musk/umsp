//! `umsp check` and `umsp doctor`.

use std::path::Path;

use serde_json::{Value, json};

use crate::cli::DoctorArgs;
use crate::config::Config;
use crate::envfile::{self, EnvFile};
use crate::error::{EXIT_OTHER, Result, UmspError};

pub fn run_check(path: &Path, json: bool) -> Result<i32> {
    let mut errors: Vec<Value> = Vec::new();
    let mut warnings: Vec<Value> = Vec::new();

    let cfg = match Config::load(path) {
        Ok(cfg) => {
            for issue in cfg.validate() {
                let entry = json!({"code": issue.code, "message": issue.message});
                if issue.level == "error" {
                    errors.push(entry);
                } else {
                    warnings.push(entry);
                }
            }
            Some(cfg)
        }
        Err(err) => {
            errors.push(json!({"code": err.code, "message": err.message}));
            None
        }
    };

    let mut env_ok = true;
    if let Some(cfg) = &cfg {
        let env_path = Config::resolve_env_path();
        let env = EnvFile::load(&env_path);
        for provider in &cfg.providers {
            if !env.has(&provider.api_key_env) {
                errors.push(json!({
                    "code": "provider_key_missing",
                    "message": format!(
                        "provider '{}' needs {} (put it in {} or the process environment)",
                        provider.id, provider.api_key_env, env_path.display()
                    )
                }));
                env_ok = false;
            }
        }
        if let Some(mode) = envfile::file_mode(&env_path) {
            if mode & 0o077 != 0 {
                warnings.push(json!({
                    "code": "env_file_mode",
                    "message": format!("{} has mode {:o}; expected 600", env_path.display(), mode)
                }));
            }
        }
        match state_dir_writable(cfg) {
            Ok(()) => {}
            Err(message) => {
                errors.push(json!({"code": "state_dir_unwritable", "message": message}))
            }
        }
    }

    let ok = errors.is_empty();
    let value = json!({
        "ok": ok,
        "config": path,
        "env_file": Config::resolve_env_path(),
        "env_ok": env_ok,
        "providers": cfg.as_ref().map(|c| c.providers.len()).unwrap_or(0),
        "models": cfg.as_ref().map(|c| c.models.len()).unwrap_or(0),
        "clients": cfg.as_ref().map(|c| c.clients.len()).unwrap_or(0),
        "errors": errors,
        "warnings": warnings,
    });

    if json {
        crate::commands::emit_json(value);
    } else if ok {
        println!(
            "ok: {} providers, {} models, {} clients{}",
            value["providers"],
            value["models"],
            value["clients"],
            if warnings.is_empty() {
                String::new()
            } else {
                format!(", {} warning(s)", warnings.len())
            }
        );
        for warning in &warnings {
            println!(
                "warning  {}  {}",
                warning["code"].as_str().unwrap_or("-"),
                warning["message"].as_str().unwrap_or("")
            );
        }
    } else {
        for error in &errors {
            println!(
                "error    {}  {}",
                error["code"].as_str().unwrap_or("-"),
                error["message"].as_str().unwrap_or("")
            );
        }
        for warning in &warnings {
            println!(
                "warning  {}  {}",
                warning["code"].as_str().unwrap_or("-"),
                warning["message"].as_str().unwrap_or("")
            );
        }
    }

    Ok(if ok { 0 } else { EXIT_OTHER })
}

fn state_dir_writable(cfg: &Config) -> std::result::Result<(), String> {
    let state_dir = cfg.server.state_path();
    std::fs::create_dir_all(&state_dir)
        .map_err(|e| format!("cannot create {}: {e}", state_dir.display()))?;
    let probe = state_dir.join(".umsp-write-test");
    std::fs::write(&probe, b"ok").map_err(|e| format!("cannot write {}: {e}", probe.display()))?;
    let _ = std::fs::remove_file(&probe);
    Ok(())
}

/// The ledger lives in `<state>/logs`; prove it can be created and appended to.
fn logs_writable(cfg: &Config) -> std::result::Result<std::path::PathBuf, String> {
    let logs = cfg.server.state_path().join("logs");
    std::fs::create_dir_all(&logs).map_err(|e| format!("cannot create {}: {e}", logs.display()))?;
    let probe = logs.join(".umsp-write-test");
    std::fs::write(&probe, b"ok").map_err(|e| format!("cannot write {}: {e}", probe.display()))?;
    let _ = std::fs::remove_file(&probe);
    Ok(logs)
}

/// `Ok(Some(detail))` when a server answers `/metrics`, `Ok(None)` when nothing
/// listens there. Never fails doctor: a stopped data plane is a skip, not an
/// error.
fn metrics_probe(listen: &str) -> std::result::Result<Option<String>, String> {
    use std::io::{Read, Write};
    let addr = listen
        .parse::<std::net::SocketAddr>()
        .map_err(|_| format!("cannot probe '{listen}': not a socket address"))?;
    let timeout = std::time::Duration::from_millis(400);
    let mut stream = match std::net::TcpStream::connect_timeout(&addr, timeout) {
        Ok(stream) => stream,
        Err(_) => return Ok(None),
    };
    let _ = stream.set_read_timeout(Some(timeout));
    let request = format!("GET /metrics HTTP/1.1\r\nHost: {addr}\r\nConnection: close\r\n\r\n");
    if let Err(e) = stream.write_all(request.as_bytes()) {
        return Ok(Some(format!("connected but write failed: {e}")));
    }
    let mut raw = Vec::new();
    let mut chunk = [0u8; 1024];
    loop {
        match stream.read(&mut chunk) {
            Ok(0) => break,
            Ok(n) => {
                raw.extend_from_slice(&chunk[..n]);
                if raw.len() >= 4096 {
                    break;
                }
            }
            Err(_) => break,
        }
    }
    let text = String::from_utf8_lossy(&raw);
    let status = text.lines().next().unwrap_or("");
    if status.contains(" 200") {
        let body_len = text
            .find("\r\n\r\n")
            .map(|i| text.len() - i - 4)
            .unwrap_or(0);
        Ok(Some(format!(
            "/metrics 200 on {addr} ({body_len} bytes read)"
        )))
    } else if status.is_empty() {
        Err(format!("{addr} accepted the connection but sent no reply"))
    } else {
        Err(format!("{addr} answered {status:?} instead of 200"))
    }
}

#[derive(Debug)]
struct CheckRow {
    name: String,
    status: &'static str, // ok | warn | fail | skip
    detail: String,
}

impl CheckRow {
    fn ok(name: impl Into<String>, detail: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            status: "ok",
            detail: detail.into(),
        }
    }
    fn warn(name: impl Into<String>, detail: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            status: "warn",
            detail: detail.into(),
        }
    }
    fn fail(name: impl Into<String>, detail: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            status: "fail",
            detail: detail.into(),
        }
    }
    fn skip(name: impl Into<String>, detail: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            status: "skip",
            detail: detail.into(),
        }
    }
}

pub fn run_doctor(path: &Path, args: DoctorArgs, json: bool) -> Result<i32> {
    let mut rows = Vec::new();

    // 1. config file + parse + validate
    let cfg = if !path.exists() {
        rows.push(CheckRow::fail(
            "config_file",
            format!("{} does not exist (run `umsp init`)", path.display()),
        ));
        None
    } else {
        rows.push(CheckRow::ok("config_file", path.display().to_string()));
        match Config::load(path) {
            Ok(cfg) => {
                rows.push(CheckRow::ok("config_parse", "toml parses"));
                let issues = cfg.validate();
                let errors: Vec<String> = issues
                    .iter()
                    .filter(|i| i.level == "error")
                    .map(|i| format!("{}: {}", i.code, i.message))
                    .collect();
                let warnings: Vec<String> = issues
                    .iter()
                    .filter(|i| i.level == "warning")
                    .map(|i| format!("{}: {}", i.code, i.message))
                    .collect();
                if errors.is_empty() {
                    rows.push(CheckRow::ok(
                        "config_validate",
                        format!(
                            "{} providers, {} models, {} clients",
                            cfg.providers.len(),
                            cfg.models.len(),
                            cfg.clients.len()
                        ),
                    ));
                } else {
                    rows.push(CheckRow::fail("config_validate", errors.join("; ")));
                }
                if !warnings.is_empty() {
                    rows.push(CheckRow::warn("config_warnings", warnings.join("; ")));
                }
                Some(cfg)
            }
            Err(err) => {
                rows.push(CheckRow::fail("config_parse", err.message));
                None
            }
        }
    };

    // 2. env file + provider keys
    let env_path = Config::resolve_env_path();
    let env = EnvFile::load(&env_path);
    if env_path.exists() {
        let mode = envfile::file_mode(&env_path).unwrap_or(0);
        if mode & 0o077 == 0 {
            rows.push(CheckRow::ok(
                "env_file",
                format!("{} mode {:o}", env_path.display(), mode),
            ));
        } else {
            rows.push(CheckRow::warn(
                "env_file_mode",
                format!("{} mode {:o}; expected 600", env_path.display(), mode),
            ));
        }
    } else {
        rows.push(CheckRow::warn(
            "env_file",
            format!(
                "{} not found (fine if keys come from the process env)",
                env_path.display()
            ),
        ));
    }
    if let Some(cfg) = &cfg {
        if cfg.providers.is_empty() {
            rows.push(CheckRow::warn("providers", "none configured"));
        }
        for provider in &cfg.providers {
            let name = format!("provider_key:{}", provider.id);
            if env.has(&provider.api_key_env) {
                rows.push(CheckRow::ok(
                    name,
                    format!("{} is set", provider.api_key_env),
                ));
            } else {
                rows.push(CheckRow::fail(
                    name,
                    format!("{} is not set", provider.api_key_env),
                ));
            }
            let models = cfg
                .models
                .iter()
                .filter(|m| m.backends.iter().any(|b| b.provider == provider.id))
                .count();
            if models == 0 {
                rows.push(CheckRow::warn(
                    format!("provider_models:{}", provider.id),
                    "no models configured for this provider",
                ));
            }
        }

        // 3. listen address + state dir
        match cfg.server.listen.parse::<std::net::SocketAddr>() {
            Ok(addr) => {
                if addr.ip().is_loopback() {
                    rows.push(CheckRow::ok(
                        "listen",
                        format!("{} (loopback)", cfg.server.listen),
                    ));
                } else {
                    rows.push(CheckRow::warn(
                        "listen",
                        format!(
                            "{} is not loopback — the spec only allows 127.0.0.1",
                            cfg.server.listen
                        ),
                    ));
                }
            }
            Err(_) => rows.push(CheckRow::fail(
                "listen",
                format!("invalid address '{}'", cfg.server.listen),
            )),
        }
        match state_dir_writable(cfg) {
            Ok(_) => rows.push(CheckRow::ok(
                "state_dir",
                cfg.server.state_path().display().to_string(),
            )),
            Err(message) => rows.push(CheckRow::fail("state_dir", message)),
        }
        match logs_writable(cfg) {
            Ok(path) => rows.push(CheckRow::ok(
                "logs",
                format!("{} is writable", path.display()),
            )),
            Err(message) => rows.push(CheckRow::fail("logs", message)),
        }
        // `/metrics` only exists while `serve` runs; not running is not a
        // failure (spec 6.2 — the data plane is optional at doctor time).
        // `UMSP_LISTEN` wins over the file, same as `umsp serve`.
        let probed_listen = std::env::var("UMSP_LISTEN")
            .ok()
            .filter(|value| !value.trim().is_empty())
            .unwrap_or_else(|| cfg.server.listen.clone());
        match metrics_probe(&probed_listen) {
            Ok(Some(detail)) => rows.push(CheckRow::ok("metrics", detail)),
            Ok(None) => rows.push(CheckRow::skip(
                "metrics",
                format!(
                    "umsp serve is not listening on {probed_listen}; /metrics appears once it runs"
                ),
            )),
            Err(message) => rows.push(CheckRow::warn("metrics", message)),
        }
        if cfg.clients.is_empty() {
            rows.push(CheckRow::fail(
                "clients",
                "none configured; every request would get 401",
            ));
        } else {
            rows.push(CheckRow::ok(
                "clients",
                format!("{} configured", cfg.clients.len()),
            ));
        }
    }

    // 4. binary
    rows.push(CheckRow::ok(
        "binary",
        format!(
            "umsp {} ({}-{})",
            crate::server::VERSION,
            std::env::consts::OS,
            std::env::consts::ARCH
        ),
    ));

    // 5. optional network probes
    if args.online {
        match &cfg {
            Some(cfg) if !cfg.providers.is_empty() => {
                for provider in &cfg.providers {
                    let name = format!("online:{}", provider.id);
                    match crate::commands::run_cmds::probe_blocking(cfg, &provider.id, "chat", None)
                    {
                        Ok(report) => {
                            if report.ok {
                                rows.push(CheckRow::ok(
                                    name,
                                    format!(
                                        "{} in {} ms",
                                        report.status.map(|s| s.to_string()).unwrap_or_default(),
                                        report.latency_ms
                                    ),
                                ));
                            } else {
                                let detail = report.error.unwrap_or_else(|| {
                                    format!(
                                        "http {}",
                                        report.status.map(|s| s.to_string()).unwrap_or_default()
                                    )
                                });
                                rows.push(CheckRow::fail(name, detail));
                            }
                        }
                        Err(err) => rows.push(CheckRow::fail(name, err.message)),
                    }
                }
            }
            _ => rows.push(CheckRow::skip("online", "skipped: no valid config")),
        }
    } else {
        rows.push(CheckRow::skip("online", "use --online to probe upstreams"));
    }

    let ok = rows.iter().filter(|r| r.status == "fail").count();
    let warn = rows.iter().filter(|r| r.status == "warn").count();
    let good = rows.iter().filter(|r| r.status == "ok").count();
    let passed = ok == 0;

    let checks: Vec<Value> = rows
        .iter()
        .map(|r| json!({"name": r.name, "status": r.status, "detail": r.detail}))
        .collect();
    let value = json!({
        "ok": passed,
        "version": crate::server::VERSION,
        "config": path,
        "checks": checks,
        "summary": {"ok": good, "warn": warn, "fail": ok},
    });

    if json {
        crate::commands::emit_json(value);
    } else {
        for row in &rows {
            println!("{:<5} {:<32} {}", row.status, row.name, row.detail);
        }
    }

    Ok(if passed {
        0
    } else {
        UmspError::other("DOCTOR", "doctor found failures").exit
    })
}
