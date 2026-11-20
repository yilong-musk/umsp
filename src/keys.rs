//! Client keys: generation, hashing and constant-time verification.
//!
//! Plaintext keys never touch disk: the config stores `key_hash` (SHA-256 hex)
//! plus a short `key_prefix` for display.

use base64::Engine;
use sha2::{Digest, Sha256};
use subtle::ConstantTimeEq;

use crate::config::Client;
use crate::util::{hex_decode, hex_encode};

const KEY_PREFIX: &str = "umsp_";
const KEY_BYTES: usize = 32;

/// `umsp_<32 random bytes as base64url>`
pub fn generate_key() -> String {
    let mut buf = [0u8; KEY_BYTES];
    getrandom::fill(&mut buf).expect("OS RNG unavailable");
    format!(
        "{KEY_PREFIX}{}",
        base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(buf)
    )
}

pub fn hash_key(plain: &str) -> String {
    let digest = Sha256::digest(plain.as_bytes());
    format!("sha256:{}", hex_encode(&digest))
}

/// Short display prefix, e.g. `umsp_9Fk2mQ1x`.
pub fn key_prefix(plain: &str) -> String {
    plain.chars().take(KEY_PREFIX.len() + 8).collect()
}

/// Verify a presented key against every configured client in constant time.
///
/// All clients are compared (no early return) so timing does not reveal which
/// entry matched, and the comparison itself uses `subtle::ConstantTimeEq`.
pub fn authenticate<'a>(clients: &'a [Client], presented: &str) -> Option<&'a Client> {
    let digest = Sha256::digest(presented.as_bytes());
    let mut matched: Option<&'a Client> = None;
    for client in clients {
        let Some(hex) = client.key_hash.strip_prefix("sha256:") else {
            continue;
        };
        let Some(bytes) = hex_decode(hex) else {
            continue;
        };
        if bytes.len() == digest.len() && bool::from(bytes.ct_eq(digest.as_slice())) {
            matched = Some(client);
        }
    }
    matched
}

/// Pull a key from `Authorization: Bearer ...` or `x-api-key: ...`.
pub fn extract_presented_key(headers: &axum::http::HeaderMap) -> Option<String> {
    if let Some(value) = headers.get(axum::http::header::AUTHORIZATION) {
        if let Ok(text) = value.to_str() {
            let text = text.trim();
            if let Some(rest) = text
                .strip_prefix("Bearer ")
                .or_else(|| text.strip_prefix("bearer "))
            {
                let rest = rest.trim();
                if !rest.is_empty() {
                    return Some(rest.to_string());
                }
            }
        }
    }
    if let Some(value) = headers.get("x-api-key") {
        if let Ok(text) = value.to_str() {
            let text = text.trim();
            if !text.is_empty() {
                return Some(text.to_string());
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{Client, PROTOCOLS};

    fn client(hash: String) -> Client {
        Client {
            name: "c".into(),
            key_hash: hash,
            key_prefix: "umsp_0000".into(),
            allowed_models: vec!["*".into()],
            allowed_protocols: PROTOCOLS.iter().map(|s| s.to_string()).collect(),
        }
    }

    #[test]
    fn generated_keys_are_unique_and_prefixed() {
        let a = generate_key();
        let b = generate_key();
        assert!(a.starts_with("umsp_"));
        assert_ne!(a, b);
        assert!(a.len() > 40);
    }

    #[test]
    fn hash_is_stable_and_verifies() {
        let key = generate_key();
        let hash = hash_key(&key);
        assert!(hash.starts_with("sha256:"));
        assert_eq!(hash.len(), 7 + 64);
        let clients = vec![client(hash.clone())];
        assert_eq!(
            authenticate(&clients, &key).map(|c| c.name.as_str()),
            Some("c")
        );
        assert!(authenticate(&clients, "umsp_wrong").is_none());
    }

    #[test]
    fn malformed_hashes_are_skipped() {
        let clients = vec![
            client("sha256:nothex".into()),
            client("md5:abc".into()),
            client(hash_key("umsp_real")),
        ];
        assert_eq!(
            authenticate(&clients, "umsp_real").map(|c| c.name.as_str()),
            Some("c")
        );
    }

    #[test]
    fn header_extraction() {
        let mut headers = axum::http::HeaderMap::new();
        headers.insert("x-api-key", "umsp_abc".parse().unwrap());
        assert_eq!(extract_presented_key(&headers).as_deref(), Some("umsp_abc"));

        let mut headers = axum::http::HeaderMap::new();
        headers.insert(
            axum::http::header::AUTHORIZATION,
            "Bearer umsp_xyz".parse().unwrap(),
        );
        assert_eq!(extract_presented_key(&headers).as_deref(), Some("umsp_xyz"));

        let headers = axum::http::HeaderMap::new();
        assert!(extract_presented_key(&headers).is_none());
    }
}
