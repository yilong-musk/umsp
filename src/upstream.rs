//! Upstream HTTP plumbing: TLS client, header forwarding, URL joining.

use std::sync::Arc;

use axum::http::{HeaderMap, HeaderName, HeaderValue};
use reqwest::redirect::Policy;

use crate::config::{Config, Provider};
use crate::error::{Result, UmspError};

/// Headers that must never be forwarded upstream.
pub const SKIP_REQUEST_HEADERS: &[&str] = &[
    "authorization",
    "x-api-key",
    "host",
    "content-length",
    "connection",
    "keep-alive",
    "proxy-authenticate",
    "proxy-authorization",
    "proxy-connection",
    "te",
    "trailer",
    "transfer-encoding",
    "upgrade",
    "cookie",
    "accept",
    "accept-encoding",
    // The gateway sets its own `content-type: application/json` upstream; copying
    // the client's would emit the header twice.
    "content-type",
];

/// Response headers that must not be relayed to the client.
pub const SKIP_RESPONSE_HEADERS: &[&str] = &[
    "content-length",
    "transfer-encoding",
    "connection",
    "keep-alive",
    "te",
    "trailer",
    "upgrade",
    "proxy-authenticate",
    "proxy-authorization",
    "proxy-connection",
];

/// Short-lived client for the OAuth token endpoint (device login and refresh).
pub fn build_login_client() -> Result<reqwest::Client> {
    let tls = tls_config()?;
    finish_client(
        reqwest::Client::builder()
            .use_preconfigured_tls(tls)
            .connect_timeout(std::time::Duration::from_secs(10))
            .timeout(std::time::Duration::from_secs(60))
            .redirect(Policy::none()),
    )
}

pub fn build_http_client(cfg: &Config) -> Result<reqwest::Client> {
    let tls = tls_config()?;
    finish_client(
        reqwest::Client::builder()
            .use_preconfigured_tls(tls)
            .connect_timeout(std::time::Duration::from_secs(10))
            .timeout(std::time::Duration::from_secs(cfg.server.request_timeout_s))
            .redirect(Policy::none())
            .pool_idle_timeout(std::time::Duration::from_secs(90)),
    )
}

/// Client for SSE requests. Deliberately has **no total timeout** — the
/// non-streaming client caps a request at `request_timeout_s` (600s), which
/// would guillotine a long stream. Per-phase limits live in the handler:
/// 10s connect (here), response headers and idle gaps are wrapped with
/// `tokio::time::timeout` around `send()` / the body stream only.
pub fn build_stream_client() -> Result<reqwest::Client> {
    let tls = tls_config()?;
    finish_client(
        reqwest::Client::builder()
            .use_preconfigured_tls(tls)
            .connect_timeout(std::time::Duration::from_secs(10))
            .redirect(Policy::none())
            .pool_idle_timeout(std::time::Duration::from_secs(90)),
    )
}

fn finish_client(builder: reqwest::ClientBuilder) -> Result<reqwest::Client> {
    with_env_proxy(builder)
        .build()
        .map_err(|e| UmspError::other("HTTP_CLIENT", format!("cannot build HTTP client: {e}")))
}

/// Honour `HTTP(S)_PROXY`, but never send loopback upstreams through it.
/// Tests and `base_url = "http://127.0.0.1:..."` depend on that.
fn with_env_proxy(mut builder: reqwest::ClientBuilder) -> reqwest::ClientBuilder {
    let https = env_proxy(&["HTTPS_PROXY", "https_proxy"]);
    let http = env_proxy(&["HTTP_PROXY", "http_proxy"]);
    let all = env_proxy(&["ALL_PROXY", "all_proxy"]);
    let mut installed = false;
    if let Some(url) = https {
        if let Ok(proxy) = reqwest::Proxy::https(url) {
            builder = builder.proxy(proxy.no_proxy(loopback_no_proxy()));
            installed = true;
        }
    }
    if let Some(url) = http {
        if let Ok(proxy) = reqwest::Proxy::http(url) {
            builder = builder.proxy(proxy.no_proxy(loopback_no_proxy()));
            installed = true;
        }
    }
    if !installed {
        if let Some(url) = all {
            if let Ok(proxy) = reqwest::Proxy::all(url) {
                builder = builder.proxy(proxy.no_proxy(loopback_no_proxy()));
            }
        }
    }
    builder
}

fn env_proxy(names: &[&str]) -> Option<String> {
    names
        .iter()
        .find_map(|name| std::env::var(name).ok())
        .filter(|value| !value.trim().is_empty())
}

