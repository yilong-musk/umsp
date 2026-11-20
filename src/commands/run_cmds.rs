//! `umsp probe` and `umsp call` — direct upstream calls for smoke testing.

use std::path::Path;
use std::sync::Arc;
use std::time::Instant;

use axum::http::HeaderMap;
use serde_json::{Value, json};

use crate::cli::{CallArgs, ProbeArgs};
use crate::config::Config;
use crate::envfile::EnvFile;
use crate::error::{Result, UmspError};
use crate::server::{ProxyError, Upstream, resolve_target};
use crate::usage::Usage;

#[derive(Debug, Clone)]
pub struct ProbeReport {
    pub provider: String,
    pub model: String,
    pub status: Option<u16>,
    pub latency_ms: u64,
    pub ok: bool,
    pub error: Option<String>,
    pub usage: Option<Usage>,
}

fn runtime() -> Result<tokio::runtime::Runtime> {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|e| UmspError::other("RUNTIME", format!("tokio: {e}")))
}

pub fn probe_blocking(
    cfg: &Config,
    provider_id: &str,
    protocol: &str,
    model_id: Option<&str>,
) -> Result<ProbeReport> {
    let rt = runtime()?;
    rt.block_on(probe_async(cfg, provider_id, protocol, model_id))
}

pub async fn probe_async(
    cfg: &Config,
    provider_id: &str,
    protocol: &str,
    model_id: Option<&str>,
) -> Result<ProbeReport> {
    if protocol != "chat" {
        return Err(UmspError::usage(format!(
            "protocol '{protocol}' probes are not implemented in M0 (chat only)"
        )));
    }
    let provider = cfg
        .provider(provider_id)
        .ok_or_else(|| UmspError::not_found(format!("provider '{provider_id}' not found")))?;
    if !provider.supports("chat") {
        return Err(UmspError::usage(format!(
            "provider '{provider_id}' does not support the chat protocol"
        )));
    }
    let model = match model_id {
        Some(id) => cfg
            .models
            .iter()
            .find(|m| m.id == id && m.backends.iter().any(|b| b.provider == provider_id))
            .ok_or_else(|| {
                UmspError::not_found(format!(
                    "model '{id}' has no backend on provider '{provider_id}'"
                ))
            })?,
        None => cfg
            .models
            .iter()
            .find(|m| m.backends.iter().any(|b| b.provider == provider_id))
            .ok_or_else(|| {
                UmspError::not_found(format!(
                    "provider '{provider_id}' has no models; add one with `umsp model add`"
                ))
            })?,
    };
    let upstream = model
        .backends
        .iter()
        .find(|b| b.provider == provider_id)
        .map(|b| b.upstream.clone())
        .ok_or_else(|| {
            UmspError::not_found(format!(
                "no upstream configured for provider '{provider_id}'"
            ))
        })?;

    let env = EnvFile::load(&Config::resolve_env_path());
    let up = Upstream::new(Arc::new(cfg.clone()), env)?;
    let body = json!({
        "model": upstream,
        "messages": [{"role": "user", "content": "ping"}],
        "max_tokens": 1,
        "reasoning_effort": "none",
        "stream": false
    });
    let started = Instant::now();
    let outcome = up.chat(provider, &upstream, &body, &HeaderMap::new()).await;
    let latency_ms = started.elapsed().as_millis() as u64;

    match outcome {
        Ok(outcome) => Ok(ProbeReport {
            provider: provider.id.clone(),
            model: model.id.clone(),
            status: Some(outcome.status),
            latency_ms: outcome.duration_ms,
            ok: outcome.status < 400,
            error: (outcome.status >= 400).then(|| format!("http_{}", outcome.status)),
            usage: outcome.usage,
        }),
        Err(ProxyError::MissingKey { provider, env }) => Err(UmspError::config(format!(
            "provider '{provider}' is missing {env}; add it to ~/.config/umsp/env"
        ))),
        Err(err) => Ok(ProbeReport {
            provider: provider.id.clone(),
            model: model.id.clone(),
            status: None,
            latency_ms,
            ok: false,
            error: Some(err.code().to_string()),
            usage: None,
        }),
    }
}

pub fn run_probe(path: &Path, args: ProbeArgs, json: bool) -> Result<i32> {
    // M0 only speaks Chat upstream, so refuse other protocols at the CLI boundary
    // instead of sending a chat request labelled as something else.
    if args.protocol != "chat" {
        return Err(UmspError::usage(format!(
            "protocol '{}' is not implemented in M0 (chat only)",
            args.protocol
        )));
    }
    let cfg = crate::commands::load_config(path)?;
    let report = probe_blocking(&cfg, &args.provider, "chat", args.model.as_deref())?;
    let value = json!({
        "provider": report.provider,
        "model": report.model,
        "protocol": "chat",
        "status": report.status,
        "latency_ms": report.latency_ms,
        "error": report.error,
        "usage": report.usage,
    });
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
    Ok(if report.ok {
        0
    } else {
        match report.status {
            Some(401 | 402 | 403 | 429) => crate::error::EXIT_UPSTREAM,
            Some(_) => crate::error::EXIT_OTHER,
            None => crate::error::EXIT_NETWORK,
        }
    })
}

