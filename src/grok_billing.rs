//! Grok subscription quota from the CLI proxy billing endpoint.
//!
//! Grok's `x-ratelimit-*` response headers are short-lived throttles, not the
//! weekly subscription pool. The weekly pool is exposed by
//! `GET {base_url}/billing?format=credits`, which needs the numeric `x-userid`
//! from `GET {base_url}/user` in addition to the CLI session headers.
//!
//! The gateway stores only the remaining fraction; request and token counts
//! from different vendors are not comparable units.

use std::time::{Duration, Instant};

use chrono::{DateTime, Utc};
use serde_json::Value;

use crate::capacity::UsageWindow;
use crate::config::Provider;
use crate::sessions::{CLI_TOKEN_AUTH, SessionStore};

/// How long one billing call may take before the health loop gives up on it.
pub const GROK_USAGE_TIMEOUT: Duration = Duration::from_secs(10);
/// Fallback period length when the billing response omits both timestamps.
const DEFAULT_PERIOD: Duration = Duration::from_secs(7 * 24 * 60 * 60);

#[derive(Debug, Clone, PartialEq)]
pub struct BillingUsage {
    pub remaining_fraction: f64,
    pub used_percent: f64,
    pub reset_at: Option<DateTime<Utc>>,
    pub period_duration: Option<Duration>,
    pub period_type: Option<String>,
}

impl BillingUsage {
    /// Convert to the route table's clock-relative window. A period that has
    /// already ended yields `None`, so a stale response cannot keep a provider
    /// in the usage class until the next poll.
    pub fn into_window(self) -> Option<UsageWindow> {
        let now_wall = Utc::now();
        let now = Instant::now();
        let reset_at = match self.reset_at {
            Some(reset) => {
                let delta = reset.signed_duration_since(now_wall).to_std().ok()?;
                Some(now + delta)
            }
            None => None,
        };
        Some(UsageWindow {
            duration: self.period_duration.unwrap_or(DEFAULT_PERIOD),
            remaining_fraction: self.remaining_fraction.clamp(0.0, 1.0),
            reset_at,
            observed_at: now,
        })
    }
}

/// `base_url` is the provider's API base (`…/v1`); billing lives beside it.
pub fn billing_url(base_url: &str) -> String {
    format!("{}/billing?format=credits", base_url.trim_end_matches('/'))
}

pub fn user_url(base_url: &str) -> String {
    format!("{}/user", base_url.trim_end_matches('/'))
}

pub fn parse_user_id(value: &Value) -> Option<String> {
    match value.get("xUserId")? {
        Value::String(id) if !id.is_empty() => Some(id.clone()),
        Value::Number(id) => Some(id.to_string()),
        _ => None,
    }
}

fn number(value: Option<&Value>) -> Option<f64> {
    value?.as_f64().filter(|value| value.is_finite())
}

fn timestamp(value: Option<&Value>) -> Option<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(value?.as_str()?)
        .ok()
        .map(|value| value.with_timezone(&Utc))
}

fn product_usage_percent(config: &Value) -> Option<f64> {
    let products = config.get("productUsage")?.as_array()?;
    products
        .iter()
        .find(|entry| entry.get("product").and_then(Value::as_str) == Some("GrokBuild"))
        .or_else(|| products.first())
        .and_then(|entry| number(entry.get("usagePercent")))
}

/// Parse `GET /billing?format=credits`. The percentage is "used", so the
/// remaining fraction is `1 - used / 100`.
pub fn parse_billing(value: &Value) -> Option<BillingUsage> {
    let config = value.get("config")?;
    let used_percent = number(config.get("creditUsagePercent"))
        .or_else(|| product_usage_percent(config))?
        .clamp(0.0, 100.0);
    let period = config.get("currentPeriod");
    let start = timestamp(period.and_then(|period| period.get("start")));
    let end = timestamp(period.and_then(|period| period.get("end")));
    let period_duration = start
        .zip(end)
        .and_then(|(start, end)| end.signed_duration_since(start).to_std().ok());
    Some(BillingUsage {
        remaining_fraction: (1.0 - used_percent / 100.0).clamp(0.0, 1.0),
        used_percent,
        reset_at: end.or_else(|| timestamp(config.get("billingPeriodEnd"))),
        period_duration,
        period_type: period
            .and_then(|period| period.get("type"))
            .and_then(Value::as_str)
            .map(str::to_string),
    })
}

async fn get_json(
    http: &reqwest::Client,
    url: &str,
    token: &str,
    version: &str,
    user_id: Option<&str>,
) -> Result<Value, String> {
    let mut request = http
        .get(url)
        .timeout(GROK_USAGE_TIMEOUT)
        .header(reqwest::header::AUTHORIZATION, format!("Bearer {token}"))
        .header("X-XAI-Token-Auth", CLI_TOKEN_AUTH)
        .header("x-grok-client-version", version);
    if let Some(user_id) = user_id {
        request = request.header("x-userid", user_id);
    }
    let response = request
        .send()
        .await
        .map_err(|error| format!("request to {url} failed: {error}"))?;
    let status = response.status();
    let bytes = response
        .bytes()
        .await
        .map_err(|error| format!("cannot read {url}: {error}"))?;
    if !status.is_success() {
        return Err(format!("{url} returned http_{}", status.as_u16()));
    }
    serde_json::from_slice(&bytes).map_err(|error| format!("{url} returned invalid JSON: {error}"))
}

