//! `umsp provider|model|client|key` — comment-preserving registry edits.

use std::path::Path;

use serde_json::{Value, json};
use toml_edit::Table;

use crate::cli::{
    CatalogCmd, ClientAddArgs, ClientCmd, KeyCmd, ModelAddArgs, ModelBackendCmd, ModelCmd,
    ProviderAddArgs, ProviderCmd, ProviderTestArgs, VendorCmd,
};
use crate::commands::config_edit::{self, ConfigDoc};
use crate::config::{Config, PROTOCOLS};
use crate::error::{EXIT_USAGE, Result, UmspError};
use crate::keys;
use crate::registry;
use crate::util;

fn after_edit(path: &Path, no_reload: bool) -> Result<Config> {
    let cfg = Config::load(path)?;
    if !no_reload {
        crate::server::reload_running_server(&cfg);
    }
    Ok(cfg)
}

pub fn run_provider(path: &Path, cmd: ProviderCmd, json: bool, no_reload: bool) -> Result<i32> {
    match cmd {
        ProviderCmd::List => {
            let cfg = crate::commands::load_config(path)?;
            let env = crate::envfile::EnvFile::load(&Config::resolve_env_path());
            let health = fetch_health(&cfg);
            let rows: Vec<Value> = cfg
                .providers
                .iter()
                .map(|p| {
                    let live = health
                        .as_ref()
                        .and_then(|h| h.get("providers"))
                        .and_then(|providers| providers.get(&p.id));
                    let sessions = crate::config::sessions_dir(path);
                    let key_present = if p.uses_oauth() {
                        crate::sessions::session_ready(&sessions, &p.id)
                    } else {
                        env.has(&p.api_key_env)
                    };
                    let mut row = json!({
                        "id": p.id,
                        "vendor": p.vendor,
                        "base_url": p.base_url,
                        "anthropic_base_url": p.anthropic_base_url,
                        "auth": p.auth,
                        "api_key_env": if p.uses_oauth() { Value::Null } else { json!(p.api_key_env) },
                        "key_present": key_present,
                        "protocols": p.protocols,
                        "quirks": p.quirks,
                        "models": cfg.models.iter().filter(|m| m.backends.iter().any(|b| b.provider == p.id)).count(),
                        "circuit": Value::Null,
                        "failures": Value::Null,
                        "opened_at": Value::Null,
                        "reason": Value::Null,
                        "last_status": Value::Null,
                        "last_error": Value::Null,
                        "last_latency_ms": Value::Null,
                    });
                    if let Some(live) = live {
                        for key in [
                            "circuit",
                            "failures",
                            "opened_at",
                            "reason",
                            "last_status",
                            "last_error",
                            "last_latency_ms",
                        ] {
                            if let Some(value) = live.get(key) {
                                row[key] = value.clone();
                            }
                        }
                    }
                    row
                })
                .collect();
            let gateway = json!({
                "running": health.is_some(),
                "status": health.as_ref().and_then(|h| h.get("status")).and_then(Value::as_str),
                "version": health.as_ref().and_then(|h| h.get("version")).and_then(Value::as_str),
                "uptime_s": health.as_ref().and_then(|h| h.get("uptime_s")).and_then(Value::as_u64),
            });
            if json {
                crate::commands::emit_json(json!({
                    "ok": true,
                    "gateway": gateway,
                    "providers": rows
                }));
            } else if rows.is_empty() {
                println!("no providers configured");
            } else {
                if health.is_none() {
                    println!(
                        "gateway: not running ({})",
                        health_url(&cfg.server.listen).unwrap_or_else(|| "-".into())
                    );
                }
                for row in &rows {
                    let circuit = row["circuit"].as_str().unwrap_or("-");
                    let failures = row["failures"]
                        .as_u64()
                        .map(|n| n.to_string())
                        .unwrap_or_else(|| "-".into());
                    let last = row["last_status"]
                        .as_u64()
                        .map(|n| n.to_string())
                        .unwrap_or_else(|| "-".into());
                    println!(
                        "{:<20} {:<13} {:<38} key:{:<7} env:{:<28} circuit:{:<9} fails:{:<3} last:{:<4} models:{:<3} quirks:{}",
                        row["id"].as_str().unwrap_or("-"),
                        row["vendor"].as_str().unwrap_or("-"),
                        row["base_url"].as_str().unwrap_or("-"),
                        if row["key_present"].as_bool().unwrap_or(false) {
                            "yes"
                        } else {
                            "MISSING"
                        },
                        if row["auth"] == "oauth" {
                            "session"
                        } else {
                            row["api_key_env"].as_str().unwrap_or("-")
                        },
                        circuit,
                        failures,
                        last,
                        row["models"],
                        row["quirks"].as_str().unwrap_or("-"),
                    );
                }
            }
            Ok(0)
        }
        ProviderCmd::Add(args) => add_provider(path, args, json, no_reload),
        ProviderCmd::Remove { id } => {
            let mut doc = ConfigDoc::load(path)?;
            if !config_edit::remove_from_table_array(&mut doc.doc, "providers", "id", &id) {
                return Err(UmspError::not_found(format!("provider '{id}' not found")));
            }
            doc.save()?;
            after_edit(path, no_reload)?;
            if let Err(err) =
                crate::sessions::remove_session(&crate::config::sessions_dir(path), &id)
            {
                eprintln!(
                    "warning: provider '{id}' was removed but its session file could not be deleted: {}",
                    err.message
                );
            }
            crate::commands::emit(json, json!({"removed": id}), || {
                format!("removed provider {id}")
            })
        }
        ProviderCmd::Test(args) => provider_test(path, args, json),
    }
}