pub fn run_call(path: &Path, args: CallArgs, json: bool) -> Result<i32> {
    if args.stream {
        return Err(UmspError::usage(
            "--stream is not implemented in M0 (SSE lands in M1); omit the flag",
        ));
    }
    if args.protocol != "chat" {
        return Err(UmspError::usage(format!(
            "protocol '{}' is not implemented in M0 (chat only)",
            args.protocol
        )));
    }
    let cfg = crate::commands::load_config(path)?;
    let (model, provider, upstream) = resolve_target(&cfg, &args.model)?;
    let env = EnvFile::load(&Config::resolve_env_path());
    let up = Upstream::new(Arc::new(cfg.clone()), env)?;
    let body = json!({
        "model": upstream,
        "messages": [{"role": "user", "content": args.prompt}],
        "stream": false
    });

    let rt = runtime()?;
    let outcome = rt.block_on(async {
        up.chat(&provider, &upstream, &body, &HeaderMap::new())
            .await
    });

    match outcome {
        Ok(outcome) => {
            let parsed: Option<Value> = serde_json::from_slice(&outcome.body).ok();
            let content = parsed
                .as_ref()
                .and_then(|v| v.pointer("/choices/0/message/content"))
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string();
            let finish_reason = parsed
                .as_ref()
                .and_then(|v| v.pointer("/choices/0/finish_reason"))
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string();
            if outcome.status >= 400 {
                let raw = String::from_utf8_lossy(&outcome.body).to_string();
                if json {
                    crate::commands::emit_json(json!({
                        "ok": false,
                        "status": outcome.status,
                        "error": {"code": format!("upstream_{}", outcome.status), "message": raw},
                    }));
                } else {
                    eprintln!("{}", raw);
                }
                return Ok(match outcome.status {
                    401 | 402 | 403 | 429 => crate::error::EXIT_UPSTREAM,
                    400 | 404 | 422 => crate::error::EXIT_NOT_FOUND,
                    _ => crate::error::EXIT_OTHER,
                });
            }
            let value = json!({
                "model": model.id,
                "provider": provider.id,
                "status": outcome.status,
                "content": content,
                "finish_reason": finish_reason,
                "usage": outcome.usage,
                "duration_ms": outcome.duration_ms,
                "quirks": outcome.quirks,
            });
            crate::commands::emit(json, value, || content)?;
            Ok(0)
        }
        Err(ProxyError::MissingKey { provider, env }) => Err(UmspError::config(format!(
            "provider '{provider}' is missing {env}; add it to ~/.config/umsp/env"
        ))),
        Err(ProxyError::Timeout(msg)) => Err(UmspError::network(msg)),
        Err(ProxyError::Network(msg)) => Err(UmspError::network(msg)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cli::{CallArgs, ProbeArgs};

    fn probe_args(protocol: &str) -> ProbeArgs {
        ProbeArgs {
            provider: "od".into(),
            protocol: protocol.into(),
            model: None,
        }
    }

    /// Non-chat probes must fail as a usage error *before* the config is read or
    /// any upstream request is built: no network, no misleading `protocol` label.
    #[test]
    fn probe_rejects_non_chat_protocol_before_touching_config() {
        let err = run_probe(
            Path::new("/nonexistent/umsp/config.toml"),
            probe_args("responses"),
            true,
        )
        .expect_err("non-chat probe should be rejected");
        assert_eq!(err.code, "USAGE");
        assert_eq!(err.exit, crate::error::EXIT_USAGE);
        assert!(err.message.contains("chat only"), "{}", err.message);
    }

    #[test]
    fn call_rejects_non_chat_protocol_before_touching_config() {
        let args = CallArgs {
            model: "od/deepseek-v4.1-flash".into(),
            protocol: "messages".into(),
            prompt: "hi".into(),
            stream: false,
        };
        let err = run_call(Path::new("/nonexistent/umsp/config.toml"), args, true)
            .expect_err("non-chat call should be rejected");
        assert_eq!(err.code, "USAGE");
        assert!(err.message.contains("chat only"), "{}", err.message);
    }

    #[test]
    fn call_rejects_stream_in_m0() {
        let args = CallArgs {
            model: "od/deepseek-v4.1-flash".into(),
            protocol: "chat".into(),
            prompt: "hi".into(),
            stream: true,
        };
        let err = run_call(Path::new("/nonexistent/umsp/config.toml"), args, true)
            .expect_err("--stream should be rejected in M0");
        assert_eq!(err.code, "USAGE");
        assert!(err.message.contains("--stream"), "{}", err.message);
    }
}
