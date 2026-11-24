//! Client key device binding: trust-on-first-use (TOFU) on the source IP.
//!
//! A client key is created unbound. The first authenticated request binds it
//! to that request's effective source IP; every later request must come from
//! the same IP or it is rejected with `device_mismatch`. Bindings live in
//! `{state_dir}/client-bindings.json` (not in config.toml: the data plane owns
//! this state and must never rewrite the operator's config).
//!
//! Effective IP resolution: UMSP listens on loopback, so public requests
//! arrive through the trusted reverse proxy with peer 127.0.0.1. The proxy
//! sets `X-Forwarded-For` to the real client address; only the rightmost
//! entry is trusted (anything left of it is client-supplied). XFF is ignored
//! entirely for non-loopback peers. Requests with no socket peer at all (the
//! in-process router used by tests) are treated as loopback.
//!
//! Recovery: `umsp client unbind <name>` clears a binding (next request
//! re-binds), `umsp client rotate` clears it implicitly (new key, new
//! identity). A binding that cannot be persisted fails the request closed:
//! serving it would silently reset TOFU on the next restart.

use std::collections::HashMap;
use std::net::{IpAddr, Ipv4Addr};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use serde_json::json;

pub const BINDINGS_FILE: &str = "client-bindings.json";

/// Outcome of checking a presented client IP against the stored binding.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BindOutcome {
    /// The request may proceed. `first_use` is true when this request created
    /// the binding.
    Allowed { first_use: bool },
    /// The key is bound to a different IP. The bound IP is for server-side
    /// logs only — never include it in the client-facing error.
    Mismatch { bound_ip: String },
    /// The new binding could not be written to disk; fail closed.
    PersistFailed,
}

#[derive(Debug)]
pub struct BindingStore {
    path: PathBuf,
    map: Mutex<HashMap<String, String>>,
}

impl BindingStore {
    /// Load `{state_dir}/client-bindings.json`. A missing file is an empty
    /// store; a corrupt file also starts empty (with an operational warning)
    /// so one bad write cannot wedge the whole data plane.
    pub fn load(state_dir: &Path) -> Self {
        let path = state_dir.join(BINDINGS_FILE);
        let map = match std::fs::read_to_string(&path) {
            Ok(text) => {
                serde_json::from_str::<HashMap<String, String>>(&text).unwrap_or_else(|err| {
                    eprintln!(
                        "{}",
                        json!({
                            "ts": crate::util::now_rfc3339(),
                            "level": "warn",
                            "event": "bindings_file_corrupt",
                            "path": path.display().to_string(),
                            "error": err.to_string(),
                        })
                    );
                    HashMap::new()
                })
            }
            Err(_) => HashMap::new(),
        };
        Self {
            path,
            map: Mutex::new(map),
        }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Check the presented IP, binding the key on first use. The check and
    /// the bind happen under one lock, so concurrent first requests from
    /// different IPs have a single winner.
    pub fn check_or_bind(&self, client: &str, ip: &str) -> BindOutcome {
        let mut map = self
            .map
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(bound) = map.get(client) {
            return if bound == ip {
                BindOutcome::Allowed { first_use: false }
            } else {
                BindOutcome::Mismatch {
                    bound_ip: bound.clone(),
                }
            };
        }
        let mut next = map.clone();
        next.insert(client.to_string(), ip.to_string());
        if self.persist_locked(&next).is_err() {
            return BindOutcome::PersistFailed;
        }
        *map = next;
        BindOutcome::Allowed { first_use: true }
    }

    /// Remove a binding. `Ok(false)` means the key was not bound. The map is
    /// only updated after the file write succeeds.
    pub fn unbind(&self, client: &str) -> std::io::Result<bool> {
        let mut map = self
            .map
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if !map.contains_key(client) {
            return Ok(false);
        }
        let mut next = map.clone();
        next.remove(client);
        self.persist_locked(&next)?;
        *map = next;
        Ok(true)
    }

    pub fn bound_ip(&self, client: &str) -> Option<String> {
        let map = self
            .map
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        map.get(client).cloned()
    }

    pub fn snapshot(&self) -> HashMap<String, String> {
        let map = self
            .map
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        map.clone()
    }

    /// Atomic, owner-only write (client names and source IPs).
    fn persist_locked(&self, map: &HashMap<String, String>) -> std::io::Result<()> {
        let text = serde_json::to_string_pretty(map).map_err(std::io::Error::other)?;
        crate::util::atomic_write(&self.path, text.as_bytes(), 0o600)
    }
}

/// Resolve the effective client IP for binding purposes.
///
/// `peer` is the socket peer address (`None` for the in-process test router,
/// treated as loopback). `xff` is the raw `X-Forwarded-For` header value, if
/// any. XFF is trusted only from loopback peers (the reverse proxy), and only
/// its rightmost entry — the one the proxy itself wrote. A rightmost entry
/// that does not parse falls back to the peer address; entries further left
/// are client-controlled and never consulted.
pub fn effective_client_ip(peer: Option<IpAddr>, xff: Option<&str>) -> IpAddr {
    let peer = peer.unwrap_or(IpAddr::V4(Ipv4Addr::LOCALHOST));
    if peer.is_loopback() {
        if let Some(ip) = xff
            .and_then(|value| value.rsplit(',').next())
            .map(str::trim)
            .and_then(|entry| entry.parse::<IpAddr>().ok())
        {
            return ip;
        }
    }
    peer
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Fixed per-test path under TMPDIR (a fresh name per run leaked dirs).
    fn temp_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("umsp-bind-{tag}"));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn first_use_binds_and_same_ip_is_allowed() {
        let dir = temp_dir("first-use");
        let store = BindingStore::load(&dir);
        assert_eq!(
            store.check_or_bind("pc", "1.2.3.4"),
            BindOutcome::Allowed { first_use: true }
        );
        assert_eq!(
            store.check_or_bind("pc", "1.2.3.4"),
            BindOutcome::Allowed { first_use: false }
        );
        assert_eq!(store.bound_ip("pc").as_deref(), Some("1.2.3.4"));
    }

