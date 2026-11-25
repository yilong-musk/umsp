//! Per-client runtime limits.
//!
//! Concurrency: a client with `max_concurrency = N` may have at most N model
//! calls in flight. A slot is held from admission until the response body is
//! fully sent or dropped, so a long SSE stream occupies its slot for its whole
//! life. The limit is read from the live config on every admission, so a
//! `SIGHUP` reload takes effect for the next request; in-flight counts live in
//! the process (they are meaningless across restarts) and survive reloads.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, PoisonError};

/// Accepted range for `max_concurrency`.
pub const MAX_CONCURRENCY_RANGE: std::ops::RangeInclusive<u32> = 1..=9;

#[derive(Debug, Default)]
pub struct ConcurrencyLimiter {
    inflight: Mutex<HashMap<String, u32>>,
}

impl ConcurrencyLimiter {
    /// Take one slot for `client` if fewer than `limit` are in use.
    pub fn try_acquire(self: &Arc<Self>, client: &str, limit: u32) -> Option<ConcurrencyPermit> {
        let mut map = self.inflight.lock().unwrap_or_else(PoisonError::into_inner);
        let used = map.entry(client.to_string()).or_insert(0);
        if *used >= limit {
            return None;
        }
        *used += 1;
        Some(ConcurrencyPermit {
            limiter: Arc::clone(self),
            client: client.to_string(),
        })
    }

    /// Slots currently held by `client`.
    pub fn in_flight(&self, client: &str) -> u32 {
        let map = self.inflight.lock().unwrap_or_else(PoisonError::into_inner);
        map.get(client).copied().unwrap_or(0)
    }
}

/// One in-flight slot; released on drop.
#[derive(Debug)]
pub struct ConcurrencyPermit {
    limiter: Arc<ConcurrencyLimiter>,
    client: String,
}

impl Drop for ConcurrencyPermit {
    fn drop(&mut self) {
        let mut map = self
            .limiter
            .inflight
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        if let Some(used) = map.get_mut(&self.client) {
            *used = used.saturating_sub(1);
            if *used == 0 {
                map.remove(&self.client);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn permits_are_capped_per_client_and_released_on_drop() {
        let limiter = Arc::new(ConcurrencyLimiter::default());
        let first = limiter.try_acquire("a", 1).expect("first slot");
        assert!(limiter.try_acquire("a", 1).is_none(), "limit 1 is full");
        // Other clients are independent.
        let other = limiter.try_acquire("b", 1).expect("b has its own slot");
        assert_eq!(limiter.in_flight("a"), 1);
        drop(first);
        assert_eq!(limiter.in_flight("a"), 0);
        assert!(limiter.try_acquire("a", 1).is_some());
        drop(other);
        assert_eq!(limiter.in_flight("b"), 0);
    }

    #[test]
    fn a_lowered_limit_applies_to_the_next_admission() {
        let limiter = Arc::new(ConcurrencyLimiter::default());
        let p1 = limiter.try_acquire("a", 3).unwrap();
        let p2 = limiter.try_acquire("a", 3).unwrap();
        // Reload lowers the limit to 2: no new slot until one is released.
        assert!(limiter.try_acquire("a", 2).is_none());
        drop(p1);
        assert!(limiter.try_acquire("a", 2).is_some());
        drop(p2);
    }
}
