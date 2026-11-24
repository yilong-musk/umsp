use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use axum::http::HeaderMap;
use serde_json::{Value, json};

use crate::circuit::CircuitState;
use crate::config::{Config, Provider};
use crate::grok_billing;
use crate::health::{self, Balance};
use crate::sessions;
use crate::upstream;

use super::client::{ChatOutcome, ProxyError, Upstream};
use super::failover::{AttemptTarget, note_circuit_error, note_circuit_result};
use super::log::plog_health;
use super::state::AppState;

pub(super) async fn probe_provider(
    state: &AppState,
    cfg: &Arc<Config>,
    provider: &Provider,
) -> Value {
    probe_provider_with_timeout(state, cfg, provider, PROBE_TIMEOUT).await
}

pub(super) async fn probe_provider_with_timeout(
    state: &AppState,
    cfg: &Arc<Config>,
    provider: &Provider,
    timeout: Duration,
) -> Value {
    if let Some(cached) = state.probe_cached(&provider.id, Duration::from_secs(30)) {
        return cached;
    }
    // DeepSeek: check the balance first — an empty account is a definitive
    // answer and must not burn a token (spec 6.1).
    let mut balance_info: Option<Balance> = None;
    if provider.quirks.as_deref() == Some("deepseek") {
        match deepseek_balance(state, provider).await {
            Ok(balance) => {
                state.note_provider_balance(&provider.id, &balance);
                if !balance.is_available {
                    let value = json!({
                        "status": "degraded",
                        "error": "insufficient_balance",
                        "balance": balance.to_json(),
                    });
                    state.probe_store(&provider.id, value.clone());
                    return value;
                }
                balance_info = Some(balance);
            }
            Err(err) => {
                let value = json!({"status": "error", "error": err});
                state.probe_store(&provider.id, value.clone());
                return value;
            }
        }
    }
    let Some(upstream) = cfg.models.iter().find_map(|m| {
        m.backends
            .iter()
            .find(|b| b.provider == provider.id)
            .map(|b| b.upstream.clone())
    }) else {
        let value = json!({"status": "unknown", "error": "no model configured for provider"});
        state.probe_store(&provider.id, value.clone());
        return value;
    };
    let started = Instant::now();
    let Some(outcome) = probe_inference(state, cfg, provider, &upstream, timeout).await else {
        let value = json!({"status": "unknown", "error": "provider has no probeable protocol"});
        state.probe_store(&provider.id, value.clone());
        return value;
    };
    let mut value = match outcome {
        Ok(outcome) => {
            state.note_health(&provider.id, outcome.status, outcome.duration_ms, None);
            if outcome.status < 400 {
                json!({"status": "ok", "latency_ms": outcome.duration_ms})
            } else {
                json!({"status": "degraded", "http_status": outcome.status, "latency_ms": outcome.duration_ms})
            }
        }
        Err(err) => {
            let latency = started.elapsed().as_millis() as u64;
            state.note_health(&provider.id, 0, latency, Some(err.code().to_string()));
            json!({"status": "error", "error": err.code(), "latency_ms": latency})
        }
    };
    if let Some(balance) = balance_info {
        value["balance"] = balance.to_json();
    }
    state.probe_store(&provider.id, value.clone());
    value
}

/// Minimal 1-token inference probe shared by `/healthz?probe=1` and background
/// circuit recovery, bounded by `timeout` so a hung provider cannot block for
/// the full 600s request timeout.
pub(super) async fn probe_inference(
    state: &AppState,
    cfg: &Arc<Config>,
    provider: &Provider,
    upstream: &str,
    timeout: Duration,
) -> Option<std::result::Result<ChatOutcome, ProxyError>> {
    let rt = state.runtime();
    let up = Upstream::with_clients(
        Arc::clone(cfg),
        Arc::clone(&rt.env),
        rt.http.clone(),
        state.stream_http().clone(),
        state.sessions(),
    );
    let call = async {
        if provider.supports("chat") {
            let body = json!({
                "model": upstream,
                "messages": [{"role": "user", "content": "ping"}],
                "max_tokens": 1,
                "reasoning_effort": "none",
                "stream": false
            });
            Some(up.chat(provider, upstream, &body, &HeaderMap::new()).await)
        } else if provider.supports("responses") {
            let body = json!({
                "model": upstream,
                "input": "ping",
                "max_output_tokens": 16,
                "stream": false
            });
            Some(up.responses(provider, &body, &HeaderMap::new()).await)
        } else {
            None
        }
    };
    match tokio::time::timeout(timeout, call).await {
        Ok(result) => result,
        Err(_) => Some(Err(ProxyError::Timeout("health probe timed out".into()))),
    }
}

