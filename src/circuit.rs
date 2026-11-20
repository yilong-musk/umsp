//! Per-provider circuit breaker (spec 3.4).
//!
//! One in-memory state machine per provider, shared by every in-flight request:
//!
//! ```text
//! closed --(5 retryable failures within 30s)--> open --(30s)--> half_open
//!    ^             (402 DS: 10min, 401/402 OD: 5min)                |
//!    +------------------ successful probe --------------------------+
//! ```
//!
//! Only failures that are worth protecting the provider from count: `429`,
//! `5xx`, connection errors and timeouts. `400`/`404`/`422` are client mistakes
//! and never move the state machine. Auth (`401`) and balance (`402`) failures
//! open the circuit immediately with a longer cooldown, because retrying them
//! just burns time until the key/balance is fixed.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde::Serialize;
use tokio::time::Instant;

use crate::util;

/// Retryable failures counted in one window before the circuit opens.
pub const FAILURE_THRESHOLD: u32 = 5;
/// Sliding window for [`FAILURE_THRESHOLD`].
pub const FAILURE_WINDOW: Duration = Duration::from_secs(30);
/// How long an opened circuit rejects traffic before allowing a probe.
pub const COOLDOWN: Duration = Duration::from_secs(30);
/// DeepSeek balance exhausted (`402`): give the account time to be topped up.
pub const DS_BALANCE_COOLDOWN: Duration = Duration::from_secs(10 * 60);
/// Upstream auth failure (`401`) or non-DeepSeek `402`.
pub const AUTH_COOLDOWN: Duration = Duration::from_secs(5 * 60);
/// Backstop for a half-open probe slot that was never released because its
/// guard was leaked (the guard path covers cancellation; this keeps a lost
/// guard from turning into a permanent outage). Larger than the non-streaming
/// request timeout (600s) so a slow but live probe is never doubled up.
pub const PROBE_STALE_AFTER: Duration = Duration::from_secs(15 * 60);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum CircuitState {
    #[default]
    Closed,
    Open,
    HalfOpen,
}

/// What a request is allowed to do with a provider right now.
///
/// A granted probe carries a [`ProbeGuard`]: the half-open slot is released
/// when the guard is dropped, so a request that is cancelled before it records
/// a result (client disconnect, aborted task) can never wedge the provider in
/// `half_open` with no probe in flight.
#[derive(Debug)]
pub enum Admission {
    /// Circuit closed: normal traffic.
    Allow,
    /// One probe allowed (circuit just reached half-open). The guard owns the
    /// single probe slot for as long as the attempt is in flight.
    Probe(ProbeGuard),
    /// Circuit open: do not touch the upstream.
    Reject,
}

/// Owns the single half-open probe slot.
///
/// Dropping it without recording a result (cancellation) only releases the
/// slot — the circuit stays `half_open` and the next request may probe again.
/// A recorded result (`record_success` / `record_failure` / `force_open` /
/// `close`) bumps the generation, so a late drop is a no-op and can never
/// clear the slot of a newer probe.
pub struct ProbeGuard {
    inner: Arc<Mutex<HashMap<String, Circuit>>>,
    provider: String,
    generation: u64,
}

impl std::fmt::Debug for ProbeGuard {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ProbeGuard")
            .field("provider", &self.provider)
            .field("generation", &self.generation)
            .finish()
    }
}

impl Drop for ProbeGuard {
    fn drop(&mut self) {
        // Never panic in a destructor: a poisoned lock still gives us the map.
        let mut map = match self.inner.lock() {
            Ok(map) => map,
            Err(poisoned) => poisoned.into_inner(),
        };
        if let Some(circuit) = map.get_mut(&self.provider) {
            if circuit.probe_in_flight && circuit.probe_generation == self.generation {
                circuit.probe_in_flight = false;
                circuit.probe_started_at = None;
            }
        }
    }
}

#[derive(Debug, Clone, Copy)]
pub struct CircuitConfig {
    pub failure_threshold: u32,
    pub failure_window: Duration,
    pub cooldown: Duration,
    pub ds_balance_cooldown: Duration,
    pub auth_cooldown: Duration,
    pub probe_stale_after: Duration,
}