    #[test]
    fn other_ip_is_rejected_and_bound_ip_is_reported() {
        let dir = temp_dir("mismatch");
        let store = BindingStore::load(&dir);
        store.check_or_bind("pc", "1.2.3.4");
        assert_eq!(
            store.check_or_bind("pc", "9.9.9.9"),
            BindOutcome::Mismatch {
                bound_ip: "1.2.3.4".into()
            }
        );
        // The rejected request must not move the binding.
        assert_eq!(store.bound_ip("pc").as_deref(), Some("1.2.3.4"));
    }

    #[test]
    fn different_clients_bind_independently() {
        let dir = temp_dir("independent");
        let store = BindingStore::load(&dir);
        store.check_or_bind("pc", "1.2.3.4");
        assert_eq!(
            store.check_or_bind("laptop", "9.9.9.9"),
            BindOutcome::Allowed { first_use: true }
        );
        assert_eq!(
            store.check_or_bind("pc", "9.9.9.9"),
            BindOutcome::Mismatch {
                bound_ip: "1.2.3.4".into()
            }
        );
    }

    #[test]
    fn unbind_allows_rebind_to_a_new_ip() {
        let dir = temp_dir("unbind");
        let store = BindingStore::load(&dir);
        store.check_or_bind("pc", "1.2.3.4");
        assert!(store.unbind("pc").unwrap());
        assert!(!store.unbind("pc").unwrap()); // idempotent
        assert_eq!(store.bound_ip("pc"), None);
        assert_eq!(
            store.check_or_bind("pc", "9.9.9.9"),
            BindOutcome::Allowed { first_use: true }
        );
    }

    #[test]
    fn bindings_persist_across_load() {
        let dir = temp_dir("persist");
        let store = BindingStore::load(&dir);
        store.check_or_bind("pc", "1.2.3.4");
        drop(store);
        let reloaded = BindingStore::load(&dir);
        assert_eq!(
            reloaded.check_or_bind("pc", "9.9.9.9"),
            BindOutcome::Mismatch {
                bound_ip: "1.2.3.4".into()
            }
        );
        reloaded.unbind("pc").unwrap();
        drop(reloaded);
        assert_eq!(BindingStore::load(&dir).bound_ip("pc"), None);
    }