fn slugify(value: &str) -> String {
    let mut out = String::new();
    let mut last_dash = false;
    for ch in value.chars() {
        if ch.is_ascii_alphanumeric() {
            out.push(ch.to_ascii_lowercase());
            last_dash = false;
        } else if !last_dash {
            out.push('-');
            last_dash = true;
        }
    }
    out.trim_matches('-').to_string()
}

/// Providers are independent identities: an id is never reused, so adding the
/// same vendor twice yields `deepseek`, `deepseek-2`, …
fn unique_provider_id(cfg: &Config, base: &str) -> String {
    let base = {
        let slug = slugify(base);
        if slug.is_empty() {
            "provider".to_string()
        } else {
            slug
        }
    };
    if cfg.provider(&base).is_none() {
        return base;
    }
    for n in 2..10_000 {
        let candidate = format!("{base}-{n}");
        if cfg.provider(&candidate).is_none() {
            return candidate;
        }
    }
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    format!("{base}-{nanos}")
}

fn env_name_for(id: &str) -> String {
    let mut out = String::new();
    for ch in id.chars() {
        if ch.is_ascii_alphanumeric() {
            out.push(ch.to_ascii_uppercase());
        } else {
            out.push('_');
        }
    }
    format!("{out}_API_KEY")
}

fn add_provider(path: &Path, args: ProviderAddArgs, json: bool, no_reload: bool) -> Result<i32> {
    if !path.exists() {
        return Err(UmspError::config(format!(
            "{} does not exist; run `umsp init` first",
            path.display()
        )));
    }
    let Some(vendor) = crate::vendors::find(&args.vendor) else {
        return Err(UmspError::usage(format!(
            "unknown vendor '{}'; supported: {} (see `umsp vendor list`)",
            args.vendor,
            crate::vendors::ids().join(", ")
        )));
    };

    let existing = Config::load(path)?;
    let raw = args.id.clone().unwrap_or_else(|| vendor.id.to_string());
    let requested = {
        let slug = slugify(&raw);
        if slug.is_empty() {
            "provider".to_string()
        } else {
            slug
        }
    };
    let id = unique_provider_id(&existing, &requested);
    let renamed_from = (id != requested).then(|| requested.clone());
    let oauth = vendor.auth == crate::config::AUTH_OAUTH;
    if oauth && args.api_key_env.is_some() {
        return Err(UmspError::usage(
            "oauth providers do not take --api-key-env; each account signs in with `umsp login <id>`",
        ));
    }
    let api_key_env = if oauth {
        String::new()
    } else {
        args.api_key_env
            .clone()
            .unwrap_or_else(|| env_name_for(&id))
    };

    let new_key = if oauth || args.no_key {
        None
    } else {
        let secret =
            crate::commands::env_cmd::read_secret(&format!("API key for {api_key_env}: "))?
                .trim()
                .to_string();
        if secret.is_empty() {
            return Err(UmspError::usage(
                "the API key must not be empty (use --no-key to add without one)",
            ));
        }
        Some(secret)
    };

    let protocols: Vec<String> = vendor.protocols.iter().map(|p| (*p).to_string()).collect();
    let mut doc = ConfigDoc::load(path)?;
    let mut table = Table::new();
    table.insert("id", config_edit::str_item(&id));
    table.insert("vendor", config_edit::str_item(vendor.id));
    table.insert("base_url", config_edit::str_item(vendor.base_url));
    if let Some(anthropic) = vendor.anthropic_base_url {
        table.insert("anthropic_base_url", config_edit::str_item(anthropic));
    }
    if oauth {
        table.insert("auth", config_edit::str_item(crate::config::AUTH_OAUTH));
    } else {
        table.insert("api_key_env", config_edit::str_item(&api_key_env));
    }
    table.insert("protocols", config_edit::str_array_item(&protocols));
    if let Some(quirks) = vendor.quirks {
        table.insert("quirks", config_edit::str_item(quirks));
    }
    doc.ensure_table("providers").push(table);
    doc.save()?;
    after_edit(path, no_reload)?;
    if let Some(key) = &new_key {
        crate::envfile::EnvFile::set(&Config::resolve_env_path(), &api_key_env, key)?;
    }

    let value = json!({
        "provider": {
            "id": id,
            "renamed_from": renamed_from,
            "vendor": vendor.id,
            "vendor_name": vendor.name,
            "base_url": vendor.base_url,
            "anthropic_base_url": vendor.anthropic_base_url,
            "auth": if oauth { crate::config::AUTH_OAUTH } else { crate::config::AUTH_API_KEY },
            "api_key_env": if oauth { Value::Null } else { json!(api_key_env) },
            "protocols": protocols,
            "quirks": vendor.quirks,
            "key_stored": new_key.is_some(),
        }
    });
    crate::commands::emit(json, value, || {
        let renamed = match &renamed_from {
            Some(old) => format!(" (id '{old}' was taken; each provider is independent)"),
            None => String::new(),
        };
        let login = if oauth {
            format!(". Sign in this account with `umsp login {id}`")
        } else {
            String::new()
        };
        format!(
            "added provider {id} from vendor {} ({}){renamed}{login}",
            vendor.id, vendor.base_url
        )
    })
}

