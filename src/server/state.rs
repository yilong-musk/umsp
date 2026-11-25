use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex, RwLock};
use std::time::{Duration, Instant};

use serde_json::Value;

use crate::circuit::{CircuitBreaker, CircuitState};
use crate::config::{Config, Model};
use crate::envfile::EnvFile;
use crate::error::Result;
use crate::health::Balance;
use crate::metrics::Metrics;
use crate::route::{PlanKind, RouteTable, UsageWindow};
use crate::sessions::SessionStore;
use crate::upstream;
use crate::usage::{LogStore, LogWriter, RequestRecord};
use crate::util;

use super::background::CATALOG_INTERVAL;
use super::client::Upstream;
use super::failover::AttemptTarget;
use super::log::{plog, plog_route};

// ---------------------------------------------------------------------------
// Shared state
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Default)]
pub struct ProviderHealth {
    pub last_success: Option<String>,
    pub last_error: Option<String>,
    pub last_status: Option<u16>,
    pub last_latency_ms: Option<u64>,
}

#[derive(Debug, Clone)]
struct ProbeCacheEntry {
    at: Instant,
    value: Value,
}

/// Reloadable runtime snapshot swapped atomically on SIGHUP (or catalog update).
///
/// Bundling these fields into one `Arc` keeps every request internally
/// consistent: a reload that changes `state_dir`, `request_timeout_s`, or
/// upstream keys updates config, env, HTTP pool, ledger store, bindings, and
/// the precomputed model list together — never half-new and half-old.
#[derive(Clone)]
pub(super) struct Runtime {
    pub(super) cfg: Arc<Config>,
    pub(super) env: Arc<EnvFile>,
    pub(super) http: reqwest::Client,
    pub(super) logs: LogWriter,
    /// TOFU device bindings (client name -> source IP). Reloaded from disk on
    /// SIGHUP so CLI `client unbind` / `rotate` edits reach the data plane.
    pub(super) bindings: Arc<crate::bindings::BindingStore>,
    /// Last successful upstream model list, keyed by provider id.
    pub(super) catalog: Arc<HashMap<String, Vec<crate::catalog::ListedModel>>>,
    /// Precomputed logical models (`catalog::merge` over `cfg` + `catalog`).
    pub(super) models: Arc<Vec<Model>>,
}

fn compute_models(
    cfg: &Config,
    catalog: &HashMap<String, Vec<crate::catalog::ListedModel>>,
) -> Vec<Model> {
    if catalog.is_empty() {
        return cfg.models.clone();
    }
    let provider_ids: Vec<String> = cfg
        .providers
        .iter()
        .map(|provider| provider.id.clone())
        .collect();
    crate::catalog::merge(&cfg.models, &provider_ids, catalog)
}

pub struct AppState {
    inner: Arc<Inner>,
}

struct Inner {
    runtime: RwLock<Arc<Runtime>>,
    http_stream: reqwest::Client,
    started: Instant,
    health: Mutex<HashMap<String, ProviderHealth>>,
    probe_cache: Mutex<HashMap<String, ProbeCacheEntry>>,
    circuits: CircuitBreaker,
    routes: Mutex<RouteTable>,
    /// When the catalog was last refreshed, automatic or manual.
    catalog_refreshed_at: Mutex<Option<Instant>>,
    metrics: Metrics,
    sessions: SessionStore,
    /// Per-client in-flight model calls; process-local, survives reloads.
    concurrency: Arc<crate::limits::ConcurrencyLimiter>,
    /// Per-client token consumption and reservations; re-read on reload.
    budgets: Arc<crate::limits::BudgetStore>,
}

impl Clone for AppState {
    fn clone(&self) -> Self {
        Self {
            inner: Arc::clone(&self.inner),
        }
    }
}

impl AppState {
    pub fn new(cfg: Config, env: EnvFile) -> Result<Self> {
        let dir = crate::config::sessions_dir(&crate::config::default_config_path());
        Self::with_sessions(cfg, env, dir)
    }

