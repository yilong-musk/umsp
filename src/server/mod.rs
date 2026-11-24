//! HTTP data plane: auth, `/v1/models`, `/healthz`, Chat/Responses/Messages proxy.

mod background;
mod buffered;
mod client;
mod error;
mod failover;
mod handlers;
mod log;
mod prepare;
mod state;
mod stream_proxy;

use std::net::SocketAddr;

use axum::Router;
use axum::middleware;
use axum::routing::{any, get, head, post};
use chrono::Utc;
use serde_json::{Value, json};

use crate::config::{Config, Model, Provider};
use crate::envfile::EnvFile;
use crate::error::{Result, UmspError};
use crate::registry;

pub use client::{ChatOutcome, ChatStream, ProxyError, Upstream};
pub use error::{gateway_error, messages_error, protocol_gateway_error};
pub use log::plog;
pub use state::{AppState, ProviderHealth};

use background::health_loop;
use handlers::{
    admin_loopback_guard, catalog_refresh_handler, chat_handler, healthz_handler, hello_handler,
    messages_count_tokens_handler, messages_handler, metrics_handler, models_handler,
    responses_handler, responses_unsupported_handler,
};
use prepare::auth_middleware;

pub const VERSION: &str = env!("CARGO_PKG_VERSION");

// ---------------------------------------------------------------------------
// Router
// ---------------------------------------------------------------------------

pub fn build_router(state: AppState) -> Router {
    let api = Router::new()
        .route("/v1/models", get(models_handler))
        .route("/v1/chat/completions", post(chat_handler))
        .route("/v1/responses", post(responses_handler))
        .route("/v1/messages", post(messages_handler))
        .route(
            "/v1/messages/count_tokens",
            post(messages_count_tokens_handler),
        )
        // Retrieval, cancel, input_items and input_tokens are not proxied; a
        // protocol-native 501 beats a bare 404 (spec 2.2, unsupported path).
        .route("/v1/responses/{*rest}", any(responses_unsupported_handler))
        .route_layer(middleware::from_fn_with_state(
            state.clone(),
            auth_middleware,
        ));

    Router::new()
        .route("/api/hello", head(hello_handler))
        .merge(
            Router::new()
                .route("/healthz", get(healthz_handler))
                .route("/metrics", get(metrics_handler))
                .route("/catalog/refresh", post(catalog_refresh_handler))
                .route_layer(middleware::from_fn(admin_loopback_guard)),
        )
        .merge(api)
        .layer(axum::extract::DefaultBodyLimit::disable())
        .with_state(state)
}

pub async fn serve(config_path: &std::path::Path, listen_override: Option<String>) -> Result<()> {
    let cfg = Config::load_checked(config_path)?;
    let listen = listen_override
        .filter(|v| !v.trim().is_empty())
        .or_else(|| {
            std::env::var("UMSP_LISTEN")
                .ok()
                .filter(|v| !v.trim().is_empty())
        })
        .unwrap_or_else(|| cfg.server.listen.clone());
    let env_path = Config::resolve_env_path();
    let env = EnvFile::load(&env_path);
    let state = AppState::with_sessions(cfg, env, crate::config::sessions_dir(config_path))?;
    let app = build_router(state.clone());

    let listener = tokio::net::TcpListener::bind(&listen)
        .await
        .map_err(|e| UmspError::other("BIND", format!("cannot bind {listen}: {e}")))?;
    let local_addr = listener
        .local_addr()
        .map(|a| a.to_string())
        .unwrap_or_else(|_| listen.clone());

    // Held until `serve` returns; dropping it removes the pid file.
    let pid_lock = crate::pidfile::PidLock::acquire(&state.cfg().server.pid_file())?;

    println!(
        "{}",
        json!({"event": "ready", "addr": local_addr, "version": VERSION})
    );
    use std::io::Write;
    let _ = std::io::stdout().flush();

    // SIGHUP: atomically reload config, env keys, HTTP client, log store and
    // device bindings when valid; keep the previous snapshot otherwise.
    let reload_state = state.clone();
    let config_path_owned = config_path.to_path_buf();
    let env_path_owned = env_path.clone();
    let reload_task = tokio::spawn(async move {
        let mut hup = match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::hangup()) {
            Ok(sig) => sig,
            Err(_) => return,
        };
        while hup.recv().await.is_some() {
            match Config::load_checked(&config_path_owned) {
                Ok(cfg) => {
                    let issues = cfg.validate();
                    let errors = issues.iter().filter(|i| i.level == "error").count();
                    if errors == 0 {
                        let env = EnvFile::load(&env_path_owned);
                        match reload_state.reload(cfg, env) {
                            Ok(()) => {
                                plog("info", "config_reloaded", "-", None, None, None);
                            }
                            Err(err) => {
                                plog(
                                    "error",
                                    "config_reload_failed",
                                    "-",
                                    None,
                                    None,
                                    Some(&err.code),
                                );
                            }
                        }
                    } else {
                        plog(
                            "error",
                            "config_reload_rejected",
                            "-",
                            None,
                            None,
                            Some("CONFIG_INVALID"),
                        );
                    }
                }
                Err(err) => {
                    plog(
                        "error",
                        "config_reload_failed",
                        "-",
                        None,
                        None,
                        Some(&err.code),
                    );
                }
            }
        }
    });

    // Background circuit recovery (spec 3.4): spawned after the listener is up
    // and aborted as soon as serving stops, so it never delays shutdown.
    let health_task = tokio::spawn(health_loop(state.clone()));

    let shutdown = shutdown_signal();
    let result = axum::serve(
        listener,
        app.into_make_service_with_connect_info::<SocketAddr>(),
    )
    .with_graceful_shutdown(shutdown)
    .await;
    reload_task.abort();
    health_task.abort();
    let flush_state = state.clone();
    let _ = tokio::task::spawn_blocking(move || flush_state.flush_logs()).await;
    drop(pid_lock);
    result.map_err(|e| UmspError::other("SERVE", format!("server error: {e}")))
}

