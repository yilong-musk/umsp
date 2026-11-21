//! Provider selection and per-model stickiness.
//!
//! Selection reads in-memory capacity snapshots. It does not call upstreams.
//! A missing or expired snapshot counts as "no data".
//!
//! Healthy providers are ordered in three classes:
//!
//! 1. Providers with a fresh usage window (`U`). Windows are compared by
//!    remaining fraction only; a provider that does not report a window is
//!    treated as having infinite remaining there. The comparison is
//!    lexicographic over the sorted fractions, so the tightest window decides
//!    first and the second-tightest breaks a tie.
//! 2. Providers with neither a fresh usage window nor a usable balance (`X`).
//! 3. Providers with a fresh, usable balance and no fresh usage window (`B`).
//!
//! Configuration order is the tie-break inside a class. A provider with a
//! fresh usage window is never gated by a balance observation: usage wins.
//!
//! A model remembers at most one provider for five hours, measured from the
//! moment the record was written. Later successes do not extend it. The record
//! is replaced only when that provider can no longer take a request, or when
//! the five hours have elapsed.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::time::{Duration, Instant};

/// How long a per-model provider record stays in force.
pub const PIN_TTL: Duration = Duration::from_secs(5 * 60 * 60);
/// A usage snapshot is refreshed every four minutes; five keeps one miss fresh.
pub const USAGE_SNAPSHOT_TTL: Duration = Duration::from_secs(5 * 60);
/// Balance is refreshed on a five-minute poll, so the snapshot outlives one miss.
pub const BALANCE_SNAPSHOT_TTL: Duration = Duration::from_secs(10 * 60);

/// One remaining-usage window reported for a provider.
///
/// Only the fraction is kept: providers report requests, tokens or credits in
/// different units, and cross-provider ordering compares fractions only.
#[derive(Debug, Clone)]
pub struct UsageWindow {
    pub duration: Duration,
    pub remaining_fraction: f64,
    /// `None` when the upstream did not say when the window resets.
    pub reset_at: Option<Instant>,
    pub observed_at: Instant,
}

/// A balance observation. `total` is the numeric balance when the upstream sent one.
#[derive(Debug, Clone)]
pub struct BalanceSample {
    pub is_available: bool,
    pub total: Option<f64>,
    pub observed_at: Instant,
}

/// Latest capacity facts for one provider. Absence of the entry means no data.
#[derive(Debug, Clone, Default)]
pub struct Capacity {
    pub windows: Vec<UsageWindow>,
    pub balance: Option<BalanceSample>,
}

/// Read-only snapshot for admin surfaces.
#[derive(Debug, Clone, PartialEq)]
pub struct UsageSnapshot {
    pub remaining_fraction: f64,
    pub windows: usize,
    pub reset_in_s: Option<u64>,
}