pub fn run_vendor(path: &Path, cmd: VendorCmd, json: bool) -> Result<i32> {
    match cmd {
        VendorCmd::List => {
            let cfg = Config::load(path).ok();
            let rows: Vec<Value> = crate::vendors::VENDORS
                .iter()
                .map(|vendor| {
                    let configured = cfg
                        .as_ref()
                        .map(|cfg| {
                            cfg.providers
                                .iter()
                                .filter(|p| p.vendor.as_deref() == Some(vendor.id))
                                .count()
                        })
                        .unwrap_or(0);
                    json!({
                        "id": vendor.id,
                        "name": vendor.name,
                        "base_url": vendor.base_url,
                        "anthropic_base_url": vendor.anthropic_base_url,
                        "protocols": vendor.protocols,
                        "quirks": vendor.quirks,
                        "auth": vendor.auth,
                        "docs_url": vendor.docs_url,
                        "configured": configured,
                    })
                })
                .collect();
            if json {
                crate::commands::emit_json(json!({"ok": true, "vendors": rows}));
            } else {
                for row in &rows {
                    println!(
                        "{:<14} {:<12} {:<38} protocols:{:<24} quirks:{:<12} configured:{}",
                        row["id"].as_str().unwrap_or("-"),
                        row["name"].as_str().unwrap_or("-"),
                        row["base_url"].as_str().unwrap_or("-"),
                        row["protocols"]
                            .as_array()
                            .map(|p| p
                                .iter()
                                .filter_map(|v| v.as_str())
                                .collect::<Vec<_>>()
                                .join(","))
                            .unwrap_or_default(),
                        row["quirks"].as_str().unwrap_or("-"),
                        row["configured"],
                    );
                    if let Some(docs) = row["docs_url"].as_str() {
                        println!("               docs: {docs}");
                    }
                }
            }
            Ok(0)
        }
    }
}

/// `/healthz` lives on the loopback listener; turn the configured bind address
/// into a URL the CLI can reach even when the server binds 0.0.0.0.
fn health_url(listen: &str) -> Option<String> {
    let (host, port) = listen.rsplit_once(':')?;
    let host = host.trim_start_matches('[').trim_end_matches(']');
    let host = match host {
        "" | "0.0.0.0" | "::" => "127.0.0.1",
        other => other,
    };
    let host = if host.contains(':') {
        format!("[{host}]")
    } else {
        host.to_string()
    };
    Some(format!("http://{host}:{port}/healthz"))
}

pub fn run_catalog(path: &Path, cmd: CatalogCmd, json_output: bool) -> Result<i32> {
    match cmd {
        CatalogCmd::Refresh => refresh_running_catalog(path, json_output),
    }
}

fn refresh_running_catalog(path: &Path, json_output: bool) -> Result<i32> {
    let cfg = Config::load(path)?;
    let url = gateway_url(&cfg.server.listen, "/catalog/refresh").ok_or_else(|| {
        UmspError::config(format!("cannot parse listen address {}", cfg.server.listen))
    })?;
    let _ = rustls::crypto::ring::default_provider().install_default();
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|err| UmspError::other("RUNTIME", format!("tokio: {err}")))?;
    let value = rt.block_on(async move {
        let client = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(90))
            .build()
            .map_err(|err| UmspError::network(format!("http client: {err}")))?;
        let response = client.post(&url).send().await.map_err(|_| {
            UmspError::network("gateway is not running; start it with `umsp serve`")
        })?;
        let status = response.status();
        let body = response.json::<Value>().await.map_err(|err| {
            UmspError::network(format!("catalog refresh returned invalid JSON: {err}"))
        })?;
        if !status.is_success() {
            return Err(UmspError::network(format!(
                "catalog refresh failed with HTTP {status}"
            )));
        }
        Ok::<Value, UmspError>(body)
    })?;
    if json_output {
        println!(
            "{}",
            serde_json::to_string(&value).unwrap_or_else(|_| "{}".into())
        );
    } else {
        println!(
            "{}",
            serde_json::to_string_pretty(&value).unwrap_or_else(|_| value.to_string())
        );
    }
    Ok(0)
}

fn gateway_url(listen: &str, path: &str) -> Option<String> {
    let health = health_url(listen)?;
    Some(format!("{}{path}", health.trim_end_matches("/healthz")))
}