// ---------------------------------------------------------------------------
// Background health (spec 3.4 / 6.1)
// ---------------------------------------------------------------------------

/// How often an open DeepSeek circuit re-checks `/user/balance`.
pub(super) const BALANCE_INTERVAL: Duration = Duration::from_secs(300);
/// How often each provider's model list is re-read. A failed read keeps the
/// previous list. `umsp catalog refresh` reads immediately and resets this clock.
pub(super) const CATALOG_INTERVAL: Duration = Duration::from_secs(12 * 60 * 60);
/// Grok billing is free; poll it often enough to keep the 5-minute snapshot fresh.
pub(super) const GROK_USAGE_INTERVAL: Duration = Duration::from_secs(240);
/// How often the health task looks at the circuits at all.
pub(super) const HEALTH_TICK: Duration = Duration::from_secs(60);
/// Per-provider bound for inference health probes (`/healthz?probe=1` and
/// background circuit recovery), well below `request_timeout_s` (600s).
pub(super) const PROBE_TIMEOUT: Duration = Duration::from_secs(15);

pub(super) async fn deepseek_balance(
    state: &AppState,
    provider: &Provider,
) -> std::result::Result<Balance, String> {
    let rt = state.runtime();
    let key = rt
        .env
        .get(&provider.api_key_env)
        .ok_or_else(|| format!("missing {}", provider.api_key_env))?;
    health::fetch_balance(&rt.http, provider, &key).await
}

pub(super) fn balance_upstream(cfg: &Config, provider_id: &str) -> String {
    cfg.models
        .iter()
        .find_map(|model| {
            model
                .backends
                .iter()
                .find(|backend| backend.provider == provider_id)
                .map(|backend| backend.upstream.clone())
        })
        .unwrap_or_else(|| provider_id.to_string())
}

pub(super) async fn refresh_catalog(state: &AppState) -> Vec<Value> {
    state.mark_catalog_refreshed();
    let cfg = state.cfg();
    let futs = cfg.providers.iter().map(|provider| {
        let state = state.clone();
        let provider = provider.clone();
        async move {
            match fetch_model_ids(&state, &provider).await {
                Ok(models) => {
                    let count = models.len();
                    state.note_catalog(&provider.id, models);
                    plog_health("catalog", &provider.id, "", &format!("models_{count}"));
                    json!({"id": provider.id, "models": count})
                }
                Err(err) => {
                    plog_health("catalog_failed", &provider.id, "", &err);
                    json!({"id": provider.id, "error": err})
                }
            }
        }
    });
    futures_util::future::join_all(futs).await
}

pub(super) async fn fetch_model_ids(
    state: &AppState,
    provider: &Provider,
) -> std::result::Result<Vec<crate::catalog::ListedModel>, String> {
    let token = if provider.uses_oauth() {
        state
            .sessions()
            .access_token(&provider.id)
            .await
            .map_err(|err| err.to_string())?
    } else {
        state
            .env()
            .get(&provider.api_key_env)
            .ok_or_else(|| format!("missing {}", provider.api_key_env))?
    };
    let url = upstream::join_url(&provider.base_url, "models");
    let mut request = state
        .http()
        .get(&url)
        .timeout(Duration::from_secs(15))
        .header(reqwest::header::AUTHORIZATION, format!("Bearer {token}"));
    if provider.vendor.as_deref() == Some("grok") {
        let version = state
            .env()
            .get("UMSP_GROK_CLI_VERSION")
            .unwrap_or_else(|| sessions::GROK_CLI_VERSION.to_string());
        request = request
            .header("X-XAI-Token-Auth", sessions::CLI_TOKEN_AUTH)
            .header("x-grok-client-version", version);
    }
    if provider.vendor.as_deref() == Some("opencode-go") {
        request = request.header(reqwest::header::USER_AGENT, upstream::OPENCODE_USER_AGENT);
    }
    let response = request
        .send()
        .await
        .map_err(|err| format!("unreachable: {err}"))?;
    let status = response.status();
    if !status.is_success() {
        return Err(format!("http_{}", status.as_u16()));
    }
    let body: Value = response
        .json()
        .await
        .map_err(|err| format!("invalid json: {err}"))?;
    crate::catalog::parse_models(&body).ok_or_else(|| "unrecognized model list".to_string())
}