async fn shutdown_signal() {
    let ctrl_c = async {
        let _ = tokio::signal::ctrl_c().await;
    };
    let term = async {
        if let Ok(mut sig) =
            tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
        {
            sig.recv().await;
        }
    };
    tokio::select! {
        _ = ctrl_c => {},
        _ = term => {},
    }
}

/// Best-effort SIGHUP to a running `umsp serve` so CLI edits take effect.
///
/// Only a server that still holds the pid-file lock is signalled, so a stale
/// pid file can never deliver SIGHUP (whose default action is to terminate)
/// to an unrelated process that reused the pid.
pub fn reload_running_server(cfg: &Config) {
    let Some(pid) = crate::pidfile::locked_pid(&cfg.server.pid_file()) else {
        return;
    };
    if crate::pidfile::send_hangup(pid) {
        plog("info", "reload_signalled", "-", None, None, None);
    }
}

/// Helper used by tests and CLI JSON output.
pub fn success_json(value: Value) -> Value {
    let mut obj = serde_json::Map::new();
    obj.insert("ok".to_string(), Value::Bool(true));
    if let Value::Object(map) = value {
        for (k, v) in map {
            obj.insert(k, v);
        }
    } else {
        obj.insert("data".to_string(), value);
    }
    Value::Object(obj)
}

/// Convenience for CLI commands: resolve a model and its preferred backend.
pub fn resolve_target(cfg: &Config, requested: &str) -> Result<(Model, Provider, String)> {
    let model = registry::resolve(cfg, requested)
        .cloned()
        .ok_or_else(|| UmspError::not_found(format!("unknown model '{requested}'")))?;
    let backend = model
        .backends
        .first()
        .cloned()
        .ok_or_else(|| UmspError::not_found(format!("model '{}' has no backends", model.id)))?;
    let provider = cfg
        .provider(&backend.provider)
        .cloned()
        .ok_or_else(|| UmspError::not_found(format!("unknown provider '{}'", backend.provider)))?;
    Ok((model, provider, backend.upstream.clone()))
}

/// Timestamp helper for the CLI.
pub fn now_iso() -> String {
    Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
}

#[cfg(test)]
mod tests {
    use super::background::probe_provider_with_timeout;
    use super::error::{
        client_response_headers, error_type_for, mapped_upstream_status, replace_token,
    };
    use super::*;
    use axum::http::{HeaderName, HeaderValue, StatusCode};
    use std::time::{Duration, Instant};

    #[test]
    fn only_representation_headers_survive() {
        let headers = vec![
            (
                HeaderName::from_static("content-type"),
                HeaderValue::from_static("application/json"),
            ),
            (
                HeaderName::from_static("retry-after"),
                HeaderValue::from_static("3"),
            ),
            (
                HeaderName::from_static("server"),
                HeaderValue::from_static("open-design"),
            ),
            (
                HeaderName::from_static("x-request-id"),
                HeaderValue::from_static("vendor-trace"),
            ),
        ];
        let kept = client_response_headers(&headers);
        assert_eq!(kept.len(), 2);
        assert!(
            kept.iter()
                .all(|(name, _)| name == "content-type" || name == "retry-after")
        );
    }