    /// `sessions_dir` is `{config_dir}/sessions`. Each oauth provider reads only
    /// its own `<id>.json` from that directory.
    pub fn with_sessions(
        cfg: Config,
        env: EnvFile,
        sessions_dir: std::path::PathBuf,
    ) -> Result<Self> {
        let http = upstream::build_http_client(&cfg)?;
        let sessions = SessionStore::new(sessions_dir, http.clone());
        Self::with_store(cfg, env, http, sessions)
    }

    pub fn with_store(
        cfg: Config,
        env: EnvFile,
        http: reqwest::Client,
        sessions: SessionStore,
    ) -> Result<Self> {
        let http_stream = upstream::build_stream_client()?;
        let state_dir = cfg.server.state_path();
        let logs = LogWriter::new(&state_dir);
        let bindings = Arc::new(crate::bindings::BindingStore::load(&state_dir));
        let catalog = Arc::new(HashMap::new());
        let models = Arc::new(compute_models(&cfg, &catalog));
        let runtime = Arc::new(Runtime {
            cfg: Arc::new(cfg),
            env: Arc::new(env),
            http,
            logs,
            bindings,
            catalog,
            models,
        });
        Ok(Self {
            inner: Arc::new(Inner {
                runtime: RwLock::new(runtime),
                http_stream,
                started: Instant::now(),
                health: Mutex::new(HashMap::new()),
                probe_cache: Mutex::new(HashMap::new()),
                circuits: CircuitBreaker::default(),
                routes: Mutex::new(RouteTable::default()),
                catalog_refreshed_at: Mutex::new(None),
                metrics: Metrics::new(),
                sessions,
                concurrency: Arc::default(),
                budgets: Arc::new(crate::limits::BudgetStore::load(&state_dir)),
            }),
        })
    }

    pub(super) fn concurrency(&self) -> &Arc<crate::limits::ConcurrencyLimiter> {
        &self.inner.concurrency
    }

    pub(super) fn budgets(&self) -> &Arc<crate::limits::BudgetStore> {
        &self.inner.budgets
    }

    /// Charge a finished call to its client's token budget. Only clients
    /// with a budget are tracked, and `count_tokens` is free.
    ///
    /// A call that reached the upstream but whose usage is missing or
    /// incomplete — the client disconnected, or a successful upstream
    /// reported none — is charged at least what it reserved: the upstream
    /// has usually billed it, and otherwise disconnecting just before the
    /// final usage frame would make every call free.
    fn charge_budget(&self, record: &RequestRecord) {
        if record.quirks.iter().any(|q| q == "count_tokens") {
            return;
        }
        let held = self.inner.budgets.settle(&record.request_id);
        let budgeted = self
            .cfg()
            .client(&record.client)
            .is_some_and(|c| c.budget_tokens.is_some());
        if !budgeted {
            return;
        }
        let reported = record.total_tokens();
        let reached_upstream = !record.provider.is_empty();
        let usage_unreliable = record.client_disconnected
            || (reported == 0 && record.upstream_status.is_some_and(|s| s < 400));
        let tokens = if reached_upstream && usage_unreliable {
            reported.max(held)
        } else {
            reported
        };
        if tokens == 0 {
            return;
        }
        if self.inner.budgets.charge(&record.client, tokens).is_err() {
            plog(
                "error",
                "budget_persist_failed",
                &record.request_id,
                Some(&record.client),
                Some(&record.provider),
                None,
            );
        }
    }

    pub(super) fn runtime(&self) -> Arc<Runtime> {
        Arc::clone(&self.inner.runtime.read().expect("runtime lock poisoned"))
    }

    pub fn circuits(&self) -> &CircuitBreaker {
        &self.inner.circuits
    }

    /// Record a balance observation. `total` is the upstream's decimal string.
    pub fn note_provider_balance(&self, provider: &str, balance: &Balance) {
        let total = balance
            .total
            .as_deref()
            .and_then(|raw| raw.trim().parse::<f64>().ok());
        self.inner
            .routes
            .lock()
            .expect("route lock poisoned")
            .note_balance(provider, balance.is_available, total, Instant::now());
    }