    #[test]
    fn missing_or_corrupt_file_starts_empty() {
        let dir = temp_dir("corrupt");
        let store = BindingStore::load(&dir);
        assert_eq!(store.snapshot().len(), 0);
        std::fs::write(dir.join(BINDINGS_FILE), b"{not json").unwrap();
        let store = BindingStore::load(&dir);
        assert_eq!(store.snapshot().len(), 0);
        assert_eq!(
            store.check_or_bind("pc", "1.2.3.4"),
            BindOutcome::Allowed { first_use: true }
        );
    }

    #[test]
    fn concurrent_first_use_has_a_single_winner() {
        let dir = temp_dir("race");
        let store = std::sync::Arc::new(BindingStore::load(&dir));
        let mut handles = Vec::new();
        for i in 0..8 {
            let store = store.clone();
            handles.push(std::thread::spawn(move || {
                store.check_or_bind("pc", &format!("10.0.0.{i}"))
            }));
        }
        let outcomes: Vec<BindOutcome> = handles.into_iter().map(|h| h.join().unwrap()).collect();
        let winners = outcomes
            .iter()
            .filter(|o| matches!(o, BindOutcome::Allowed { first_use: true }))
            .count();
        assert_eq!(winners, 1);
        let bound = store.bound_ip("pc").unwrap();
        for outcome in &outcomes {
            match outcome {
                BindOutcome::Allowed { first_use: false } => {}
                BindOutcome::Allowed { first_use: true } => {}
                BindOutcome::Mismatch { bound_ip } => assert_eq!(bound_ip, &bound),
                BindOutcome::PersistFailed => panic!("unexpected persist failure"),
            }
        }
    }

    fn ip(s: &str) -> IpAddr {
        s.parse().unwrap()
    }

    #[test]
    fn effective_ip_without_peer_is_loopback() {
        assert_eq!(effective_client_ip(None, None), ip("127.0.0.1"));
        assert_eq!(effective_client_ip(None, Some("9.9.9.9")), ip("9.9.9.9"));
    }

    #[test]
    fn effective_ip_trusts_only_the_rightmost_xff_entry() {
        let peer = Some(ip("127.0.0.1"));
        assert_eq!(effective_client_ip(peer, Some("9.9.9.9")), ip("9.9.9.9"));
        // Left entries are client-supplied spoofing; the proxy wrote the last.
        assert_eq!(
            effective_client_ip(peer, Some("1.2.3.4, 5.6.7.8, 9.9.9.9")),
            ip("9.9.9.9")
        );
        assert_eq!(
            effective_client_ip(peer, Some("9.9.9.9,  8.8.8.8 ")),
            ip("8.8.8.8")
        );
    }

    #[test]
    fn effective_ip_falls_back_to_peer_on_garbage_xff() {
        let peer = Some(ip("127.0.0.1"));
        assert_eq!(effective_client_ip(peer, Some("unknown")), ip("127.0.0.1"));
        assert_eq!(effective_client_ip(peer, Some("")), ip("127.0.0.1"));
        assert_eq!(effective_client_ip(peer, Some("1.2.3.4")), ip("1.2.3.4"));
        // A non-parsing rightmost entry must not fall through to the left.
        assert_eq!(
            effective_client_ip(peer, Some("9.9.9.9, unknown")),
            ip("127.0.0.1")
        );
    }

    #[test]
    fn effective_ip_ignores_xff_from_non_loopback_peers() {
        let peer = Some(ip("10.1.2.3"));
        assert_eq!(effective_client_ip(peer, Some("9.9.9.9")), ip("10.1.2.3"));
    }

    #[test]
    fn effective_ip_handles_ipv6() {
        assert_eq!(
            effective_client_ip(Some(ip("::1")), Some("2001:db8::1")),
            ip("2001:db8::1")
        );
        assert_eq!(effective_client_ip(None, None), ip("127.0.0.1"));
        assert_eq!(effective_client_ip(Some(ip("::1")), None), ip("::1"));
    }
}