    #[test]
    fn upstream_failures_map_to_gateway_statuses() {
        assert_eq!(mapped_upstream_status(400), 400);
        assert_eq!(mapped_upstream_status(404), 404);
        assert_eq!(mapped_upstream_status(401), 502);
        assert_eq!(mapped_upstream_status(402), 502);
        assert_eq!(mapped_upstream_status(403), 502);
        assert_eq!(mapped_upstream_status(429), 429);
        assert_eq!(mapped_upstream_status(503), 502);
        assert_eq!(mapped_upstream_status(200), 200);
    }

    #[test]
    fn replace_token_respects_word_boundaries() {
        assert_eq!(
            replace_token("a deepseek b", "deepseek", "umsp"),
            "a umsp b"
        );
        assert_eq!(
            replace_token("deepseek-v4.1-flash", "deepseek", "umsp"),
            "deepseek-v4.1-flash",
            "a provider id inside an upstream model id must stay untouched"
        );
        assert_eq!(replace_token("xdeepseek", "deepseek", "umsp"), "xdeepseek");
        assert_eq!(replace_token("deepseek.", "deepseek", "umsp"), "umsp.");
    }

    #[test]
    fn gateway_error_has_native_shape_and_code_header() {
        let resp = gateway_error(StatusCode::NOT_FOUND, "model_not_found", "nope", "rid1");
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
        assert_eq!(
            resp.headers().get("x-umsp-code").unwrap(),
            "model_not_found"
        );
        assert_eq!(resp.headers().get("x-umsp-request-id").unwrap(), "rid1");
    }

    #[test]
    fn error_type_mapping() {
        assert_eq!(
            error_type_for(StatusCode::UNAUTHORIZED),
            "authentication_error"
        );
        assert_eq!(error_type_for(StatusCode::BAD_GATEWAY), "server_error");
        assert_eq!(
            error_type_for(StatusCode::BAD_REQUEST),
            "invalid_request_error"
        );
        // 501 tells the caller to change the request (stream:false), so it is not
        // a server fault.
        assert_eq!(
            error_type_for(StatusCode::NOT_IMPLEMENTED),
            "invalid_request_error"
        );
    }

    #[tokio::test]
    async fn probe_provider_enforces_short_probe_timeout() {
        let app = Router::new().route(
            "/v1/chat/completions",
            post(|| async {
                futures_util::future::pending::<StatusCode>().await;
                StatusCode::OK
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });

        let dir = std::env::temp_dir().join(format!("umsp-probe-to-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let env_path = dir.join("env");
        std::fs::write(&env_path, "PROBE_KEY=sk-test\n").unwrap();

        let server = crate::config::ServerConfig {
            state_dir: dir.display().to_string(),
            request_timeout_s: 60,
            ..crate::config::ServerConfig::default()
        };
        let cfg = Config {
            server,
            providers: vec![Provider {
                id: "hung".into(),
                vendor: None,
                base_url: format!("http://{addr}/v1"),
                anthropic_base_url: None,
                api_key_env: "PROBE_KEY".into(),
                auth: "api_key".into(),
                protocols: vec!["chat".into()],
                quirks: None,
            }],
            models: vec![Model {
                id: "m".into(),
                backends: vec![crate::config::Backend {
                    provider: "hung".into(),
                    upstream: "m".into(),
                }],
                aliases: Vec::new(),
                context_length: 8192,
                context_lengths: Vec::new(),
                max_completion_tokens: 1024,
                capabilities: vec!["text".into()],
                display_name: None,
                created: 0,
                description: None,
            }],
            clients: Vec::new(),
        };

        let state = AppState::new(cfg, EnvFile::load(&env_path)).unwrap();
        let cfg_arc = state.cfg();
        let started = Instant::now();
        let value = probe_provider_with_timeout(
            &state,
            &cfg_arc,
            &cfg_arc.providers[0],
            Duration::from_millis(200),
        )
        .await;
        let elapsed = started.elapsed();
        assert_eq!(value["status"], "error", "{value}");
        assert_eq!(value["error"], "upstream_timeout", "{value}");
        assert!(
            elapsed < Duration::from_secs(2),
            "probe must not wait for request_timeout_s: {elapsed:?}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}
