//! Free provider health checks (spec 6.1): DeepSeek's balance endpoint.
//!
//! `GET https://api.deepseek.com/user/balance` costs no tokens and tells us
//! whether a `402` is worth probing with real inference. The root URL has no
//! `/v1` prefix even when `base_url` carries one.

use std::time::Duration;

use serde_json::{Value, json};

use crate::config::Provider;

/// How long a balance call may take before the health loop gives up on it.
pub const BALANCE_TIMEOUT: Duration = Duration::from_secs(10);

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Balance {
    pub is_available: bool,
    pub total: Option<String>,
    pub currency: Option<String>,
}

impl Balance {
    pub fn to_json(&self) -> Value {
        json!({
            "is_available": self.is_available,
            "total": self.total,
            "currency": self.currency,
        })
    }
}

/// `GET {root}/user/balance`; `base_url` may end in `/v1`.
pub fn balance_url(base_url: &str) -> String {
    let root = base_url.trim().trim_end_matches('/');
    let root = root.strip_suffix("/v1").unwrap_or(root);
    format!("{root}/user/balance")
}

pub fn parse_balance(value: &Value) -> Option<Balance> {
    let is_available = value.get("is_available")?.as_bool()?;
    let info = value
        .get("balance_infos")
        .and_then(Value::as_array)
        .and_then(|infos| infos.first());
    Some(Balance {
        is_available,
        total: info
            .and_then(|i| i.get("total_balance"))
            .and_then(Value::as_str)
            .map(str::to_string),
        currency: info
            .and_then(|i| i.get("currency"))
            .and_then(Value::as_str)
            .map(str::to_string),
    })
}

pub async fn fetch_balance(
    http: &reqwest::Client,
    provider: &Provider,
    key: &str,
) -> Result<Balance, String> {
    let url = balance_url(&provider.base_url);
    let resp = http
        .get(&url)
        .timeout(BALANCE_TIMEOUT)
        .header(reqwest::header::AUTHORIZATION, format!("Bearer {key}"))
        .send()
        .await
        .map_err(|e| format!("balance request to {url} failed: {e}"))?;
    let status = resp.status();
    let bytes = resp
        .bytes()
        .await
        .map_err(|e| format!("cannot read balance response: {e}"))?;
    if !status.is_success() {
        return Err(format!("balance http_{}", status.as_u16()));
    }
    serde_json::from_slice::<Value>(&bytes)
        .ok()
        .and_then(|value| parse_balance(&value))
        .ok_or_else(|| "balance response has no is_available field".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn balance_url_uses_the_api_root() {
        assert_eq!(
            balance_url("https://api.deepseek.com"),
            "https://api.deepseek.com/user/balance"
        );
        assert_eq!(
            balance_url("https://api.deepseek.com/v1/"),
            "https://api.deepseek.com/user/balance"
        );
        assert_eq!(
            balance_url("http://127.0.0.1:9"),
            "http://127.0.0.1:9/user/balance"
        );
    }

    #[test]
    fn parses_the_documented_shape() {
        let value = json!({
            "is_available": true,
            "balance_infos": [{
                "currency": "CNY",
                "total_balance": "5.78",
                "granted_balance": "0.00",
                "topped_up_balance": "5.78"
            }]
        });
        let balance = parse_balance(&value).unwrap();
        assert!(balance.is_available);
        assert_eq!(balance.total.as_deref(), Some("5.78"));
        assert_eq!(balance.currency.as_deref(), Some("CNY"));
    }

    #[test]
    fn rejects_responses_without_is_available() {
        assert!(parse_balance(&json!({"error": "nope"})).is_none());
        assert!(parse_balance(&json!({"is_available": false})).is_some());
    }
}