impl Default for CircuitConfig {
    fn default() -> Self {
        Self {
            failure_threshold: FAILURE_THRESHOLD,
            failure_window: FAILURE_WINDOW,
            cooldown: COOLDOWN,
            ds_balance_cooldown: DS_BALANCE_COOLDOWN,
            auth_cooldown: AUTH_COOLDOWN,
            probe_stale_after: PROBE_STALE_AFTER,
        }
    }
}

#[derive(Debug, Clone, Default)]
struct Circuit {
    state: CircuitState,
    failures: u32,
    window_start: Option<Instant>,
    opened_at: Option<Instant>,
    opened_at_wall: Option<String>,
    open_for: Duration,
    reason: Option<String>,
    probe_in_flight: bool,
    probe_started_at: Option<Instant>,
    /// Bumped every time a probe slot is granted or cleared; guards from an
    /// older grant can no longer touch the slot.
    probe_generation: u64,
}

impl Circuit {
    /// Cooldown of the current open state has elapsed: traffic may probe.
    fn cooled(&self) -> bool {
        self.opened_at
            .map(|at| at.elapsed() >= self.open_for)
            .unwrap_or(true)
    }

    /// Drop any probe slot and invalidate outstanding guards.
    fn clear_probe(&mut self) {
        self.probe_in_flight = false;
        self.probe_started_at = None;
        self.probe_generation = self.probe_generation.wrapping_add(1);
    }
}

/// Read-only snapshot for `/healthz`.
#[derive(Debug, Clone, Serialize)]
pub struct CircuitView {
    pub state: CircuitState,
    pub failures: u32,
    pub opened_at: Option<String>,
    pub reason: Option<String>,
    /// Cooldown that applies to the current open state (seconds).
    #[serde(skip)]
    pub open_for_s: u64,
    /// `open` but the cooldown has passed: the next request is a probe.
    #[serde(skip)]
    pub ready_for_probe: bool,
}

pub struct CircuitBreaker {
    cfg: CircuitConfig,
    inner: Arc<Mutex<HashMap<String, Circuit>>>,
}

impl Default for CircuitBreaker {
    fn default() -> Self {
        Self::new(CircuitConfig::default())
    }
}