/// Fetch the weekly billing snapshot and the numeric user id. The caller caches
/// the id, so steady-state polling costs one request per interval.
pub async fn fetch_usage(
    http: &reqwest::Client,
    sessions: &SessionStore,
    provider: &Provider,
    version: &str,
    user_id: Option<&str>,
) -> Result<(BillingUsage, String), String> {
    let token = sessions
        .access_token(&provider.id)
        .await
        .map_err(|error| error.to_string())?;
    let user_id = match user_id {
        Some(id) => id.to_string(),
        None => {
            let value =
                get_json(http, &user_url(&provider.base_url), &token, version, None).await?;
            parse_user_id(&value)
                .ok_or_else(|| format!("{} has no xUserId", user_url(&provider.base_url)))?
        }
    };
    let value = get_json(
        http,
        &billing_url(&provider.base_url),
        &token,
        version,
        Some(&user_id),
    )
    .await?;
    parse_billing(&value)
        .map(|usage| (usage, user_id))
        .ok_or_else(|| "billing response has no creditUsagePercent".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn urls_keep_the_api_prefix() {
        assert_eq!(
            billing_url("https://cli-chat-proxy.grok.com/v1"),
            "https://cli-chat-proxy.grok.com/v1/billing?format=credits"
        );
        assert_eq!(
            user_url("https://cli-chat-proxy.grok.com/v1/"),
            "https://cli-chat-proxy.grok.com/v1/user"
        );
    }

    #[test]
    fn parses_a_weekly_snapshot() {
        let value = json!({
            "config": {
                "currentPeriod": {
                    "type": "USAGE_PERIOD_TYPE_WEEKLY",
                    "start": "2026-11-19T08:33:29.430+00:00",
                    "end": "2026-11-26T08:33:29.430+00:00"
                },
                "creditUsagePercent": 44.0,
                "productUsage": [{"product": "GrokBuild", "usagePercent": 44.0}],
                "isUnifiedBillingUser": true,
                "billingPeriodEnd": "2026-11-26T08:33:29.430+00:00"
            }
        });
        let usage = parse_billing(&value).unwrap();
        assert!((usage.remaining_fraction - 0.56).abs() < 1e-9);
        assert_eq!(usage.used_percent, 44.0);
        assert_eq!(
            usage.period_type.as_deref(),
            Some("USAGE_PERIOD_TYPE_WEEKLY")
        );
        assert_eq!(usage.period_duration, Some(Duration::from_secs(7 * 86400)));
        assert!(usage.reset_at.is_some());
    }

    #[test]
    fn falls_back_to_product_usage_and_clamps() {
        let value = json!({
            "config": {
                "productUsage": [{"product": "GrokBuild", "usagePercent": 100.0}]
            }
        });
        let usage = parse_billing(&value).unwrap();
        assert_eq!(usage.remaining_fraction, 0.0);
        assert_eq!(usage.period_duration, None);

        let over = json!({"config": {"creditUsagePercent": 140.0}});
        assert_eq!(parse_billing(&over).unwrap().remaining_fraction, 0.0);

        let under = json!({"config": {"creditUsagePercent": -10.0}});
        assert_eq!(parse_billing(&under).unwrap().remaining_fraction, 1.0);
    }

    #[test]
    fn an_ended_period_yields_no_window() {
        let ended = json!({
            "config": {
                "currentPeriod": {
                    "type": "USAGE_PERIOD_TYPE_WEEKLY",
                    "start": "2020-01-01T00:00:00Z",
                    "end": "2020-01-08T00:00:00Z"
                },
                "creditUsagePercent": 10.0
            }
        });
        assert!(parse_billing(&ended).unwrap().into_window().is_none());

        let current = json!({
            "config": {
                "currentPeriod": {
                    "type": "USAGE_PERIOD_TYPE_WEEKLY",
                    "start": "2020-01-01T00:00:00Z",
                    "end": "2099-01-08T00:00:00Z"
                },
                "creditUsagePercent": 10.0
            }
        });
        assert!(parse_billing(&current).unwrap().into_window().is_some());
    }

    #[test]
    fn rejects_a_response_without_usage() {
        assert!(parse_billing(&json!({"config": {}})).is_none());
        assert!(parse_billing(&json!({"error": "nope"})).is_none());
    }

    #[test]
    fn parses_both_user_id_shapes() {
        assert_eq!(
            parse_user_id(&json!({"xUserId": "2011310503512195073"})).as_deref(),
            Some("2011310503512195073")
        );
        assert_eq!(
            parse_user_id(&json!({"xUserId": 2011310503512195073u64})).as_deref(),
            Some("2011310503512195073")
        );
        assert!(parse_user_id(&json!({"userId": "x"})).is_none());
    }
}