    /// Read the tightest fresh usage window for admin output.
    pub fn route_usage(&self, provider: &str) -> Option<crate::route::UsageSnapshot> {
        self.inner
            .routes
            .lock()
            .expect("route lock poisoned")
            .usage_summary(provider, Instant::now())
    }

    /// Record a fresh usage window for `provider`. Selection reads this on
    /// the next request; a spent window makes the provider ineligible.
    pub fn note_provider_usage(&self, provider: &str, window: UsageWindow) {
        self.inner
            .routes
            .lock()
            .expect("route lock poisoned")
            .note_usage(provider, vec![window]);
    }

    /// Remember `provider` for `model` when the model has no live pin.
    /// Returns true when a new record was written.
    pub(super) fn remember_route(&self, model: &str, provider: &str) -> bool {
        self.inner
            .routes
            .lock()
            .expect("route lock poisoned")
            .remember(model, provider, Instant::now())
    }

    /// Drop the model's pin only when it still names `provider`.
    pub(super) fn forget_route(&self, model: &str, provider: &str) -> bool {
        self.inner
            .routes
            .lock()
            .expect("route lock poisoned")
            .forget_if(model, provider)
    }

    /// Backends to try, in selection order. A live pin's provider is first, and
    /// every backend of a provider is kept in configuration order. `use_pin`
    /// false leaves the stored pin untouched.
    pub(super) fn route_targets(
        &self,
        model: &str,
        candidates: &[AttemptTarget],
        excluded: &HashSet<String>,
        skipped: &HashSet<(String, String)>,
        use_pin: bool,
    ) -> Vec<AttemptTarget> {
        let mut ids = Vec::new();
        let mut closed = HashMap::new();
        let mut backends = Vec::new();
        for candidate in candidates {
            if !ids.contains(&candidate.provider.id) {
                ids.push(candidate.provider.id.clone());
            }
            let state = self.circuits().view(&candidate.provider.id).state;
            closed.insert(candidate.provider.id.clone(), state == CircuitState::Closed);
            backends.push((candidate.provider.id.clone(), candidate.upstream.clone()));
        }
        let (order, drop) = {
            let routes = self.inner.routes.lock().expect("route lock poisoned");
            let now = Instant::now();
            let plan = routes.plan(model, &ids, excluded, &closed, now, use_pin);
            let mut order = plan.provider_ids.clone();
            if plan.kind == PlanKind::Pinned {
                let mut tail_excluded = excluded.clone();
                if let Some(pin_id) = order.first() {
                    tail_excluded.insert(pin_id.clone());
                }
                let tail = routes.plan(model, &ids, &tail_excluded, &closed, now, false);
                order.extend(tail.provider_ids);
            }
            (order, plan.drop)
        };
        if use_pin {
            if let Some((provider, reason)) = drop {
                if self.forget_route(model, &provider) {
                    plog_route("route_unpin", model, &provider, reason.as_str());
                }
            }
        }
        let pairs = crate::route::expand_backends(&order, &backends, skipped);
        let mut used = vec![false; candidates.len()];
        pairs
            .into_iter()
            .filter_map(|(provider, upstream)| {
                let index = candidates
                    .iter()
                    .enumerate()
                    .position(|(index, candidate)| {
                        !used[index]
                            && candidate.provider.id == provider
                            && candidate.upstream == upstream
                    })?;
                used[index] = true;
                Some(candidates[index].clone())
            })
            .collect()
    }

    pub fn cfg(&self) -> Arc<Config> {
        Arc::clone(&self.runtime().cfg)
    }

    /// Logical models callers can request. Before any upstream list has been
    /// read this is the configured set. After that it is the deduplicated
    /// union. Metadata comes from the upstream list when that list carries it;
    /// configured entries fill only the gaps and keep unlisted models.
    pub fn models(&self) -> Arc<Vec<Model>> {
        Arc::clone(&self.runtime().models)
    }