fn loopback_no_proxy() -> Option<reqwest::NoProxy> {
    let mut list = std::env::var("NO_PROXY")
        .or_else(|_| std::env::var("no_proxy"))
        .unwrap_or_default();
    if !list.is_empty() && !list.trim_end().ends_with(',') {
        list.push(',');
    }
    list.push_str("localhost,127.0.0.1,::1");
    reqwest::NoProxy::from_string(&list)
}

fn tls_config() -> Result<rustls::ClientConfig> {
    let mut roots = rustls::RootCertStore::empty();
    roots.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    Ok(rustls::ClientConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .map_err(|e| UmspError::other("TLS", format!("cannot configure TLS versions: {e}")))?
        .with_root_certificates(roots)
        .with_no_client_auth())
}

/// Copy client headers upstream, minus the deny list. Returned as a list so
/// callers can add them to a request builder (HeaderMap iteration yields
/// `Option<HeaderName>` for multi-valued headers).
pub fn forward_request_headers(
    src: &HeaderMap,
) -> Vec<(reqwest::header::HeaderName, reqwest::header::HeaderValue)> {
    let mut out = Vec::new();
    for (name, value) in src.iter() {
        let lower = name.as_str().to_ascii_lowercase();
        if SKIP_REQUEST_HEADERS.contains(&lower.as_str()) {
            continue;
        }
        let Ok(name) = reqwest::header::HeaderName::from_bytes(name.as_str().as_bytes()) else {
            continue;
        };
        let Ok(value) = reqwest::header::HeaderValue::from_bytes(value.as_bytes()) else {
            continue;
        };
        out.push((name, value));
    }
    out.push((
        reqwest::header::ACCEPT_ENCODING,
        reqwest::header::HeaderValue::from_static("identity"),
    ));
    out
}

/// Relay upstream response headers, minus hop-by-hop headers.
pub fn relay_response_headers(src: &HeaderMap) -> Vec<(HeaderName, HeaderValue)> {
    let mut out = Vec::new();
    for (name, value) in src.iter() {
        let lower = name.as_str().to_ascii_lowercase();
        if SKIP_RESPONSE_HEADERS.contains(&lower.as_str()) {
            continue;
        }
        out.push((name.clone(), value.clone()));
    }
    out
}

pub fn join_url(base: &str, path: &str) -> String {
    format!(
        "{}/{}",
        base.trim_end_matches('/'),
        path.trim_start_matches('/')
    )
}

/// Append the client's raw query string to an upstream URL (Claude Code sends
/// `/v1/messages?beta=true`; spec 2.0 says keep query parameters as-is).
pub fn append_query(url: String, query: Option<&str>) -> String {
    match query {
        Some(query) if !query.is_empty() => format!("{url}?{query}"),
        _ => url,
    }
}

/// Rewrite `retry-after` HTTP-date values to integer seconds (spec 2.0:
/// Anthropic clients require a number of seconds).
pub fn normalize_retry_after(headers: &mut [(HeaderName, HeaderValue)]) {
    for (name, value) in headers.iter_mut() {
        if !name.as_str().eq_ignore_ascii_case("retry-after") {
            continue;
        }
        let Ok(text) = value.to_str() else {
            continue;
        };
        if text.trim().parse::<u64>().is_ok() {
            continue;
        }
        if let Some(seconds) = http_date_seconds_from_now(text) {
            if let Ok(new_value) = HeaderValue::from_str(&seconds.to_string()) {
                *value = new_value;
            }
        }
    }
}

/// Seconds from now until an IMF-fixdate / RFC 2822 timestamp. Negative
/// (already past) values clamp to 0.
fn http_date_seconds_from_now(text: &str) -> Option<i64> {
    let text = text.trim();
    if let Ok(dt) = chrono::DateTime::parse_from_rfc2822(text) {
        return Some(
            (dt.with_timezone(&chrono::Utc) - chrono::Utc::now())
                .num_seconds()
                .max(0),
        );
    }
    let stripped = text.strip_suffix(" GMT").unwrap_or(text).trim();
    let naive = chrono::NaiveDateTime::parse_from_str(stripped, "%a, %d %b %Y %H:%M:%S").ok()?;
    Some((naive.and_utc() - chrono::Utc::now()).num_seconds().max(0))
}

/// Upstream URL for a protocol entry point.
pub fn protocol_url(provider: &Provider, protocol: &str) -> Option<String> {
    match protocol {
        "chat" => Some(join_url(&provider.base_url, "chat/completions")),
        "responses" => Some(join_url(&provider.base_url, "responses")),
        "messages" => {
            if let Some(base) = &provider.anthropic_base_url {
                Some(join_url(base, "v1/messages"))
            } else if provider.base_url.ends_with("/v1") {
                Some(join_url(&provider.base_url, "messages"))
            } else {
                Some(join_url(&provider.base_url, "v1/messages"))
            }
        }
        _ => None,
    }
}