/// Circuit state only lives in the gateway process; ask it, and treat a dead
/// gateway as "status unknown" instead of failing `provider list`.
fn fetch_health(cfg: &Config) -> Option<Value> {
    let url = health_url(&cfg.server.listen)?;
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .ok()?;
    // reqwest builds its TLS config eagerly; install ring once (idempotent).
    let _ = rustls::crypto::ring::default_provider().install_default();
    rt.block_on(async move {
        let client = reqwest::Client::builder()
            .timeout(std::time::Duration::from_millis(400))
            .build()
            .ok()?;
        client
            .get(url)
            .send()
            .await
            .ok()?
            .json::<Value>()
            .await
            .ok()
    })
}

fn provider_test(path: &Path, args: ProviderTestArgs, json: bool) -> Result<i32> {
    let cfg = crate::commands::load_config(path)?;
    if cfg.provider(&args.id).is_none() {
        return Err(UmspError::not_found(format!(
            "provider '{}' not found",
            args.id
        )));
    }
    let report = crate::commands::run_cmds::probe_blocking(
        &cfg,
        &crate::config::sessions_dir(path),
        &args.id,
        &args.protocol,
        None,
    )?;
    let value = json!({
        "provider": report.provider,
        "model": report.model,
        "protocol": args.protocol,
        "status": report.status,
        "latency_ms": report.latency_ms,
        "error": report.error,
    });
    let code = if report.ok {
        0
    } else {
        match report.status {
            Some(401 | 402 | 403 | 429) => crate::error::EXIT_UPSTREAM,
            Some(_) => crate::error::EXIT_OTHER,
            None => crate::error::EXIT_NETWORK,
        }
    };
    crate::commands::emit(json, value, || {
        format!(
            "{} {} status={} in {} ms{}",
            report.provider,
            report.model,
            report
                .status
                .map(|s| s.to_string())
                .unwrap_or_else(|| "-".into()),
            report.latency_ms,
            report.error.map(|e| format!(" ({e})")).unwrap_or_default()
        )
    })?;
    Ok(code)
}

pub fn run_model(path: &Path, cmd: ModelCmd, json: bool, no_reload: bool) -> Result<i32> {
    match cmd {
        ModelCmd::List { aliases } => {
            let cfg = crate::commands::load_config(path)?;
            let rows: Vec<Value> = cfg
                .models
                .iter()
                .map(|model| {
                    let mut row = registry::model_payload(model);
                    row["backends"] = json!(
                        model
                            .backends
                            .iter()
                            .map(|b| json!({"provider": b.provider, "upstream": b.upstream}))
                            .collect::<Vec<_>>()
                    );
                    row["aliases"] = json!(model.aliases);
                    row
                })
                .collect();
            let alias_rows: Vec<Value> = if aliases {
                registry::alias_table(&cfg)
                    .into_iter()
                    .map(|(alias, canonical)| json!({"alias": alias, "model": canonical}))
                    .collect()
            } else {
                Vec::new()
            };
            if json {
                let mut payload = json!({"ok": true, "models": rows});
                if aliases {
                    payload["aliases"] = json!(alias_rows);
                }
                crate::commands::emit_json(payload);
            } else if rows.is_empty() {
                println!("no models configured");
            } else {
                for model in &cfg.models {
                    let backends: Vec<String> = model
                        .backends
                        .iter()
                        .map(|b| format!("{}:{}", b.provider, b.upstream))
                        .collect();
                    println!(
                        "{:<32} ctx:{:<9} max:{:<8} {:<24} backends: {}",
                        model.id,
                        model.context_length,
                        model.max_completion_tokens,
                        model.display_name.as_deref().unwrap_or("-"),
                        backends.join(", ")
                    );
                }
                for row in &alias_rows {
                    println!(
                        "alias {:<28} -> {}",
                        row["alias"].as_str().unwrap_or("-"),
                        row["model"].as_str().unwrap_or("-")
                    );
                }
            }
            Ok(0)
        }
        ModelCmd::Add(args) => add_model(path, *args, json, no_reload),
        ModelCmd::Backend { cmd } => run_model_backend(path, cmd, json, no_reload),
        ModelCmd::Remove { id } => {
            let mut doc = ConfigDoc::load(path)?;
            if !config_edit::remove_from_table_array(&mut doc.doc, "models", "id", &id) {
                return Err(UmspError::not_found(format!("model '{id}' not found")));
            }
            doc.save()?;
            after_edit(path, no_reload)?;
            crate::commands::emit(json, json!({"removed": id}), || {
                format!("removed model {id}")
            })
        }
    }
}

fn run_model_backend(
    path: &Path,
    cmd: ModelBackendCmd,
    json: bool,
    no_reload: bool,
) -> Result<i32> {
    match cmd {
        ModelBackendCmd::Add {
            model,
            backend,
            first,
        } => add_backend(path, &model, &backend, first, json, no_reload),
        ModelBackendCmd::Remove { model, provider } => {
            remove_backend(path, &model, &provider, json, no_reload)
        }
    }
}

fn parse_backend(spec: &str) -> Result<(String, String)> {
    let Some((provider, upstream)) = spec.split_once(':') else {
        return Err(UmspError::usage(format!(
            "backend '{spec}' must be provider:upstream"
        )));
    };
    let (provider, upstream) = (provider.trim(), upstream.trim());
    if provider.is_empty() || upstream.is_empty() {
        return Err(UmspError::usage(format!(
            "backend '{spec}' must be provider:upstream"
        )));
    }
    Ok((provider.to_string(), upstream.to_string()))
}