/// The provider a model is sticking to.
#[derive(Debug, Clone)]
pub struct Pin {
    pub provider_id: String,
    pub recorded_at: Instant,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PlanKind {
    /// The live pin. Try this provider and nobody else until it fails.
    Pinned,
    /// Healthy providers in selection order.
    Ranked,
    /// Nobody is healthy. Walk config order and let the circuit breaker decide.
    Circuit,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PinDrop {
    Expired,
    Ineligible,
    Unhealthy,
    Unreachable,
}

impl PinDrop {
    pub fn as_str(self) -> &'static str {
        match self {
            PinDrop::Expired => "expired",
            PinDrop::Ineligible => "ineligible",
            PinDrop::Unhealthy => "unhealthy",
            PinDrop::Unreachable => "unreachable",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Plan {
    pub provider_ids: Vec<String>,
    pub kind: PlanKind,
    /// Remove this pin before using `provider_ids`. The id is the one that was
    /// stored, so a newer pin written by another request is left alone.
    pub drop: Option<(String, PinDrop)>,
}

#[derive(Debug, Default)]
pub struct RouteTable {
    capacity: HashMap<String, Capacity>,
    pins: HashMap<String, Pin>,
}

impl RouteTable {
    pub fn note_usage(&mut self, provider: &str, windows: Vec<UsageWindow>) {
        let mut by_duration: BTreeMap<Duration, UsageWindow> = BTreeMap::new();
        for mut window in windows {
            if window.remaining_fraction.is_finite() {
                window.remaining_fraction = window.remaining_fraction.clamp(0.0, 1.0);
                by_duration.insert(window.duration, window);
            }
        }
        self.capacity
            .entry(provider.to_string())
            .or_default()
            .windows = by_duration.into_values().collect();
    }

    pub fn note_balance(
        &mut self,
        provider: &str,
        is_available: bool,
        total: Option<f64>,
        observed_at: Instant,
    ) {
        let total = total.filter(|value| value.is_finite());
        self.capacity
            .entry(provider.to_string())
            .or_default()
            .balance = Some(BalanceSample {
            is_available,
            total,
            observed_at,
        });
    }

    /// Record `provider` for `model` when no live pin exists. A live pin is
    /// kept, including its original timestamp.
    pub fn remember(&mut self, model: &str, provider: &str, now: Instant) -> bool {
        if let Some(pin) = self.pins.get(model) {
            if now.saturating_duration_since(pin.recorded_at) <= PIN_TTL {
                return false;
            }
        }
        self.pins.insert(
            model.to_string(),
            Pin {
                provider_id: provider.to_string(),
                recorded_at: now,
            },
        );
        true
    }

    /// Drop the pin only when it still names `provider`.
    pub fn forget_if(&mut self, model: &str, provider: &str) -> bool {
        let matches = self
            .pins
            .get(model)
            .is_some_and(|pin| pin.provider_id == provider);
        if matches {
            self.pins.remove(model);
        }
        matches
    }

    pub fn pin(&self, model: &str) -> Option<&Pin> {
        self.pins.get(model)
    }

    /// Tightest fresh usage window for `provider`, if one exists.
    pub fn usage_summary(&self, provider: &str, now: Instant) -> Option<UsageSnapshot> {
        let capacity = self.capacity.get(provider)?;
        let windows = fresh_windows(capacity, now);
        let tightest = windows
            .iter()
            .map(|window| window.remaining_fraction)
            .min_by(f64::total_cmp)?;
        let reset_in_s = windows
            .iter()
            .filter_map(|window| window.reset_at)
            .min()
            .and_then(|reset_at| reset_at.checked_duration_since(now))
            .map(|duration| duration.as_secs());
        Some(UsageSnapshot {
            remaining_fraction: tightest,
            windows: windows.len(),
            reset_in_s,
        })
    }

    /// `candidates` is config order, already limited to providers that can
    /// serve this request. `closed` is true when that provider's circuit is
    /// `closed`. `excluded` are providers this request has already failed.
    ///
    /// When `use_pin` is false the stored pin is neither consulted nor dropped.
    pub fn plan(
        &self,
        model: &str,
        candidates: &[String],
        excluded: &HashSet<String>,
        closed: &HashMap<String, bool>,
        now: Instant,
        use_pin: bool,
    ) -> Plan {
        let mut drop = None;
        if use_pin {
            if let Some(pin) = self.pins.get(model) {
                let reason = if now.saturating_duration_since(pin.recorded_at) > PIN_TTL {
                    Some(PinDrop::Expired)
                } else if excluded.contains(&pin.provider_id) {
                    Some(PinDrop::Unreachable)
                } else if !candidates.iter().any(|id| id == &pin.provider_id) {
                    Some(PinDrop::Ineligible)
                } else if !self.healthy(
                    &pin.provider_id,
                    closed.get(&pin.provider_id).copied().unwrap_or(false),
                    now,
                ) {
                    Some(PinDrop::Unhealthy)
                } else {
                    None
                };
                if let Some(reason) = reason {
                    drop = Some((pin.provider_id.clone(), reason));
                } else {
                    return Plan {
                        provider_ids: vec![pin.provider_id.clone()],
                        kind: PlanKind::Pinned,
                        drop: None,
                    };
                }
            }
        }

        let eligible: Vec<(usize, &str)> = candidates
            .iter()
            .enumerate()
            .filter(|(_, id)| !excluded.contains(id.as_str()) && !self.usage_exhausted(id, now))
            .map(|(index, id)| (index, id.as_str()))
            .collect();
        let mut healthy: Vec<(usize, &str)> = eligible
            .iter()
            .copied()
            .filter(|(_, id)| self.healthy(id, closed.get(*id).copied().unwrap_or(false), now))
            .collect();
        if healthy.is_empty() {
            return Plan {
                provider_ids: eligible.iter().map(|(_, id)| (*id).to_string()).collect(),
                kind: PlanKind::Circuit,
                drop,
            };
        }
        healthy.sort_by(|left, right| {
            self.class(left.1, now)
                .cmp(&self.class(right.1, now))
                .then_with(|| self.cmp_usage(left.1, right.1, now))
                .then_with(|| left.0.cmp(&right.0))
        });
        Plan {
            provider_ids: healthy.iter().map(|(_, id)| (*id).to_string()).collect(),
            kind: PlanKind::Ranked,
            drop,
        }
    }

    fn usage_exhausted(&self, provider: &str, now: Instant) -> bool {
        self.capacity
            .get(provider)
            .map(|capacity| {
                fresh_windows(capacity, now)
                    .iter()
                    .any(|window| window.remaining_fraction <= 0.0)
            })
            .unwrap_or(false)
    }

    fn healthy(&self, provider: &str, circuit_closed: bool, now: Instant) -> bool {
        if !circuit_closed {
            return false;
        }
        let Some(capacity) = self.capacity.get(provider) else {
            return true;
        };
        let windows = fresh_windows(capacity, now);
        if !windows.is_empty() {
            return windows.iter().all(|window| window.remaining_fraction > 0.0);
        }
        if let Some(balance) = fresh_balance(capacity, now) {
            if !balance.is_available || balance.total.is_some_and(|total| total <= 0.0) {
                return false;
            }
        }
        true
    }

    /// 0 = usage, 1 = neither, 2 = balance only.
    fn class(&self, provider: &str, now: Instant) -> u8 {
        let Some(capacity) = self.capacity.get(provider) else {
            return 1;
        };
        if !fresh_windows(capacity, now).is_empty() {
            return 0;
        }
        let has_balance = fresh_balance(capacity, now).is_some_and(|balance| {
            balance.is_available && balance.total.is_none_or(|total| total > 0.0)
        });
        if has_balance {
            return 2;
        }
        1
    }

    /// Compare the sorted remaining fractions lexicographically. A provider
    /// without a given window counts as `INFINITY` there, so any real data
    /// sorts ahead of missing data at the same position.
    fn cmp_usage(&self, left: &str, right: &str, now: Instant) -> std::cmp::Ordering {
        let mut left_ratios = self
            .capacity
            .get(left)
            .map(|capacity| {
                fresh_windows(capacity, now)
                    .into_iter()
                    .map(|window| window.remaining_fraction)
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        let mut right_ratios = self
            .capacity
            .get(right)
            .map(|capacity| {
                fresh_windows(capacity, now)
                    .into_iter()
                    .map(|window| window.remaining_fraction)
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        left_ratios.sort_by(f64::total_cmp);
        right_ratios.sort_by(f64::total_cmp);
        for index in 0..left_ratios.len().max(right_ratios.len()) {
            let left_ratio = left_ratios.get(index).copied().unwrap_or(f64::INFINITY);
            let right_ratio = right_ratios.get(index).copied().unwrap_or(f64::INFINITY);
            let order = left_ratio.total_cmp(&right_ratio);
            if order != std::cmp::Ordering::Equal {
                return order;
            }
        }
        std::cmp::Ordering::Equal
    }
}

fn fresh_windows(capacity: &Capacity, now: Instant) -> Vec<&UsageWindow> {
    let mut by_duration: BTreeMap<Duration, &UsageWindow> = BTreeMap::new();
    for window in &capacity.windows {
        if window_fresh(window, now) {
            by_duration.insert(window.duration, window);
        }
    }
    by_duration.into_values().collect()
}

fn window_fresh(window: &UsageWindow, now: Instant) -> bool {
    if !window.remaining_fraction.is_finite() {
        return false;
    }
    if now.saturating_duration_since(window.observed_at) > USAGE_SNAPSHOT_TTL {
        return false;
    }
    window.reset_at.is_none_or(|reset_at| reset_at > now)
}

fn fresh_balance(capacity: &Capacity, now: Instant) -> Option<&BalanceSample> {
    capacity.balance.as_ref().filter(|balance| {
        now.saturating_duration_since(balance.observed_at) <= BALANCE_SNAPSHOT_TTL
    })
}

/// Backends in `provider_order`. Every backend of a provider is kept, in the
/// order it appears in `backends`, except pairs listed in `skipped`.
pub fn expand_backends(
    provider_order: &[String],
    backends: &[(String, String)],
    skipped: &HashSet<(String, String)>,
) -> Vec<(String, String)> {
    let mut out = Vec::new();
    for provider in provider_order {
        for (id, upstream) in backends {
            if id == provider && !skipped.contains(&(id.clone(), upstream.clone())) {
                out.push((id.clone(), upstream.clone()));
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn now() -> Instant {
        Instant::now()
    }

    fn earlier(now: Instant, age: Duration) -> Instant {
        now.checked_sub(age)
            .expect("test instant is old enough to subtract")
    }

    fn window(now: Instant, duration_s: u64, remaining_fraction: f64) -> UsageWindow {
        UsageWindow {
            duration: Duration::from_secs(duration_s),
            remaining_fraction,
            reset_at: Some(now + Duration::from_secs(duration_s)),
            observed_at: now,
        }
    }

    fn ids(names: &[&str]) -> Vec<String> {
        names.iter().map(|name| (*name).to_string()).collect()
    }

    fn all_closed(names: &[&str]) -> HashMap<String, bool> {
        names
            .iter()
            .map(|name| ((*name).to_string(), true))
            .collect()
    }

    #[test]
    fn usage_beats_unknown_and_unknown_beats_balance() {
        let now = now();
        let mut table = RouteTable::default();
        table.note_usage(
            "od-go",
            vec![window(now, 5 * 3600, 0.8), window(now, 7 * 86400, 0.2)],
        );
        table.note_usage("od-plus", vec![window(now, 7 * 86400, 0.5)]);
        table.note_balance("deepseek", true, Some(20.0), now);
        let plan = table.plan(
            "m",
            &ids(&["deepseek", "grok", "od-plus", "od-go"]),
            &HashSet::new(),
            &all_closed(&["deepseek", "grok", "od-plus", "od-go"]),
            now,
            true,
        );
        assert_eq!(plan.kind, PlanKind::Ranked);
        assert_eq!(
            plan.provider_ids,
            ids(&["od-go", "od-plus", "grok", "deepseek"])
        );
    }

    #[test]
    fn the_tightest_remaining_fraction_is_compared_first() {
        let now = now();
        let mut table = RouteTable::default();
        table.note_usage(
            "a",
            vec![window(now, 5 * 3600, 0.2), window(now, 7 * 86400, 0.01)],
        );
        table.note_usage(
            "b",
            vec![window(now, 5 * 3600, 0.1), window(now, 7 * 86400, 1.0)],
        );
        let plan = table.plan(
            "m",
            &ids(&["a", "b"]),
            &HashSet::new(),
            &all_closed(&["a", "b"]),
            now,
            true,
        );
        assert_eq!(plan.provider_ids, ids(&["a", "b"]));
    }

    #[test]
    fn the_second_smallest_fraction_breaks_a_tie() {
        let now = now();
        let mut table = RouteTable::default();
        table.note_usage(
            "a",
            vec![window(now, 5 * 3600, 0.1), window(now, 7 * 86400, 0.9)],
        );
        table.note_usage(
            "b",
            vec![window(now, 5 * 3600, 0.1), window(now, 7 * 86400, 0.4)],
        );
        let plan = table.plan(
            "m",
            &ids(&["a", "b"]),
            &HashSet::new(),
            &all_closed(&["a", "b"]),
            now,
            true,
        );
        assert_eq!(plan.provider_ids, ids(&["b", "a"]));
    }

    #[test]
    fn a_spent_window_is_not_healthy() {
        let now = now();
        let mut table = RouteTable::default();
        table.note_usage(
            "spent",
            vec![window(now, 5 * 3600, 0.0), window(now, 7 * 86400, 0.4)],
        );
        table.note_usage("left", vec![window(now, 5 * 3600, 0.3)]);
        let plan = table.plan(
            "m",
            &ids(&["spent", "left"]),
            &HashSet::new(),
            &all_closed(&["spent", "left"]),
            now,
            true,
        );
        assert_eq!(plan.kind, PlanKind::Ranked);
        assert_eq!(plan.provider_ids, ids(&["left"]));
    }

    #[test]
    fn a_spent_provider_is_skipped_even_when_nobody_else_is_healthy() {
        let now = now();
        let mut table = RouteTable::default();
        table.note_usage("spent", vec![window(now, 7 * 86400, 0.0)]);
        let plan = table.plan(
            "m",
            &ids(&["spent"]),
            &HashSet::new(),
            &all_closed(&["spent"]),
            now,
            true,
        );
        assert_eq!(plan.kind, PlanKind::Circuit);
        assert!(plan.provider_ids.is_empty());
    }

    #[test]
    fn an_unavailable_balance_is_not_healthy() {
        let now = now();
        let mut table = RouteTable::default();
        table.note_balance("deepseek", false, Some(0.0), now);
        let mut closed = all_closed(&["deepseek"]);
        closed.insert("other".into(), true);
        let plan = table.plan(
            "m",
            &ids(&["deepseek", "other"]),
            &HashSet::new(),
            &closed,
            now,
            true,
        );
        assert_eq!(plan.provider_ids, ids(&["other"]));
    }

    #[test]
    fn an_open_circuit_waits_until_nobody_healthy_remains() {
        let now = now();
        let table = RouteTable::default();
        let mut closed = all_closed(&["od", "deepseek"]);
        closed.insert("od".into(), false);
        let plan = table.plan(
            "m",
            &ids(&["od", "deepseek"]),
            &HashSet::new(),
            &closed,
            now,
            true,
        );
        assert_eq!(plan.kind, PlanKind::Ranked);
        assert_eq!(plan.provider_ids, ids(&["deepseek"]));

        closed.insert("deepseek".into(), false);
        let plan = table.plan(
            "m",
            &ids(&["od", "deepseek"]),
            &HashSet::new(),
            &closed,
            now,
            true,
        );
        assert_eq!(plan.kind, PlanKind::Circuit);
        assert_eq!(plan.provider_ids, ids(&["od", "deepseek"]));
    }

    #[test]
    fn a_window_without_a_reset_expires_after_the_snapshot_ttl() {
        let now = now();
        let mut table = RouteTable::default();
        table.note_usage(
            "od",
            vec![UsageWindow {
                duration: Duration::from_secs(5 * 3600),
                remaining_fraction: 1.0,
                reset_at: None,
                observed_at: earlier(now, USAGE_SNAPSHOT_TTL + Duration::from_secs(1)),
            }],
        );
        table.note_balance("deepseek", true, Some(5.0), now);
        let plan = table.plan(
            "m",
            &ids(&["od", "deepseek"]),
            &HashSet::new(),
            &all_closed(&["od", "deepseek"]),
            now,
            true,
        );
        assert_eq!(plan.provider_ids, ids(&["od", "deepseek"]));
    }

    #[test]
    fn a_window_past_its_reset_is_ignored_even_if_just_observed() {
        let now = now();
        let mut table = RouteTable::default();
        table.note_usage(
            "od",
            vec![UsageWindow {
                duration: Duration::from_secs(5 * 3600),
                remaining_fraction: 1.0,
                reset_at: Some(earlier(now, Duration::from_secs(1))),
                observed_at: now,
            }],
        );
        let plan = table.plan(
            "m",
            &ids(&["od", "plain"]),
            &HashSet::new(),
            &all_closed(&["od", "plain"]),
            now,
            true,
        );
        assert_eq!(plan.provider_ids, ids(&["od", "plain"]));
    }

    #[test]
    fn a_future_reset_does_not_rescue_a_stale_snapshot() {
        let now = now();
        let mut table = RouteTable::default();
        table.note_usage(
            "od",
            vec![UsageWindow {
                duration: Duration::from_secs(5 * 3600),
                remaining_fraction: 1.0,
                reset_at: Some(now + Duration::from_secs(3600)),
                observed_at: earlier(now, USAGE_SNAPSHOT_TTL + Duration::from_secs(1)),
            }],
        );
        let capacity = table.capacity.get("od").unwrap();
        assert!(fresh_windows(capacity, now).is_empty());
    }

    #[test]
    fn a_live_pin_is_the_only_candidate() {
        let now = now();
        let mut table = RouteTable::default();
        table.note_usage("od", vec![window(now, 5 * 3600, 1.0)]);
        table.note_usage("other", vec![window(now, 5 * 3600, 0.5)]);
        assert!(table.remember("m", "od", earlier(now, Duration::from_secs(60))));
        let plan = table.plan(
            "m",
            &ids(&["od", "other"]),
            &HashSet::new(),
            &all_closed(&["od", "other"]),
            now,
            true,
        );
        assert_eq!(plan.kind, PlanKind::Pinned);
        assert_eq!(plan.provider_ids, ids(&["od"]));
        assert!(plan.drop.is_none());
    }

    #[test]
    fn a_pin_at_exactly_five_hours_is_still_live() {
        let now = now();
        let mut table = RouteTable::default();
        table.remember("m", "od", earlier(now, PIN_TTL));
        let plan = table.plan(
            "m",
            &ids(&["od", "other"]),
            &HashSet::new(),
            &all_closed(&["od", "other"]),
            now,
            true,
        );
        assert_eq!(plan.kind, PlanKind::Pinned);
        assert_eq!(plan.provider_ids, ids(&["od"]));
    }

    #[test]
    fn a_pin_older_than_five_hours_is_dropped_and_selection_runs() {
        let now = now();
        let mut table = RouteTable::default();
        table.note_usage("fresh", vec![window(now, 5 * 3600, 1.0)]);
        table.remember("m", "stale", earlier(now, PIN_TTL + Duration::from_secs(1)));
        let plan = table.plan(
            "m",
            &ids(&["stale", "fresh"]),
            &HashSet::new(),
            &all_closed(&["stale", "fresh"]),
            now,
            true,
        );
        assert_eq!(plan.kind, PlanKind::Ranked);
        assert_eq!(plan.provider_ids, ids(&["fresh", "stale"]));
        assert_eq!(plan.drop, Some(("stale".to_string(), PinDrop::Expired)));
    }

    #[test]
    fn an_unhealthy_pin_is_dropped() {
        let now = now();
        let mut table = RouteTable::default();
        table.remember("m", "od", now);
        let mut closed = all_closed(&["od", "other"]);
        closed.insert("od".into(), false);
        let plan = table.plan(
            "m",
            &ids(&["od", "other"]),
            &HashSet::new(),
            &closed,
            now,
            true,
        );
        assert_eq!(plan.provider_ids, ids(&["other"]));
        assert_eq!(plan.drop, Some(("od".into(), PinDrop::Unhealthy)));
    }

    #[test]
    fn a_pin_the_request_cannot_use_is_ineligible() {
        let now = now();
        let mut table = RouteTable::default();
        table.remember("m", "grok", now);
        let plan = table.plan(
            "m",
            &ids(&["od"]),
            &HashSet::new(),
            &all_closed(&["od"]),
            now,
            true,
        );
        assert_eq!(plan.provider_ids, ids(&["od"]));
        assert_eq!(plan.drop, Some(("grok".into(), PinDrop::Ineligible)));
    }

    #[test]
    fn remember_does_not_refresh_or_replace_a_live_pin() {
        let now = now();
        let mut table = RouteTable::default();
        let recorded = earlier(now, Duration::from_secs(30));
        assert!(table.remember("m", "od", recorded));
        assert!(!table.remember("m", "od", now));
        assert!(!table.remember("m", "other", now));
        let pin = table.pin("m").unwrap();
        assert_eq!(pin.provider_id, "od");
        assert_eq!(pin.recorded_at, recorded);
    }

    #[test]
    fn forget_if_leaves_a_different_provider_in_place() {
        let now = now();
        let mut table = RouteTable::default();
        table.remember("m", "od", now);
        assert!(!table.forget_if("m", "other"));
        assert!(table.pin("m").is_some());
        assert!(table.forget_if("m", "od"));
        assert!(table.pin("m").is_none());
    }

    #[test]
    fn config_order_breaks_ties_inside_a_class() {
        let now = now();
        let table = RouteTable::default();
        let plan = table.plan(
            "m",
            &ids(&["second", "first"]),
            &HashSet::new(),
            &all_closed(&["second", "first"]),
            now,
            true,
        );
        assert_eq!(plan.provider_ids, ids(&["second", "first"]));
    }

    #[test]
    fn ignoring_the_pin_leaves_it_in_place() {
        let now = now();
        let mut table = RouteTable::default();
        table.remember("m", "od", now);
        let plan = table.plan(
            "m",
            &ids(&["deepseek"]),
            &HashSet::new(),
            &all_closed(&["deepseek"]),
            now,
            false,
        );
        assert!(plan.drop.is_none());
        assert_eq!(plan.provider_ids, ids(&["deepseek"]));
        assert_eq!(table.pin("m").unwrap().provider_id, "od");
    }

    #[test]
    fn expand_keeps_later_backends_of_the_same_provider() {
        let backends = vec![
            ("od".to_string(), "first".to_string()),
            ("od".to_string(), "second".to_string()),
            ("deepseek".to_string(), "flash".to_string()),
        ];
        let mut skipped = HashSet::new();
        skipped.insert(("od".to_string(), "first".to_string()));
        let order = ids(&["od", "deepseek"]);
        assert_eq!(
            expand_backends(&order, &backends, &skipped),
            vec![
                ("od".to_string(), "second".to_string()),
                ("deepseek".to_string(), "flash".to_string()),
            ]
        );
    }
}