    pub(super) fn catalog_snapshot(
        &self,
    ) -> Arc<HashMap<String, Vec<crate::catalog::ListedModel>>> {
        Arc::clone(&self.runtime().catalog)
    }

    pub(super) fn note_catalog(&self, provider: &str, models: Vec<crate::catalog::ListedModel>) {
        let mut guard = self.inner.runtime.write().expect("runtime lock poisoned");
        let mut next = (**guard).clone();
        let mut catalog = (*next.catalog).clone();
        catalog.insert(provider.to_string(), models);
        next.models = Arc::new(compute_models(&next.cfg, &catalog));
        next.catalog = Arc::new(catalog);
        *guard = Arc::new(next);
    }

    pub(super) fn catalog_due(&self) -> bool {
        self.inner
            .catalog_refreshed_at
            .lock()
            .expect("catalog clock poisoned")
            .map(|at| at.elapsed() >= CATALOG_INTERVAL)
            .unwrap_or(true)
    }

    pub(super) fn mark_catalog_refreshed(&self) {
        *self
            .inner
            .catalog_refreshed_at
            .lock()
            .expect("catalog clock poisoned") = Some(Instant::now());
    }

    pub fn env(&self) -> Arc<EnvFile> {
        Arc::clone(&self.runtime().env)
    }

    pub fn http(&self) -> reqwest::Client {
        self.runtime().http.clone()
    }

    /// Timeout-free client used for SSE upstream requests.
    pub fn stream_http(&self) -> &reqwest::Client {
        &self.inner.http_stream
    }

    pub fn logs(&self) -> LogStore {
        self.runtime().logs.store().clone()
    }

    /// Block until every queued ledger row has been written to disk.
    pub fn flush_logs(&self) {
        self.runtime().logs.flush();
    }

    pub fn metrics(&self) -> &Metrics {
        &self.inner.metrics
    }

    pub fn sessions(&self) -> SessionStore {
        self.inner.sessions.clone()
    }

    /// Upstream client bound to this process's HTTP pools and session store.
    pub fn upstream(&self) -> Upstream {
        let rt = self.runtime();
        Upstream::with_clients(
            Arc::clone(&rt.cfg),
            Arc::clone(&rt.env),
            rt.http.clone(),
            self.stream_http().clone(),
            self.sessions(),
        )
    }

    /// Single exit point for a finished request: one ledger row *and* one
    /// metrics observation, so `/metrics` and `umsp usage` cannot diverge.
    pub fn finish_request(&self, record: &RequestRecord) {
        self.charge_budget(record);
        self.inner.metrics.observe(record);
        if !self.runtime().logs.enqueue(record.clone()) {
            plog(
                "warn",
                "ledger_queue_full",
                &record.request_id,
                Some(&record.client),
                Some(&record.provider),
                None,
            );
        }
    }

    pub fn uptime_s(&self) -> u64 {
        self.inner.started.elapsed().as_secs()
    }

    /// Atomically replace the reloadable runtime snapshot (SIGHUP reload).
    ///
    /// Re-reads device bindings from the new `state_dir`, swaps the env file,
    /// switches the log store when `state_dir` changed, rebuilds the HTTP
    /// client when `request_timeout_s` changed (keeping existing pools when it
    /// did not), and recomputes the merged model list.
    pub fn reload(&self, cfg: Config, env: EnvFile) -> Result<()> {
        let current = self.runtime();
        let next_http = if cfg.server.request_timeout_s != current.cfg.server.request_timeout_s {
            Some(upstream::build_http_client(&cfg)?)
        } else {
            None
        };
        let state_dir = cfg.server.state_path();
        let next_logs = if state_dir != current.cfg.server.state_path() {
            Some(LogWriter::new(&state_dir))
        } else {
            None
        };
        let next_bindings = Arc::new(crate::bindings::BindingStore::load(&state_dir));
        self.inner.budgets.reload(&state_dir);
        let next_cfg = Arc::new(cfg);
        let next_env = Arc::new(env);

        let mut guard = self.inner.runtime.write().expect("runtime lock poisoned");
        let models = Arc::new(compute_models(&next_cfg, &guard.catalog));
        let runtime = Runtime {
            cfg: next_cfg,
            env: next_env,
            http: next_http.unwrap_or_else(|| guard.http.clone()),
            logs: next_logs.unwrap_or_else(|| guard.logs.clone()),
            bindings: next_bindings,
            catalog: Arc::clone(&guard.catalog),
            models,
        };
        *guard = Arc::new(runtime);
        Ok(())
    }