fn model_table<'a>(doc: &'a mut toml_edit::DocumentMut, id: &str) -> Option<&'a mut Table> {
    let models = doc.get_mut("models")?.as_array_of_tables_mut()?;
    models
        .iter_mut()
        .find(|table| table.get("id").and_then(toml_edit::Item::as_str) == Some(id))
}

fn backends_json(cfg: &Config, model_id: &str) -> Vec<Value> {
    cfg.model(model_id)
        .map(|model| {
            model
                .backends
                .iter()
                .map(|b| json!({"provider": b.provider, "upstream": b.upstream}))
                .collect()
        })
        .unwrap_or_default()
}

fn add_backend(
    path: &Path,
    model_id: &str,
    spec: &str,
    first: bool,
    json: bool,
    no_reload: bool,
) -> Result<i32> {
    let (provider, upstream) = parse_backend(spec)?;
    let cfg = crate::commands::load_config(path)?;
    let Some(model) = cfg.model(model_id) else {
        return Err(UmspError::not_found(format!(
            "model '{model_id}' not found"
        )));
    };
    if cfg.provider(&provider).is_none() {
        return Err(UmspError::not_found(format!(
            "provider '{provider}' not found; run `umsp provider add` first"
        )));
    }
    if model
        .backends
        .iter()
        .any(|b| b.provider == provider && b.upstream == upstream)
    {
        return Err(UmspError::new(
            "BACKEND_EXISTS",
            format!("model '{model_id}' already has backend {provider}:{upstream}"),
            EXIT_USAGE,
        ));
    }

    let mut doc = ConfigDoc::load(path)?;
    {
        let table = model_table(&mut doc.doc, model_id).ok_or_else(|| {
            UmspError::config(format!(
                "model '{model_id}' is missing from the config file"
            ))
        })?;
        match table.get_mut("backends") {
            Some(toml_edit::Item::Value(toml_edit::Value::Array(array))) => {
                let mut inline = toml_edit::InlineTable::new();
                inline.insert("provider", toml_edit::Value::from(provider.as_str()));
                inline.insert("upstream", toml_edit::Value::from(upstream.as_str()));
                let value = toml_edit::Value::InlineTable(inline);
                if first {
                    array.insert(0, value);
                } else {
                    array.push(value);
                }
            }
            Some(toml_edit::Item::ArrayOfTables(tables)) => {
                let mut entry = Table::new();
                entry.insert("provider", config_edit::str_item(&provider));
                entry.insert("upstream", config_edit::str_item(&upstream));
                if first {
                    tables.insert(0, entry);
                } else {
                    tables.push(entry);
                }
            }
            _ => {
                return Err(UmspError::config(format!(
                    "model '{model_id}' has no `backends` list to edit"
                )));
            }
        }
    }
    doc.save()?;
    let cfg = after_edit(path, no_reload)?;
    let rows = backends_json(&cfg, model_id);
    crate::commands::emit(json, json!({"model": model_id, "backends": rows}), || {
        format!("added backend {provider}:{upstream} to {model_id}")
    })
}

fn remove_backend(
    path: &Path,
    model_id: &str,
    provider: &str,
    json: bool,
    no_reload: bool,
) -> Result<i32> {
    let cfg = crate::commands::load_config(path)?;
    let Some(model) = cfg.model(model_id) else {
        return Err(UmspError::not_found(format!(
            "model '{model_id}' not found"
        )));
    };
    let hits = model
        .backends
        .iter()
        .filter(|b| b.provider == provider)
        .count();
    if hits == 0 {
        return Err(UmspError::not_found(format!(
            "model '{model_id}' has no backend from provider '{provider}'"
        )));
    }
    if hits == model.backends.len() {
        return Err(UmspError::usage(format!(
            "refusing to remove the last backend of '{model_id}'; add another one first"
        )));
    }

    let mut doc = ConfigDoc::load(path)?;
    {
        let table = model_table(&mut doc.doc, model_id).ok_or_else(|| {
            UmspError::config(format!(
                "model '{model_id}' is missing from the config file"
            ))
        })?;
        match table.get_mut("backends") {
            Some(toml_edit::Item::Value(toml_edit::Value::Array(array))) => {
                let mut keep = toml_edit::Array::new();
                for value in array.iter() {
                    let entry_provider = value
                        .as_inline_table()
                        .and_then(|t| t.get("provider"))
                        .and_then(|v| v.as_str());
                    if entry_provider != Some(provider) {
                        keep.push(value.clone());
                    }
                }
                *array = keep;
            }
            Some(toml_edit::Item::ArrayOfTables(tables)) => {
                let mut index = 0;
                while index < tables.len() {
                    let entry_provider = tables
                        .get(index)
                        .and_then(|t| t.get("provider"))
                        .and_then(toml_edit::Item::as_str);
                    if entry_provider == Some(provider) {
                        tables.remove(index);
                    } else {
                        index += 1;
                    }
                }
            }
            _ => {
                return Err(UmspError::config(format!(
                    "model '{model_id}' has no `backends` list to edit"
                )));
            }
        }
    }
    doc.save()?;
    let cfg = after_edit(path, no_reload)?;
    let rows = backends_json(&cfg, model_id);
    crate::commands::emit(json, json!({"model": model_id, "backends": rows}), || {
        format!("removed provider '{provider}' from {model_id}")
    })
}

