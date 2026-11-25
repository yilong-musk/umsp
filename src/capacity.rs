//! Provider capacity facts shared between billing/balance observers and the
//! routing layer.
//!
//! Living in a neutral module avoids inverting dependencies: vendor billing
//! pollers (e.g. `grok_billing`) produce [`UsageWindow`] values without
//! importing [`crate::route`].

use std::time::{Duration, Instant};

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