impl CircuitBreaker {
    pub fn new(cfg: CircuitConfig) -> Self {
        Self {
            cfg,
            inner: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    pub fn config(&self) -> CircuitConfig {
        self.cfg
    }

    fn with<R>(&self, provider: &str, f: impl FnOnce(&mut Circuit) -> R) -> R {
        let mut map = self.inner.lock().expect("circuit lock poisoned");
        let entry = map.entry(provider.to_string()).or_default();
        f(entry)
    }

    fn open(circuit: &mut Circuit, reason: &str, cooldown: Duration) -> bool {
        let was_open = circuit.state == CircuitState::Open;
        circuit.state = CircuitState::Open;
        circuit.opened_at = Some(Instant::now());
        circuit.opened_at_wall = Some(util::now_rfc3339());
        circuit.open_for = cooldown;
        circuit.reason = Some(reason.to_string());
        circuit.clear_probe();
        !was_open
    }

    /// Take the single probe slot. Every grant bumps the generation so guards
    /// from earlier grants become no-ops when they are finally dropped.
    fn start_probe(circuit: &mut Circuit) -> u64 {
        circuit.probe_generation = circuit.probe_generation.wrapping_add(1);
        circuit.probe_in_flight = true;
        circuit.probe_started_at = Some(Instant::now());
        circuit.probe_generation
    }

    fn probe_guard(&self, provider: &str, generation: u64) -> ProbeGuard {
        ProbeGuard {
            inner: Arc::clone(&self.inner),
            provider: provider.to_string(),
            generation,
        }
    }

    /// Ask permission for one upstream attempt.
    pub fn admit(&self, provider: &str) -> Admission {
        let stale_after = self.cfg.probe_stale_after;
        let mut granted: Option<u64> = None;
        let allowed = self.with(provider, |circuit| match circuit.state {
            CircuitState::Closed => true,
            CircuitState::Open => {
                if circuit.cooled() {
                    circuit.state = CircuitState::HalfOpen;
                    granted = Some(Self::start_probe(circuit));
                    true
                } else {
                    false
                }
            }
            CircuitState::HalfOpen => {
                if circuit.probe_in_flight {
                    // A guard that was leaked must not wedge the provider in
                    // half-open forever; reclaim the slot after the backstop.
                    let stale = circuit
                        .probe_started_at
                        .map(|at| at.elapsed() >= stale_after)
                        .unwrap_or(true);
                    if stale {
                        granted = Some(Self::start_probe(circuit));
                        true
                    } else {
                        false
                    }
                } else {
                    granted = Some(Self::start_probe(circuit));
                    true
                }
            }
        });
        if !allowed {
            return Admission::Reject;
        }
        match granted {
            Some(generation) => Admission::Probe(self.probe_guard(provider, generation)),
            None => Admission::Allow,
        }
    }

    /// Claim the probe slot from the background health task. Returns the guard
    /// that owns the slot when the caller may probe now. Works for an `open`
    /// circuit whose cooldown has elapsed and for a `half_open` circuit whose
    /// slot is free (e.g. the previous probe was cancelled), so the background
    /// task can recover a provider without waiting for client traffic.
    pub fn take_probe(&self, provider: &str) -> Option<ProbeGuard> {
        let stale_after = self.cfg.probe_stale_after;
        let mut granted: Option<u64> = None;
        self.with(provider, |circuit| match circuit.state {
            CircuitState::Closed => {}
            CircuitState::Open => {
                if circuit.cooled() {
                    circuit.state = CircuitState::HalfOpen;
                    granted = Some(Self::start_probe(circuit));
                }
            }
            CircuitState::HalfOpen => {
                let free = !circuit.probe_in_flight
                    || circuit
                        .probe_started_at
                        .map(|at| at.elapsed() >= stale_after)
                        .unwrap_or(true);
                if free {
                    granted = Some(Self::start_probe(circuit));
                }
            }
        });
        granted.map(|generation| self.probe_guard(provider, generation))
    }

    /// Record a successful attempt. Returns the previous state when this closed
    /// a circuit (callers log `circuit_close`).
    pub fn record_success(&self, provider: &str) -> Option<CircuitState> {
        self.with(provider, |circuit| {
            let previous = circuit.state;
            circuit.state = CircuitState::Closed;
            circuit.failures = 0;
            circuit.window_start = None;
            circuit.opened_at = None;
            circuit.opened_at_wall = None;
            circuit.open_for = Duration::ZERO;
            circuit.reason = None;
            circuit.clear_probe();
            (previous != CircuitState::Closed).then_some(previous)
        })
    }

    /// Record a retryable failure (429/5xx/connection/timeout). Returns `true`
    /// when this opened (or reopened) the circuit.
    pub fn record_failure(&self, provider: &str, reason: &str) -> bool {
        let cooldown = self.cfg.cooldown;
        let threshold = self.cfg.failure_threshold;
        let window = self.cfg.failure_window;
        self.with(provider, |circuit| match circuit.state {
            CircuitState::Open => false,
            CircuitState::HalfOpen => Self::open(circuit, reason, cooldown),
            CircuitState::Closed => {
                let now = Instant::now();
                let stale = circuit
                    .window_start
                    .map(|start| now.duration_since(start) > window)
                    .unwrap_or(true);
                if stale {
                    circuit.window_start = Some(now);
                    circuit.failures = 0;
                }
                circuit.failures += 1;
                if circuit.failures >= threshold {
                    Self::open(circuit, reason, cooldown)
                } else {
                    false
                }
            }
        })
    }

    /// Open the circuit immediately (auth failure, balance exhausted). Returns
    /// `true` when it was not already open.
    pub fn force_open(&self, provider: &str, reason: &str, cooldown: Duration) -> bool {
        self.with(provider, |circuit| Self::open(circuit, reason, cooldown))
    }

    /// Close the circuit (balance restored / probe succeeded). Returns `true`
    /// when it was not already closed.
    pub fn close(&self, provider: &str) -> bool {
        self.with(provider, |circuit| {
            let was_closed = circuit.state == CircuitState::Closed;
            circuit.state = CircuitState::Closed;
            circuit.failures = 0;
            circuit.window_start = None;
            circuit.opened_at = None;
            circuit.opened_at_wall = None;
            circuit.open_for = Duration::ZERO;
            circuit.reason = None;
            circuit.clear_probe();
            !was_closed
        })
    }

    pub fn view(&self, provider: &str) -> CircuitView {
        self.with(provider, |circuit| {
            let ready_for_probe = circuit.state == CircuitState::Open && circuit.cooled();
            CircuitView {
                state: if ready_for_probe {
                    CircuitState::HalfOpen
                } else {
                    circuit.state
                },
                failures: circuit.failures,
                opened_at: circuit.opened_at_wall.clone(),
                reason: circuit.reason.clone(),
                open_for_s: circuit.open_for.as_secs(),
                ready_for_probe,
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fast() -> CircuitConfig {
        CircuitConfig {
            failure_threshold: 3,
            failure_window: Duration::from_millis(200),
            cooldown: Duration::from_millis(60),
            ds_balance_cooldown: Duration::from_millis(120),
            auth_cooldown: Duration::from_millis(90),
            probe_stale_after: Duration::from_millis(500),
        }
    }

    fn probe(breaker: &CircuitBreaker, provider: &str) -> Option<ProbeGuard> {
        match breaker.admit(provider) {
            Admission::Probe(guard) => Some(guard),
            _ => None,
        }
    }

    #[test]
    fn defaults_match_the_spec() {
        let cfg = CircuitConfig::default();
        assert_eq!(cfg.failure_threshold, 5);
        assert_eq!(cfg.failure_window, Duration::from_secs(30));
        assert_eq!(cfg.cooldown, Duration::from_secs(30));
        assert_eq!(cfg.ds_balance_cooldown, Duration::from_secs(600));
        assert_eq!(cfg.auth_cooldown, Duration::from_secs(300));
        assert_eq!(cfg.probe_stale_after, PROBE_STALE_AFTER);
        assert!(cfg.probe_stale_after > Duration::from_secs(600));
    }

    #[test]
    fn opens_after_threshold_and_reopens_after_probe_failure() {
        let breaker = CircuitBreaker::new(fast());
        assert!(matches!(breaker.admit("od"), Admission::Allow));
        assert!(!breaker.record_failure("od", "http_500"));
        assert!(!breaker.record_failure("od", "http_500"));
        assert!(breaker.record_failure("od", "http_500"));
        assert_eq!(breaker.view("od").state, CircuitState::Open);
        assert!(matches!(breaker.admit("od"), Admission::Reject));
    }

    #[test]
    fn old_failures_fall_out_of_the_window() {
        let breaker = CircuitBreaker::new(fast());
        breaker.record_failure("od", "http_500");
        breaker.record_failure("od", "http_500");
        std::thread::sleep(Duration::from_millis(250));
        // The window restarts; three fresh failures are needed.
        assert!(!breaker.record_failure("od", "http_500"));
        assert!(!breaker.record_failure("od", "http_500"));
        assert!(breaker.record_failure("od", "http_500"));
    }

    #[tokio::test]
    async fn half_open_allows_one_probe_then_closes_on_success() {
        let breaker = CircuitBreaker::new(fast());
        breaker.force_open("od", "http_500", Duration::from_millis(40));
        assert!(matches!(breaker.admit("od"), Admission::Reject));
        tokio::time::sleep(Duration::from_millis(60)).await;
        let guard = probe(&breaker, "od").expect("probe granted");
        // The probe slot is taken: a second concurrent request must not pass.
        assert!(matches!(breaker.admit("od"), Admission::Reject));
        assert_eq!(breaker.record_success("od"), Some(CircuitState::HalfOpen));
        drop(guard);
        assert_eq!(breaker.view("od").state, CircuitState::Closed);
        assert!(matches!(breaker.admit("od"), Admission::Allow));
    }

    #[tokio::test]
    async fn probe_failure_reopens_the_circuit() {
        let breaker = CircuitBreaker::new(fast());
        breaker.force_open("od", "http_500", Duration::from_millis(40));
        tokio::time::sleep(Duration::from_millis(60)).await;
        let guard = probe(&breaker, "od").expect("probe granted");
        assert!(breaker.record_failure("od", "http_500"));
        drop(guard);
        assert_eq!(breaker.view("od").state, CircuitState::Open);
        assert!(matches!(breaker.admit("od"), Admission::Reject));
    }

    /// The reported defect: a probe request that is cancelled (client
    /// disconnect, aborted task) never records a result. Its guard must release
    /// the slot so the next request can probe again — without counting a
    /// failure.
    #[tokio::test]
    async fn cancelled_probe_releases_the_slot() {
        let breaker = CircuitBreaker::new(fast());
        breaker.force_open("od", "http_500", Duration::from_millis(40));
        tokio::time::sleep(Duration::from_millis(60)).await;
        {
            let guard = probe(&breaker, "od").expect("probe granted");
            assert!(matches!(breaker.admit("od"), Admission::Reject));
            drop(guard); // request cancelled before any upstream result
        }
        let view = breaker.view("od");
        assert_eq!(view.failures, 0, "cancellation must not count as a failure");
        assert_eq!(view.state, CircuitState::HalfOpen);
        assert!(probe(&breaker, "od").is_some(), "next request may probe");
    }

    /// A late drop from an older probe must not release the slot of a newer
    /// probe that is still running.
    #[tokio::test]
    async fn stale_guard_cannot_release_a_newer_probe() {
        let cfg = CircuitConfig {
            probe_stale_after: Duration::from_millis(20),
            ..fast()
        };
        let breaker = CircuitBreaker::new(cfg);
        breaker.force_open("od", "http_500", Duration::from_millis(40));
        tokio::time::sleep(Duration::from_millis(60)).await;
        let old = probe(&breaker, "od").expect("first probe");
        // The first probe outlives the backstop, so the slot is reclaimed.
        tokio::time::sleep(Duration::from_millis(30)).await;
        let new = probe(&breaker, "od").expect("reclaimed probe");
        drop(old);
        assert!(
            matches!(breaker.admit("od"), Admission::Reject),
            "the newer probe must keep owning the slot"
        );
        drop(new);
        assert!(probe(&breaker, "od").is_some());
    }

    /// Even a guard that is leaked outright (`mem::forget`) cannot wedge the
    /// provider: the backstop reclaims the slot.
    #[tokio::test]
    async fn leaked_probe_slot_is_reclaimed_after_the_backstop() {
        let cfg = CircuitConfig {
            probe_stale_after: Duration::from_millis(20),
            ..fast()
        };
        let breaker = CircuitBreaker::new(cfg);
        breaker.force_open("od", "http_500", Duration::from_millis(40));
        tokio::time::sleep(Duration::from_millis(60)).await;
        let guard = probe(&breaker, "od").expect("probe granted");
        std::mem::forget(guard);
        assert!(matches!(breaker.admit("od"), Admission::Reject));
        tokio::time::sleep(Duration::from_millis(30)).await;
        assert!(probe(&breaker, "od").is_some());
    }

    #[tokio::test]
    async fn background_probe_owns_the_slot_until_dropped() {
        let breaker = CircuitBreaker::new(fast());
        breaker.force_open("od", "http_500", Duration::from_millis(40));
        tokio::time::sleep(Duration::from_millis(60)).await;
        let guard = breaker.take_probe("od").expect("background probe");
        assert!(matches!(breaker.admit("od"), Admission::Reject));
        // The background task cannot take a second probe while one is running.
        assert!(breaker.take_probe("od").is_none());
        drop(guard);
        assert!(probe(&breaker, "od").is_some());
    }

    /// After a cancelled probe the slot is free again, so the background health
    /// task can recover the provider even without client traffic.
    #[tokio::test]
    async fn background_probe_can_retake_a_cancelled_probe_slot() {
        let breaker = CircuitBreaker::new(fast());
        breaker.force_open("od", "http_500", Duration::from_millis(40));
        tokio::time::sleep(Duration::from_millis(60)).await;
        drop(probe(&breaker, "od").expect("client probe"));
        assert!(breaker.take_probe("od").is_some());
    }

    #[tokio::test]
    async fn forced_open_keeps_its_own_cooldown() {
        let breaker = CircuitBreaker::new(fast());
        assert!(breaker.force_open(
            "deepseek",
            "insufficient_balance",
            Duration::from_millis(120)
        ));
        assert!(matches!(breaker.admit("deepseek"), Admission::Reject));
        tokio::time::sleep(Duration::from_millis(140)).await;
        assert!(probe(&breaker, "deepseek").is_some());
        let view = breaker.view("deepseek");
        assert_eq!(view.reason.as_deref(), Some("insufficient_balance"));
        assert!(view.opened_at.is_some());
        assert_eq!(view.open_for_s, 0); // 120ms truncates to 0 seconds
    }

    #[test]
    fn close_is_idempotent_and_reports_changes() {
        let breaker = CircuitBreaker::new(fast());
        assert!(!breaker.close("od"));
        breaker.force_open("od", "auth_failed", Duration::from_secs(5));
        assert!(breaker.close("od"));
        assert!(!breaker.close("od"));
        assert_eq!(breaker.view("od").failures, 0);
    }
}