fn add_model(path: &Path, args: ModelAddArgs, json: bool, no_reload: bool) -> Result<i32> {
    if !path.exists() {
        return Err(UmspError::config(format!(
            "{} does not exist; run `umsp init` first",
            path.display()
        )));
    }
    let existing = Config::load(path)?;
    if existing.model(&args.id).is_some() {
        return Err(UmspError::new(
            "MODEL_EXISTS",
            format!("model '{}' already exists", args.id),
            EXIT_USAGE,
        ));
    }

    // `provider:upstream`; the order in which they are given is the priority.
    let mut parsed: Vec<(String, String)> = Vec::new();
    for raw in &args.backends {
        let Some((provider, upstream)) = raw.split_once(':') else {
            return Err(UmspError::usage(format!(
                "backend '{raw}' must be provider:upstream"
            )));
        };
        let (provider, upstream) = (provider.trim(), upstream.trim());
        if provider.is_empty() || upstream.is_empty() {
            return Err(UmspError::usage(format!(
                "backend '{raw}' must be provider:upstream"
            )));
        }
        if existing.provider(provider).is_none() {
            return Err(UmspError::not_found(format!(
                "provider '{provider}' not found; add it first"
            )));
        }
        parsed.push((provider.to_string(), upstream.to_string()));
    }

    let mut doc = ConfigDoc::load(path)?;
    let mut table = Table::new();
    table.insert("id", config_edit::str_item(&args.id));

    let mut backends = toml_edit::Array::new();
    for (provider, upstream) in &parsed {
        let mut inline = toml_edit::InlineTable::new();
        inline.insert("provider", toml_edit::Value::from(provider.as_str()));
        inline.insert("upstream", toml_edit::Value::from(upstream.as_str()));
        backends.push(toml_edit::Value::InlineTable(inline));
    }
    table.insert("backends", toml_edit::value(backends));
    if !args.aliases.is_empty() {
        table.insert("aliases", config_edit::str_array_item(&args.aliases));
    }
    if let Some(ctx) = args.context_length {
        table.insert("context_length", config_edit::int_item(ctx as i64));
    }
    if let Some(max) = args.max_completion_tokens {
        table.insert("max_completion_tokens", config_edit::int_item(max as i64));
    }
    if !args.capabilities.is_empty() {
        table.insert(
            "capabilities",
            config_edit::str_array_item(&args.capabilities),
        );
    }
    if let Some(display) = &args.display_name {
        table.insert("display_name", config_edit::str_item(display));
    }
    if let Some(description) = &args.description {
        table.insert("description", config_edit::str_item(description));
    }
    table.insert("created", config_edit::int_item(args.created));
    doc.ensure_table("models").push(table);
    doc.save()?;
    after_edit(path, no_reload)?;

    let backends_json: Vec<Value> = parsed
        .iter()
        .map(|(provider, upstream)| json!({"provider": provider, "upstream": upstream}))
        .collect();
    let value = json!({"model": {"id": args.id, "backends": backends_json}});
    crate::commands::emit(json, value, || format!("added model {}", args.id))
}

pub fn run_client(path: &Path, cmd: ClientCmd, json: bool, no_reload: bool) -> Result<i32> {
    match cmd {
        ClientCmd::List => client_list(path, json),
        ClientCmd::Add(args) => client_add(path, args, json, no_reload),
        ClientCmd::Remove { name } => client_remove(path, &name, json, no_reload),
        ClientCmd::Rotate { name } => client_rotate(path, &name, json, no_reload),
        ClientCmd::Unbind { name } => client_unbind(path, &name, json, no_reload),
    }
}

pub fn run_key(path: &Path, cmd: KeyCmd, json: bool, no_reload: bool) -> Result<i32> {
    match cmd {
        KeyCmd::List => client_list(path, json),
        KeyCmd::Add(args) => client_add(path, args, json, no_reload),
        KeyCmd::Revoke { name } => client_remove(path, &name, json, no_reload),
        KeyCmd::Unbind { name } => client_unbind(path, &name, json, no_reload),
    }
}

fn client_list(path: &Path, json: bool) -> Result<i32> {
    let cfg = crate::commands::load_config(path)?;
    let bindings = crate::bindings::BindingStore::load(&cfg.server.state_path());
    let rows: Vec<Value> = cfg
        .clients
        .iter()
        .map(|c| {
            json!({
                "name": c.name,
                "key_prefix": c.key_prefix,
                "bound_ip": bindings.bound_ip(&c.name),
                "allowed_models": if c.allowed_models.is_empty() { vec!["*".to_string()] } else { c.allowed_models.clone() },
                "allowed_protocols": c.allowed_protocols,
            })
        })
        .collect();
    if json {
        crate::commands::emit_json(json!({"ok": true, "clients": rows}));
    } else if rows.is_empty() {
        println!("no clients configured");
    } else {
        for row in &rows {
            println!(
                "{:<16} {:<20} bound:{:<16} models:{:<24} protocols:{}",
                row["name"].as_str().unwrap_or("-"),
                row["key_prefix"].as_str().unwrap_or("-"),
                row["bound_ip"].as_str().unwrap_or("-"),
                row["allowed_models"]
                    .as_array()
                    .map(|a| a
                        .iter()
                        .filter_map(|v| v.as_str())
                        .collect::<Vec<_>>()
                        .join(","))
                    .unwrap_or_default(),
                row["allowed_protocols"]
                    .as_array()
                    .map(|a| a
                        .iter()
                        .filter_map(|v| v.as_str())
                        .collect::<Vec<_>>()
                        .join(","))
                    .unwrap_or_default(),
            );
        }
    }
    Ok(0)
}

