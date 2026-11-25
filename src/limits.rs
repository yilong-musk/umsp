//! Per-client runtime limits.
//!
//! Concurrency: a client with `max_concurrency = N` may have at most N model
//! calls in flight. A slot is held from admission until the response body is
//! fully sent or dropped, so a long SSE stream occupies its slot for its whole
//! life. The limit is read from the live config on every admission, so a
//! `SIGHUP` reload takes effect for the next request; in-flight counts live in
//! the process (they are meaningless across restarts) and survive reloads.
//!
//! Token budgets: a client with `budget_tokens = N` may consume N tokens in
//! total (no periodic reset). Before a model call the requested output cap
//! (`max_tokens` and friends) is reserved, and a call whose cap does not fit
//! in `budget - used - reserved` is refused, so output never overruns the
//! budget. Input tokens are not reserved (the gateway has no tokenizer): the
//! last admitted call may overshoot by its own input. When the call finishes
//! its real usage is charged and the reservation released. A call that
//! reached the upstream but reported no usage (the client disconnected, or
//! the upstream omitted it) is charged its reservation instead, so walking
//! away before the final usage frame is not free.
//!
//! Consumption persists in `{state_dir}/client-usage.json`. Every charge
//! re-reads the file and adds to the value on disk, so a CLI reset written
//! while the server runs is never overwritten by a stale in-memory total.
//! Each read-modify-write holds an exclusive `flock` on
//! `client-usage.json.lock`, which the CLI takes too, so a charge and a reset
//! from another process cannot interleave. File I/O runs under its own lock,
//! never under the one admission checks take, so a slow disk does not stall
//! reservations.

use std::collections::HashMap;
use std::fs::{File, OpenOptions};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

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

pub const USAGE_FILE: &str = "client-usage.json";

/// How long a reservation amount waits for its call's ledger row before it
/// is forgotten. Every admitted call writes a row, so this only bounds memory
/// if one is ever lost.
const PENDING_TTL: Duration = Duration::from_secs(24 * 60 * 60);

/// Why a reservation was refused.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BudgetDenied {
    /// Nothing left: `used + reserved >= budget`.
    Exhausted { used: u64, budget: u64 },
    /// Some budget left, but less than the requested output cap.
    Insufficient { requested: u64, available: u64 },
}

impl BudgetDenied {
    pub fn message(&self) -> String {
        match self {
            BudgetDenied::Exhausted { used, budget } => format!(
                "this key has used its token budget ({used} of {budget}); ask the gateway admin to raise or reset it"
            ),
            BudgetDenied::Insufficient {
                requested,
                available,
            } => format!(
                "this call may produce up to {requested} output tokens but this key's budget has {available} tokens left; set a lower max_tokens"
            ),
        }
    }
}

#[derive(Debug)]
pub struct BudgetStore {
    inner: Mutex<BudgetInner>,
    /// Serializes this process's file read-modify-writes. Taken before
    /// `inner` when both are needed, and `inner` is never held across I/O.
    io: Mutex<()>,
}

#[derive(Debug)]
struct BudgetInner {
    path: PathBuf,
    used: HashMap<String, u64>,
    reserved: HashMap<String, u64>,
    /// Reserved amount per request id, kept until `settle` so a call whose
    /// usage never arrives can still be charged what it reserved — even when
    /// its response body (and so the reservation) is dropped first.
    pending: HashMap<String, (u64, Instant)>,
}

impl BudgetStore {
    /// Load `{state_dir}/client-usage.json`. A missing file is empty; a
    /// corrupt one also starts empty, with an operational warning.
    pub fn load(state_dir: &Path) -> Self {
        let path = state_dir.join(USAGE_FILE);
        let used = read_usage(&path).unwrap_or_default();
        Self {
            inner: Mutex::new(BudgetInner {
                path,
                used,
                reserved: HashMap::new(),
                pending: HashMap::new(),
            }),
            io: Mutex::new(()),
        }
    }

    /// Re-read consumption from `state_dir` (SIGHUP). In-flight reservations
    /// are kept: their calls are still running.
    pub fn reload(&self, state_dir: &Path) {
        let path = state_dir.join(USAGE_FILE);
        let _io = self.io_lock();
        let used = {
            let _file = UsageFileLock::acquire(&path);
            read_usage(&path)
        };
        let mut inner = self.lock();
        inner.path = path;
        if let Some(used) = used {
            inner.used = used;
        }
    }

    /// Tokens charged to `client` so far.
    pub fn used(&self, client: &str) -> u64 {
        self.lock().used.get(client).copied().unwrap_or(0)
    }