/// Run one health-tick pass for a single provider (usage poll, balance check,
/// and circuit recovery probe). Executed concurrently across providers so one
/// slow or hung upstream cannot delay the rest.
pub(super) async fn health_tick_provider(
    state: AppState,
    cfg: Arc<Config>,
    provider: Provider,
    due_grok: bool,
    due_balance: bool,
    grok_user_id: Option<String>,
    probe_timeout: Duration,
) -> Option<(String, String)> {
    let circuits = state.circuits();
    let view = circuits.view(&provider.id);
    let mut updated_grok_user_id = None;

    if due_grok {
        let upstream = balance_upstream(&cfg, &provider.id);
        let rt = state.runtime();
        match grok_billing::fetch_usage(
            &rt.http,
            &state.sessions(),
            &provider,
            &rt.env
                .get("UMSP_GROK_CLI_VERSION")
                .unwrap_or_else(|| sessions::GROK_CLI_VERSION.to_string()),
            grok_user_id.as_deref(),
        )
        .await
        {
            Ok((usage, user_id)) => {
                updated_grok_user_id = Some((provider.id.clone(), user_id));
                if let Some(window) = usage.into_window() {
                    let remaining = window.remaining_fraction;
                    state.note_provider_usage(&provider.id, window);
                    plog_health(
                        "grok_usage",
                        &provider.id,
                        &upstream,
                        &format!("remaining_{:.0}pct", remaining * 100.0),
                    );
                } else {
                    plog_health(
                        "grok_usage",
                        &provider.id,
                        &upstream,
                        "billing_period_ended",
                    );
                }
            }
            Err(err) => plog_health("grok_usage_failed", &provider.id, &upstream, &err),
        }
    }

    // DeepSeek balance is free. Refresh it for healthy providers too, so
    // selection can skip an exhausted account without waiting for a failure.
    if due_balance {
        match deepseek_balance(&state, &provider).await {
            Ok(balance) => {
                state.note_provider_balance(&provider.id, &balance);
                if view.state != CircuitState::Closed {
                    if balance.is_available {
                        let balance_reason = matches!(
                            view.reason.as_deref(),
                            Some("insufficient_balance") | Some("balance_check_failed") | None
                        );
                        if balance_reason && circuits.close(&provider.id) {
                            let upstream = balance_upstream(&cfg, &provider.id);
                            plog_health(
                                "circuit_close",
                                &provider.id,
                                &upstream,
                                "balance_restored",
                            );
                        }
                        if balance_reason {
                            return updated_grok_user_id;
                        }
                    } else {
                        circuits.force_open(
                            &provider.id,
                            "insufficient_balance",
                            circuits.config().ds_balance_cooldown,
                        );
                        return updated_grok_user_id;
                    }
                }
            }
            Err(err) => {
                let upstream = balance_upstream(&cfg, &provider.id);
                let reason = format!("balance_check_failed:{err}");
                if view.state == CircuitState::Closed {
                    plog_health("balance_check_failed", &provider.id, &upstream, &reason);
                } else {
                    plog_health("circuit_open", &provider.id, &upstream, &reason);
                    if view.state == CircuitState::HalfOpen {
                        circuits.force_open(&provider.id, "balance_check_failed", BALANCE_INTERVAL);
                    }
                    return updated_grok_user_id;
                }
            }
        }
    }

    if view.state == CircuitState::Closed {
        return updated_grok_user_id;
    }
    let Some((model, upstream)) = cfg.models.iter().find_map(|m| {
        m.backends
            .iter()
            .find(|b| b.provider == provider.id)
            .map(|b| (m.clone(), b.upstream.clone()))
    }) else {
        return updated_grok_user_id;
    };

    let Some(_probe_guard) = circuits.take_probe(&provider.id) else {
        return updated_grok_user_id;
    };
    let target = AttemptTarget {
        model,
        provider: provider.clone(),
        upstream: upstream.clone(),
        upstream_protocol: "chat".into(),
    };
    let Some(outcome) = probe_inference(&state, &cfg, &provider, &upstream, probe_timeout).await
    else {
        return updated_grok_user_id;
    };
    match outcome {
        Ok(outcome) => {
            state.note_health(
                &target.provider.id,
                outcome.status,
                outcome.duration_ms,
                (outcome.status >= 400).then(|| format!("http_{}", outcome.status)),
            );
            if outcome.status < 400 || !upstream::status_is_retryable(outcome.status) {
                // A client error still proves the provider is reachable
                // and the key works: close, don't count it.
                if circuits.close(&target.provider.id) {
                    plog_health(
                        "circuit_close",
                        &target.provider.id,
                        &target.upstream,
                        "probe_ok",
                    );
                }
            } else {
                note_circuit_result(&state, &target, outcome.status, true);
            }
        }
        Err(err) => {
            state.note_health(&target.provider.id, 0, 0, Some(err.code().to_string()));
            note_circuit_error(&state, &target, &err);
        }
    }
    updated_grok_user_id
}