fn client_add(path: &Path, args: ClientAddArgs, json: bool, no_reload: bool) -> Result<i32> {
    if !path.exists() {
        return Err(UmspError::config(format!(
            "{} does not exist; run `umsp init` first",
            path.display()
        )));
    }
    let existing = Config::load(path)?;
    if existing.client(&args.name).is_some() {
        return Err(UmspError::new(
            "CLIENT_EXISTS",
            format!(
                "client '{}' already exists (use `umsp client rotate`)",
                args.name
            ),
            EXIT_USAGE,
        ));
    }
    let models: Vec<String> = args
        .models
        .as_deref()
        .map(util::parse_list)
        .filter(|v| !v.is_empty())
        .unwrap_or_else(|| vec!["*".to_string()]);
    let protocols = util::parse_list(&args.protocols);
    for protocol in &protocols {
        if !PROTOCOLS.contains(&protocol.as_str()) {
            return Err(UmspError::usage(format!("unknown protocol '{protocol}'")));
        }
    }

    let plaintext = keys::generate_key();
    let hash = keys::hash_key(&plaintext);
    let prefix = keys::key_prefix(&plaintext);

    let mut doc = ConfigDoc::load(path)?;
    let mut table = Table::new();
    table.insert("name", config_edit::str_item(&args.name));
    table.insert("key_hash", config_edit::str_item(&hash));
    table.insert("key_prefix", config_edit::str_item(&prefix));
    table.insert("allowed_models", config_edit::str_array_item(&models));
    table.insert("allowed_protocols", config_edit::str_array_item(&protocols));
    doc.ensure_table("clients").push(table);
    doc.save()?;
    after_edit(path, no_reload)?;

    let value = json!({
        "client": {
            "name": args.name,
            "key": plaintext,
            "key_prefix": prefix,
            "allowed_models": models,
            "allowed_protocols": protocols,
        }
    });
    if json {
        crate::commands::emit_json(crate::server::success_json(value));
    } else {
        println!("client {} created", args.name);
        println!("key (shown once, store it now): {plaintext}");
    }
    Ok(0)
}

fn client_remove(path: &Path, name: &str, json: bool, no_reload: bool) -> Result<i32> {
    let mut doc = ConfigDoc::load(path)?;
    if !config_edit::remove_from_table_array(&mut doc.doc, "clients", "name", name) {
        return Err(UmspError::not_found(format!("client '{name}' not found")));
    }
    doc.save()?;
    // A re-added client with the same name must not inherit the old binding.
    let cfg = Config::load(path)?;
    crate::bindings::BindingStore::load(&cfg.server.state_path())
        .unbind(name)
        .map_err(|e| UmspError::other("BINDING", format!("could not clear device binding: {e}")))?;
    after_edit(path, no_reload)?;
    crate::commands::emit(json, json!({"removed": name}), || {
        format!("revoked client {name}")
    })
}

fn client_rotate(path: &Path, name: &str, json: bool, no_reload: bool) -> Result<i32> {
    let existing = Config::load(path)?;
    if existing.client(name).is_none() {
        return Err(UmspError::not_found(format!("client '{name}' not found")));
    }
    let plaintext = keys::generate_key();
    let hash = keys::hash_key(&plaintext);
    let prefix = keys::key_prefix(&plaintext);

    let mut doc = ConfigDoc::load(path)?;
    let updated_hash = config_edit::set_field_in_table_array(
        &mut doc.doc,
        "clients",
        "name",
        name,
        "key_hash",
        config_edit::str_item(&hash),
    );
    let updated_prefix = config_edit::set_field_in_table_array(
        &mut doc.doc,
        "clients",
        "name",
        name,
        "key_prefix",
        config_edit::str_item(&prefix),
    );
    if !updated_hash || !updated_prefix {
        return Err(UmspError::other(
            "ROTATE",
            "client entry could not be updated",
        ));
    }
    doc.save()?;
    // New key, new identity: the old device binding must not carry over.
    crate::bindings::BindingStore::load(&existing.server.state_path())
        .unbind(name)
        .map_err(|e| UmspError::other("BINDING", format!("could not clear device binding: {e}")))?;
    after_edit(path, no_reload)?;

    let value = json!({"client": {"name": name, "key": plaintext, "key_prefix": prefix}});
    if json {
        crate::commands::emit_json(crate::server::success_json(value));
    } else {
        println!("client {name} rotated");
        println!("new key (shown once): {plaintext}");
    }
    Ok(0)
}