    /// Reserve `amount` output tokens against `budget` for call
    /// `request_id`. The check and the reservation happen under one lock, so
    /// concurrent calls cannot overbook.
    pub fn try_reserve(
        self: &Arc<Self>,
        client: &str,
        budget: u64,
        amount: u64,
        request_id: &str,
    ) -> Result<BudgetReservation, BudgetDenied> {
        let mut inner = self.lock();
        let used = inner.used.get(client).copied().unwrap_or(0);
        let reserved = inner.reserved.get(client).copied().unwrap_or(0);
        let committed = used.saturating_add(reserved);
        if committed >= budget {
            return Err(BudgetDenied::Exhausted { used, budget });
        }
        let available = budget - committed;
        if amount > available {
            return Err(BudgetDenied::Insufficient {
                requested: amount,
                available,
            });
        }
        *inner.reserved.entry(client.to_string()).or_insert(0) += amount;
        let now = Instant::now();
        inner
            .pending
            .retain(|_, (_, at)| now.duration_since(*at) < PENDING_TTL);
        inner.pending.insert(request_id.to_string(), (amount, now));
        Ok(BudgetReservation {
            store: Arc::clone(self),
            client: client.to_string(),
            amount,
        })
    }

    /// The amount call `request_id` reserved (0 if none), forgotten from
    /// here on. Called once per finished call, when its ledger row is written.
    pub fn settle(&self, request_id: &str) -> u64 {
        self.lock()
            .pending
            .remove(request_id)
            .map_or(0, |(amount, _)| amount)
    }

    /// Add `tokens` to `client`'s consumption on disk and in memory. The
    /// in-memory total is updated even when the write fails, so admission
    /// stays strict for the life of the process.
    pub fn charge(&self, client: &str, tokens: u64) -> std::io::Result<u64> {
        self.read_modify_write(|map| {
            let total = map.get(client).copied().unwrap_or(0).saturating_add(tokens);
            map.insert(client.to_string(), total);
            (true, total)
        })
    }

    /// Forget `client`'s consumption. `Ok(false)` means nothing was recorded.
    pub fn reset(&self, client: &str) -> std::io::Result<bool> {
        self.read_modify_write(|map| {
            let had = map.remove(client).is_some();
            (had, had)
        })
    }

    /// Apply `edit` to the consumption on disk (falling back to memory when
    /// the file is corrupt), write it back when `edit` says it changed, and
    /// adopt the result in memory. The admission lock is only taken to copy
    /// the state in and out, never across the file I/O.
    fn read_modify_write<T>(
        &self,
        edit: impl FnOnce(&mut HashMap<String, u64>) -> (bool, T),
    ) -> std::io::Result<T> {
        let _io = self.io_lock();
        let (path, snapshot) = {
            let inner = self.lock();
            (inner.path.clone(), inner.used.clone())
        };
        let file_lock = UsageFileLock::acquire(&path);
        let mut map = read_usage(&path).unwrap_or(snapshot);
        let (changed, out) = edit(&mut map);
        let written = match (&file_lock, changed) {
            (_, false) => Ok(()),
            (Err(err), true) => Err(std::io::Error::new(err.kind(), err.to_string())),
            (Ok(_), true) => write_usage(&path, &map),
        };
        drop(file_lock);
        self.lock().used = map;
        written.map(|()| out)
    }

    fn io_lock(&self) -> std::sync::MutexGuard<'_, ()> {
        self.io.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, BudgetInner> {
        self.inner.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

/// An output-token reservation; released on drop (after the call's real
/// usage has been charged).
#[derive(Debug)]
pub struct BudgetReservation {
    store: Arc<BudgetStore>,
    client: String,
    amount: u64,
}

impl Drop for BudgetReservation {
    fn drop(&mut self) {
        let mut inner = self.store.lock();
        if let Some(reserved) = inner.reserved.get_mut(&self.client) {
            *reserved = reserved.saturating_sub(self.amount);
            if *reserved == 0 {
                inner.reserved.remove(&self.client);
            }
        }
    }
}

/// Exclusive advisory lock on `client-usage.json.lock`, shared by the server
/// and the CLI; released on drop.
struct UsageFileLock {
    _file: File,
}

impl UsageFileLock {
    fn acquire(usage_path: &Path) -> std::io::Result<Self> {
        use std::os::fd::AsRawFd;
        use std::os::unix::fs::OpenOptionsExt;

        if let Some(dir) = usage_path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let mut lock_path = usage_path.as_os_str().to_owned();
        lock_path.push(".lock");
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .mode(0o600)
            .open(PathBuf::from(lock_path))?;
        loop {
            // SAFETY: `file` owns a valid descriptor for the duration of the call.
            if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX) } == 0 {
                return Ok(Self { _file: file });
            }
            let err = std::io::Error::last_os_error();
            if err.kind() != std::io::ErrorKind::Interrupted {
                return Err(err);
            }
        }
    }
}