/// First error-ish status we consider retryable (used by later milestones).
pub fn status_is_retryable(status: u16) -> bool {
    status == 429 || status == 402 || (500..=599).contains(&status)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn joins_without_double_slashes() {
        assert_eq!(
            join_url("https://x.test/v1/", "/chat/completions"),
            "https://x.test/v1/chat/completions"
        );
        assert_eq!(
            join_url("https://x.test/v1", "chat/completions"),
            "https://x.test/v1/chat/completions"
        );
    }

    #[test]
    fn protocol_urls() {
        let provider = Provider {
            id: "od".into(),
            vendor: Some("open-design".into()),
            base_url: "https://od.test/v1".into(),
            anthropic_base_url: None,
            api_key_env: "K".into(),
            auth: "api_key".into(),
            protocols: vec!["chat".into(), "messages".into()],
            quirks: None,
        };
        assert_eq!(
            protocol_url(&provider, "chat").unwrap(),
            "https://od.test/v1/chat/completions"
        );
        assert_eq!(
            protocol_url(&provider, "messages").unwrap(),
            "https://od.test/v1/messages"
        );
        assert!(protocol_url(&provider, "nope").is_none());
    }

    #[test]
    fn strips_sensitive_headers_but_keeps_anthropic_beta() {
        let mut headers = HeaderMap::new();
        headers.insert("authorization", "Bearer secret".parse().unwrap());
        headers.insert("x-api-key", "secret".parse().unwrap());
        headers.insert(
            "anthropic-beta",
            "prompt-caching-2024-07-31".parse().unwrap(),
        );
        headers.insert("x-stainless-lang", "python".parse().unwrap());
        headers.insert("cookie", "a=b".parse().unwrap());
        headers.insert("content-type", "application/json".parse().unwrap());
        headers.insert("accept", "application/json".parse().unwrap());

        let out = forward_request_headers(&headers);
        let find = |name: &str| {
            out.iter()
                .find(|(n, _)| n.as_str() == name)
                .map(|(_, v)| v.to_str().unwrap().to_string())
        };
        assert!(find("authorization").is_none());
        assert!(find("x-api-key").is_none());
        assert!(find("cookie").is_none());
        // The caller sets these explicitly; forwarding them again duplicates the header.
        assert!(find("content-type").is_none());
        assert!(find("accept").is_none());
        assert_eq!(find("anthropic-beta").unwrap(), "prompt-caching-2024-07-31");
        assert_eq!(find("x-stainless-lang").unwrap(), "python");
        assert_eq!(find("accept-encoding").unwrap(), "identity");
        assert_eq!(
            out.iter()
                .filter(|(n, _)| n.as_str() == "content-type")
                .count(),
            0,
            "content-type must appear zero times, not once"
        );
        assert_eq!(
            out.iter().filter(|(n, _)| n.as_str() == "accept").count(),
            0,
            "accept must appear zero times, not once"
        );
    }

    #[test]
    fn append_query_keeps_beta_and_ignores_empty() {
        assert_eq!(
            append_query("http://x.test/v1/messages".into(), Some("beta=true")),
            "http://x.test/v1/messages?beta=true"
        );
        assert_eq!(
            append_query("http://x.test/v1/messages".into(), None),
            "http://x.test/v1/messages"
        );
        assert_eq!(
            append_query("http://x.test/v1/messages".into(), Some("")),
            "http://x.test/v1/messages"
        );
    }

    #[test]
    fn retry_after_http_date_becomes_integer_seconds() {
        let mut headers = vec![
            (
                HeaderName::from_static("retry-after"),
                HeaderValue::from_static("Wed, 21 Oct 2099 07:28:00 GMT"),
            ),
            (
                HeaderName::from_static("x-should-retry"),
                HeaderValue::from_static("true"),
            ),
        ];
        normalize_retry_after(&mut headers);
        let value = headers[0].1.to_str().unwrap();
        let seconds: u64 = value
            .parse()
            .unwrap_or_else(|_| panic!("retry-after must be integer seconds, got {value}"));
        assert!(seconds > 0);
        assert_eq!(headers[0].0.as_str(), "retry-after");
        assert_eq!(headers[1].1, "true");
    }

    #[test]
    fn retry_after_integer_and_unknown_formats_are_left_alone() {
        let mut headers = vec![
            (
                HeaderName::from_static("retry-after"),
                HeaderValue::from_static("12"),
            ),
            (
                HeaderName::from_static("retry-after"),
                HeaderValue::from_static("not-a-date"),
            ),
        ];
        normalize_retry_after(&mut headers);
        assert_eq!(headers[0].1, "12");
        assert_eq!(headers[1].1, "not-a-date");
    }
}