fn client_unbind(path: &Path, name: &str, json: bool, no_reload: bool) -> Result<i32> {
    let cfg = Config::load(path)?;
    if cfg.client(name).is_none() {
        return Err(UmspError::not_found(format!("client '{name}' not found")));
    }
    let store = crate::bindings::BindingStore::load(&cfg.server.state_path());
    let was_bound = store
        .unbind(name)
        .map_err(|e| UmspError::other("BINDING", format!("could not clear device binding: {e}")))?;
    after_edit(path, no_reload)?;
    crate::commands::emit(
        json,
        json!({"unbound": name, "was_bound": was_bound}),
        || {
            if was_bound {
                format!("cleared device binding for {name}; the next request re-binds")
            } else {
                format!("client {name} was not bound")
            }
        },
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::CONFIG_TEMPLATE;
    use std::path::PathBuf;

    fn temp_config(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("umsp-reg-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join(format!("{tag}.toml"));
        std::fs::write(&path, CONFIG_TEMPLATE).unwrap();
        path
    }

    fn backends(path: &Path, model: &str) -> Vec<String> {
        Config::load(path)
            .unwrap()
            .model(model)
            .unwrap()
            .backends
            .iter()
            .map(|b| format!("{}:{}", b.provider, b.upstream))
            .collect()
    }

    #[test]
    fn health_url_targets_loopback() {
        assert_eq!(
            health_url("127.0.0.1:3460").as_deref(),
            Some("http://127.0.0.1:3460/healthz")
        );
        assert_eq!(
            health_url("0.0.0.0:9000").as_deref(),
            Some("http://127.0.0.1:9000/healthz")
        );
        assert_eq!(
            health_url("[::1]:3460").as_deref(),
            Some("http://[::1]:3460/healthz")
        );
        assert_eq!(health_url("nonsense"), None);
    }

    #[test]
    fn dead_gateway_is_reported_as_unknown() {
        let mut cfg = Config::load(&temp_config("health-dead")).unwrap();
        cfg.server.listen = "127.0.0.1:1".into();
        assert!(fetch_health(&cfg).is_none());
    }

    #[test]
    fn env_names_follow_the_instance_id() {
        assert_eq!(env_name_for("deepseek"), "DEEPSEEK_API_KEY");
        assert_eq!(env_name_for("deepseek-2"), "DEEPSEEK_2_API_KEY");
        assert_eq!(env_name_for("open-design"), "OPEN_DESIGN_API_KEY");
    }

    #[test]
    fn every_add_gets_a_distinct_id() {
        let path = temp_config("provider-ids");
        let cfg = Config::load(&path).unwrap();
        assert_eq!(unique_provider_id(&cfg, "deepseek"), "deepseek-2");
        assert_eq!(unique_provider_id(&cfg, "open-design"), "open-design-2");
        assert_eq!(
            unique_provider_id(&cfg, "deepseek-personal"),
            "deepseek-personal"
        );
    }

    #[test]
    fn backend_add_appends_and_first_prepends() {
        let path = temp_config("backend-order");
        add_backend(
            &path,
            "claude-haiku-5.5",
            "deepseek:deepseek-flash",
            false,
            true,
            true,
        )
        .unwrap();
        assert_eq!(
            backends(&path, "claude-haiku-5.5"),
            vec!["open-design:claude-haiku-5.5", "deepseek:deepseek-flash"]
        );
        add_backend(
            &path,
            "claude-haiku-5.5",
            "deepseek:deepseek-chat",
            true,
            true,
            true,
        )
        .unwrap();
        assert_eq!(
            backends(&path, "claude-haiku-5.5")[0],
            "deepseek:deepseek-chat"
        );
    }

    #[test]
    fn backend_add_rejects_duplicates_and_unknown_providers() {
        let path = temp_config("backend-dup");
        let err = add_backend(
            &path,
            "claude-haiku-5.5",
            "open-design:claude-haiku-5.5",
            false,
            true,
            true,
        )
        .unwrap_err();
        assert_eq!(err.code, "BACKEND_EXISTS");

        let err = add_backend(&path, "claude-haiku-5.5", "nope:x", false, true, true).unwrap_err();
        assert_eq!(err.code, "NOT_FOUND");

        let err = add_backend(
            &path,
            "claude-haiku-5.5",
            "not-a-backend",
            false,
            true,
            true,
        )
        .unwrap_err();
        assert_eq!(err.code, "USAGE");
    }

    #[test]
    fn backend_remove_never_leaves_a_model_without_backends() {
        let path = temp_config("backend-remove");
        let err = remove_backend(&path, "claude-haiku-5.5", "open-design", true, true).unwrap_err();
        assert!(err.message.contains("last backend"));

        add_backend(
            &path,
            "claude-haiku-5.5",
            "deepseek:deepseek-flash",
            false,
            true,
            true,
        )
        .unwrap();
        remove_backend(&path, "claude-haiku-5.5", "open-design", true, true).unwrap();
        assert_eq!(
            backends(&path, "claude-haiku-5.5"),
            vec!["deepseek:deepseek-flash"]
        );
    }
}