/// Background circuit recovery and provider health (spec 3.4). DeepSeek gets a
/// balance check first; other open providers get a 1-token inference probe.
///
/// Spawned by `serve` and aborted when serving stops; `JoinSet`s abort any
/// in-flight provider probes or catalog refreshes on drop so nothing outlives
/// shutdown.
pub(super) async fn health_loop(state: AppState) {
    let mut ticker = tokio::time::interval(HEALTH_TICK);
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut last_balance: HashMap<String, Instant> = HashMap::new();
    let mut last_grok_usage: HashMap<String, Instant> = HashMap::new();
    let mut grok_user_ids: HashMap<String, String> = HashMap::new();
    let mut catalog_tasks: tokio::task::JoinSet<()> = tokio::task::JoinSet::new();

    loop {
        ticker.tick().await;
        while catalog_tasks.try_join_next().is_some() {}
        if state.catalog_due() && catalog_tasks.is_empty() {
            let refresh = state.clone();
            catalog_tasks.spawn(async move {
                let _ = refresh_catalog(&refresh).await;
            });
        }
        let cfg = state.cfg();
        let mut tick_tasks = tokio::task::JoinSet::new();
        for provider in &cfg.providers {
            let due_grok = provider.vendor.as_deref() == Some("grok")
                && provider.uses_oauth()
                && last_grok_usage
                    .get(&provider.id)
                    .map(|at| at.elapsed() >= GROK_USAGE_INTERVAL)
                    .unwrap_or(true);
            if due_grok {
                last_grok_usage.insert(provider.id.clone(), Instant::now());
            }
            let due_balance = provider.quirks.as_deref() == Some("deepseek")
                && last_balance
                    .get(&provider.id)
                    .map(|at| at.elapsed() >= BALANCE_INTERVAL)
                    .unwrap_or(true);
            if due_balance {
                last_balance.insert(provider.id.clone(), Instant::now());
            }
            let user_id = grok_user_ids.get(&provider.id).cloned();
            tick_tasks.spawn(health_tick_provider(
                state.clone(),
                Arc::clone(&cfg),
                provider.clone(),
                due_grok,
                due_balance,
                user_id,
                PROBE_TIMEOUT,
            ));
        }
        while let Some(res) = tick_tasks.join_next().await {
            if let Ok(Some((provider_id, user_id))) = res {
                grok_user_ids.insert(provider_id, user_id);
            }
        }
    }
}
