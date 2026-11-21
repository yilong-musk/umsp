//! Provider selection and per-model stickiness.
//!
//! Selection reads only in-memory capacity facts. It does not call upstreams.
//! A missing or stale balance observation is not evidence against a provider.
//!
//! A provider is healthy when its circuit is closed and, if a fresh balance
//! observation exists, that balance is available and positive. Healthy
//! providers are tried in configuration order; a live pin for the model is
//! tried first. Balance is a health gate, not a ranking signal: an exhausted
//! account is skipped, but otherwise the configured order decides.
//!
//! A model remembers at most one provider for five hours, measured from the
//! moment the record was written. Later successes do not extend it. The record
//! is replaced only when that provider can no longer take a request, or when
//! the five hours have elapsed.

use std::collections::{HashMap, HashSet};
use std::time::{Duration, Instant};

/// How long a per-model provider record stays in force.
pub const PIN_TTL: Duration = Duration::from_secs(5 * 60 * 60);
/// Balance is refreshed on a five-minute poll, so the snapshot outlives one miss.
pub const BALANCE_SNAPSHOT_TTL: Duration = Duration::from_secs(10 * 60);

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
    pub balance: Option<BalanceSample>,
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
            .filter(|(_, id)| !excluded.contains(id.as_str()))
            .map(|(index, id)| (index, id.as_str()))
            .collect();
        let healthy: Vec<(usize, &str)> = eligible
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
        Plan {
            provider_ids: healthy.iter().map(|(_, id)| (*id).to_string()).collect(),
            kind: PlanKind::Ranked,
            drop,
        }
    }

    fn healthy(&self, provider: &str, circuit_closed: bool, now: Instant) -> bool {
        if !circuit_closed {
            return false;
        }
        let Some(balance) = self
            .capacity
            .get(provider)
            .and_then(|capacity| fresh_balance(capacity, now))
        else {
            return true;
        };
        balance.is_available && balance.total.is_none_or(|total| total > 0.0)
    }
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
    fn an_unavailable_balance_is_not_healthy() {
        let now = now();
        let mut table = RouteTable::default();
        table.note_balance("deepseek", false, Some(0.0), now);
        let plan = table.plan(
            "m",
            &ids(&["deepseek", "other"]),
            &HashSet::new(),
            &all_closed(&["deepseek", "other"]),
            now,
            true,
        );
        assert_eq!(plan.provider_ids, ids(&["other"]));
    }

    #[test]
    fn a_zero_balance_is_not_healthy() {
        let now = now();
        let mut table = RouteTable::default();
        table.note_balance("deepseek", true, Some(0.0), now);
        let plan = table.plan(
            "m",
            &ids(&["deepseek", "other"]),
            &HashSet::new(),
            &all_closed(&["deepseek", "other"]),
            now,
            true,
        );
        assert_eq!(plan.provider_ids, ids(&["other"]));
    }

    #[test]
    fn a_positive_balance_keeps_config_order() {
        let now = now();
        let mut table = RouteTable::default();
        table.note_balance("deepseek", true, Some(5.0), now);
        let plan = table.plan(
            "m",
            &ids(&["od", "deepseek"]),
            &HashSet::new(),
            &all_closed(&["od", "deepseek"]),
            now,
            true,
        );
        assert_eq!(plan.kind, PlanKind::Ranked);
        assert_eq!(plan.provider_ids, ids(&["od", "deepseek"]));
    }

    #[test]
    fn a_stale_balance_is_ignored() {
        let now = now();
        let mut table = RouteTable::default();
        table.note_balance(
            "deepseek",
            false,
            Some(0.0),
            earlier(now, BALANCE_SNAPSHOT_TTL + Duration::from_secs(1)),
        );
        let plan = table.plan(
            "m",
            &ids(&["deepseek", "other"]),
            &HashSet::new(),
            &all_closed(&["deepseek", "other"]),
            now,
            true,
        );
        assert_eq!(plan.provider_ids, ids(&["deepseek", "other"]));
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
    fn a_live_pin_is_the_only_candidate() {
        let now = now();
        let mut table = RouteTable::default();
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
        assert_eq!(plan.provider_ids, ids(&["stale", "fresh"]));
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
    fn config_order_breaks_ties() {
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