/// `Some(map)` for a readable file (missing = empty), `None` when corrupt.
fn read_usage(path: &Path) -> Option<HashMap<String, u64>> {
    match std::fs::read_to_string(path) {
        Ok(text) => match serde_json::from_str(&text) {
            Ok(map) => Some(map),
            Err(err) => {
                eprintln!(
                    "{}",
                    serde_json::json!({
                        "ts": crate::util::now_rfc3339(),
                        "level": "warn",
                        "event": "client_usage_file_corrupt",
                        "path": path.display().to_string(),
                        "error": err.to_string(),
                    })
                );
                None
            }
        },
        Err(_) => Some(HashMap::new()),
    }
}

/// Atomic, owner-only write.
fn write_usage(path: &Path, map: &HashMap<String, u64>) -> std::io::Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let text = serde_json::to_string_pretty(map).map_err(std::io::Error::other)?;
    crate::util::atomic_write(path, text.as_bytes(), 0o600)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn usage_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("umsp-usage-{tag}"));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn reservations_never_overbook_the_output_budget() {
        let store = Arc::new(BudgetStore::load(&usage_dir("overbook")));
        let a = store.try_reserve("c", 100, 60, "r").expect("fits");
        assert_eq!(
            store.try_reserve("c", 100, 50, "r").unwrap_err(),
            BudgetDenied::Insufficient {
                requested: 50,
                available: 40
            }
        );
        let b = store.try_reserve("c", 100, 40, "r").expect("exactly fits");
        assert_eq!(
            store.try_reserve("c", 100, 0, "r").unwrap_err(),
            BudgetDenied::Exhausted {
                used: 0,
                budget: 100
            },
            "a full reservation book admits nothing, not even an uncapped call"
        );
        drop(a);
        drop(b);
        assert!(store.try_reserve("c", 100, 100, "r").is_ok());
    }

    #[test]
    fn settle_returns_the_reservation_even_after_it_is_dropped() {
        let store = Arc::new(BudgetStore::load(&usage_dir("settle")));
        let r = store.try_reserve("c", 100, 40, "req-1").unwrap();
        drop(r);
        assert_eq!(store.settle("req-1"), 40);
        assert_eq!(store.settle("req-1"), 0, "settled once");
        assert_eq!(store.settle("unknown"), 0);
    }

    #[test]
    fn charges_persist_and_exhaust() {
        let dir = usage_dir("charge");
        let store = Arc::new(BudgetStore::load(&dir));
        {
            let _r = store.try_reserve("c", 100, 10, "r").unwrap();
            assert_eq!(store.charge("c", 70).unwrap(), 70);
        }
        assert_eq!(store.used("c"), 70);
        // Input may overshoot: the last admitted call is charged in full.
        store.charge("c", 45).unwrap();
        assert_eq!(
            store.try_reserve("c", 100, 0, "r").unwrap_err(),
            BudgetDenied::Exhausted {
                used: 115,
                budget: 100
            }
        );
        // Survives a restart.
        assert_eq!(BudgetStore::load(&dir).used("c"), 115);
    }

    #[test]
    fn a_reset_on_disk_is_not_overwritten_by_the_next_charge() {
        let dir = usage_dir("reset-race");
        let server = BudgetStore::load(&dir);
        server.charge("c", 500).unwrap();
        // The CLI resets the file while the server keeps its stale total.
        assert!(BudgetStore::load(&dir).reset("c").unwrap());
        assert_eq!(server.used("c"), 500, "server has not reloaded yet");
        server.charge("c", 7).unwrap();
        assert_eq!(server.used("c"), 7);
        assert_eq!(BudgetStore::load(&dir).used("c"), 7);
    }

    #[test]
    fn charges_from_two_stores_on_one_file_add_up() {
        // Two processes (server and CLI) share the file through the lock.
        let dir = usage_dir("two-stores");
        let a = Arc::new(BudgetStore::load(&dir));
        let b = Arc::new(BudgetStore::load(&dir));
        let threads: Vec<_> = [a, b]
            .into_iter()
            .map(|store| {
                std::thread::spawn(move || {
                    for _ in 0..50 {
                        store.charge("c", 1).unwrap();
                    }
                })
            })
            .collect();
        for t in threads {
            t.join().unwrap();
        }
        assert_eq!(BudgetStore::load(&dir).used("c"), 100);
        assert!(dir.join("client-usage.json.lock").exists());
    }

    #[test]
    fn reload_keeps_in_flight_reservations() {
        let dir = usage_dir("reload");
        let store = Arc::new(BudgetStore::load(&dir));
        let _r = store.try_reserve("c", 100, 80, "r").unwrap();
        store.reload(&dir);
        assert!(store.try_reserve("c", 100, 30, "r").is_err());
    }

    #[test]
    fn a_corrupt_file_falls_back_to_memory_and_is_repaired() {
        let dir = usage_dir("corrupt");
        let store = BudgetStore::load(&dir);
        store.charge("c", 10).unwrap();
        std::fs::write(dir.join(USAGE_FILE), "{not json").unwrap();
        assert_eq!(store.charge("c", 5).unwrap(), 15);
        assert_eq!(BudgetStore::load(&dir).used("c"), 15);
    }

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