    /// Swap in a new config while keeping the current env file and bindings.
    /// Returns the previous config.
    pub fn replace_config(&self, cfg: Config) -> Arc<Config> {
        let current = self.runtime();
        let next_http = if cfg.server.request_timeout_s != current.cfg.server.request_timeout_s {
            upstream::build_http_client(&cfg).ok()
        } else {
            None
        };
        let state_dir = cfg.server.state_path();
        let next_logs = if state_dir != current.cfg.server.state_path() {
            Some(LogWriter::new(&state_dir))
        } else {
            None
        };
        let mut guard = self.inner.runtime.write().expect("runtime lock poisoned");
        let prev = Arc::clone(&guard.cfg);
        let next_cfg = Arc::new(cfg);
        let models = Arc::new(compute_models(&next_cfg, &guard.catalog));
        let runtime = Runtime {
            cfg: next_cfg,
            env: Arc::clone(&guard.env),
            http: next_http.unwrap_or_else(|| guard.http.clone()),
            logs: next_logs.unwrap_or_else(|| guard.logs.clone()),
            bindings: Arc::clone(&guard.bindings),
            catalog: Arc::clone(&guard.catalog),
            models,
        };
        *guard = Arc::new(runtime);
        prev
    }

    /// Current device-binding store (cheap Arc clone under a read lock).
    pub fn bindings(&self) -> Arc<crate::bindings::BindingStore> {
        Arc::clone(&self.runtime().bindings)
    }

    /// Re-read bindings from disk.
    pub fn reload_bindings(&self, state_dir: &std::path::Path) {
        let store = Arc::new(crate::bindings::BindingStore::load(state_dir));
        let mut guard = self.inner.runtime.write().expect("runtime lock poisoned");
        let mut next = (**guard).clone();
        next.bindings = store;
        *guard = Arc::new(next);
    }

    pub fn note_health(&self, provider: &str, status: u16, latency_ms: u64, error: Option<String>) {
        let mut health = self.inner.health.lock().expect("health lock poisoned");
        let entry = health.entry(provider.to_string()).or_default();
        entry.last_status = Some(status);
        entry.last_latency_ms = Some(latency_ms);
        if status < 400 {
            entry.last_success = Some(util::now_rfc3339());
            entry.last_error = None;
        } else {
            entry.last_error = error.or_else(|| Some(format!("http_{status}")));
        }
    }

    pub(super) fn provider_health(&self, provider: &str) -> ProviderHealth {
        self.inner
            .health
            .lock()
            .expect("health lock poisoned")
            .get(provider)
            .cloned()
            .unwrap_or_default()
    }

    pub(super) fn probe_cached(&self, provider: &str, ttl: Duration) -> Option<Value> {
        let cache = self.inner.probe_cache.lock().expect("probe lock poisoned");
        cache
            .get(provider)
            .filter(|entry| entry.at.elapsed() < ttl)
            .map(|entry| entry.value.clone())
    }

    pub(super) fn probe_store(&self, provider: &str, value: Value) {
        let mut cache = self.inner.probe_cache.lock().expect("probe lock poisoned");
        cache.insert(
            provider.to_string(),
            ProbeCacheEntry {
                at: Instant::now(),
                value,
            },
        );
    }
}
