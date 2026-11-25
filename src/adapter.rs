//! Per-vendor provider adapters.
//!
//! Centralizes vendor-specific authentication flows, request/catalog headers,
//! client-header stripping, request/response payload patches, 402 circuit
//! policies, and background usage/balance polling so `server/` and `commands/`
//! do not branch on vendor or quirks strings.

use axum::http::HeaderMap;
use reqwest::header::{HeaderName, HeaderValue};
use serde_json::Value;

use crate::chatgpt_oauth;
use crate::config::Provider;
use crate::envfile::EnvFile;
use crate::quirks;
use crate::sessions;
use crate::upstream;

/// Authentication mechanism used by a provider.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuthFlow {
    ApiKey,
    GrokOAuth,
    ChatGptOAuth,
}

/// Background quota/balance poller owned by a provider.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HealthPoller {
    None,
    GrokBilling,
    DeepSeekBalance,
}

/// Circuit cooldown policy when an upstream answers HTTP 402.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PaymentFailurePolicy {
    /// Generic 402 handling (`payment_required`, `auth_cooldown`).
    PaymentRequired,
    /// DeepSeek balance exhaustion (`insufficient_balance`, `ds_balance_cooldown`).
    InsufficientBalance,
}

/// Vendor/provider behavioral hook resolved once per provider via [`for_provider`].
pub trait ProviderAdapter: Send + Sync {
    fn auth_flow(&self) -> AuthFlow {
        AuthFlow::ApiKey
    }

    fn health_poller(&self) -> HealthPoller {
        HealthPoller::None
    }

    fn payment_failure_policy(&self) -> PaymentFailurePolicy {
        PaymentFailurePolicy::PaymentRequired
    }

    /// Extra headers added to every inference request (`POST` chat/responses/messages).
    fn inference_headers(
        &self,
        _provider: &Provider,
        _env: &EnvFile,
        _body: &Value,
        _client_headers: &HeaderMap,
    ) -> std::result::Result<Vec<(HeaderName, HeaderValue)>, String> {
        Ok(Vec::new())
    }

    /// Extra headers added to model-catalog fetches (`GET /models`).
    fn catalog_headers(&self, _env: &EnvFile) -> Vec<(HeaderName, HeaderValue)> {
        Vec::new()
    }

    /// Whether a forwarded client header must be dropped because this adapter
    /// sets it directly.
    fn strips_client_header(&self, _name: &str) -> bool {
        false
    }

    /// Per-attempt request body patches (runs after translation when protocols
    /// differ, and on the cloned body for passthrough attempts).
    fn patch_request(
        &self,
        _body: &mut Value,
        _upstream_protocol: &str,
        _client_name: &str,
    ) -> Vec<String> {
        Vec::new()
    }

    /// Normalize raw non-streaming upstream bytes before JSON parsing.
    fn normalize_response_bytes(&self, bytes: Vec<u8>) -> Vec<u8> {
        bytes
    }

    /// Non-streaming 2xx JSON response patches.
    fn patch_response(
        &self,
        _resp: &mut Value,
        _upstream_protocol: &str,
        _request_body: &Value,
    ) -> Vec<String> {
        Vec::new()
    }
}

struct DefaultAdapter;
struct GrokAdapter;
struct ChatGptAdapter;
struct OpenCodeGoAdapter;
struct OpenDesignAdapter;
struct DeepSeekAdapter;

static DEFAULT: DefaultAdapter = DefaultAdapter;
static GROK: GrokAdapter = GrokAdapter;
static CHATGPT: ChatGptAdapter = ChatGptAdapter;
static OPENCODE_GO: OpenCodeGoAdapter = OpenCodeGoAdapter;
static OPENDESIGN: OpenDesignAdapter = OpenDesignAdapter;
static DEEPSEEK: DeepSeekAdapter = DeepSeekAdapter;

/// Resolve the static adapter for `provider`.
///
/// Vendor-specific transport/auth adapters (`grok`, `chatgpt`, `opencode-go`)
/// match on `provider.vendor`; payload quirk presets (`opendesign`, `deepseek`)
/// follow `provider.quirks` so tests or custom overrides with `quirks = None`
/// keep exact control over payload rewrites.
pub fn for_provider(provider: &Provider) -> &'static dyn ProviderAdapter {
    match provider.vendor.as_deref() {
        Some("grok") => &GROK,
        Some("chatgpt") => &CHATGPT,
        Some("opencode-go") => &OPENCODE_GO,
        _ => match provider.quirks.as_deref() {
            Some("opendesign") => &OPENDESIGN,
            Some("deepseek") => &DEEPSEEK,
            _ => &DEFAULT,
        },
    }
}

/// Grok CLI version sent to `cli-chat-proxy` (overridable via `UMSP_GROK_CLI_VERSION`).
pub fn grok_cli_version(env: &EnvFile) -> String {
    env.get("UMSP_GROK_CLI_VERSION")
        .unwrap_or_else(|| sessions::GROK_CLI_VERSION.to_string())
}

impl ProviderAdapter for DefaultAdapter {}

impl ProviderAdapter for GrokAdapter {
    fn auth_flow(&self) -> AuthFlow {
        AuthFlow::GrokOAuth
    }

    fn health_poller(&self) -> HealthPoller {
        HealthPoller::GrokBilling
    }

    fn inference_headers(
        &self,
        provider: &Provider,
        env: &EnvFile,
        body: &Value,
        _client_headers: &HeaderMap,
    ) -> std::result::Result<Vec<(HeaderName, HeaderValue)>, String> {
        let mut headers = self.catalog_headers(env);
        if let Some(model) = body.get("model").and_then(Value::as_str) {
            let value = HeaderValue::from_str(model).map_err(|_| {
                format!(
                    "upstream model id for provider '{}' is not a valid header value",
                    provider.id
                )
            })?;
            headers.push((HeaderName::from_static("x-grok-model-override"), value));
        }
        Ok(headers)
    }

    fn catalog_headers(&self, env: &EnvFile) -> Vec<(HeaderName, HeaderValue)> {
        let mut headers = vec![(
            HeaderName::from_static("x-xai-token-auth"),
            HeaderValue::from_static(sessions::CLI_TOKEN_AUTH),
        )];
        let version = grok_cli_version(env);
        if let Ok(value) = HeaderValue::from_str(&version) {
            headers.push((HeaderName::from_static("x-grok-client-version"), value));
        }
        headers
    }

    fn strips_client_header(&self, name: &str) -> bool {
        sessions::is_cli_session_header(name)
    }
}

impl ProviderAdapter for ChatGptAdapter {
    fn auth_flow(&self) -> AuthFlow {
        AuthFlow::ChatGptOAuth
    }

    fn patch_request(
        &self,
        body: &mut Value,
        upstream_protocol: &str,
        _client_name: &str,
    ) -> Vec<String> {
        if upstream_protocol != "responses" {
            return Vec::new();
        }
        if let Some(obj) = body.as_object_mut() {
            obj.insert("store".to_string(), Value::Bool(false));
            obj.insert("stream".to_string(), Value::Bool(true));
            return vec!["chatgpt_plan_stream".to_string()];
        }
        Vec::new()
    }

    fn normalize_response_bytes(&self, bytes: Vec<u8>) -> Vec<u8> {
        if let Some(completed) = chatgpt_oauth::collapse_responses_stream(&bytes) {
            serde_json::to_vec(&completed).unwrap_or(bytes)
        } else {
            bytes
        }
    }
}

impl ProviderAdapter for OpenCodeGoAdapter {
    fn inference_headers(
        &self,
        provider: &Provider,
        env: &EnvFile,
        body: &Value,
        client_headers: &HeaderMap,
    ) -> std::result::Result<Vec<(HeaderName, HeaderValue)>, String> {
        let mut headers = self.catalog_headers(env);
        let session = upstream::opencode_session(client_headers, body);
        let value = HeaderValue::from_str(&session).map_err(|_| {
            format!(
                "opencode session for provider '{}' is not a valid header value",
                provider.id
            )
        })?;
        headers.push((HeaderName::from_static("x-opencode-session"), value));
        Ok(headers)
    }

    fn catalog_headers(&self, _env: &EnvFile) -> Vec<(HeaderName, HeaderValue)> {
        vec![(
            reqwest::header::USER_AGENT,
            HeaderValue::from_static(upstream::OPENCODE_USER_AGENT),
        )]
    }

    fn strips_client_header(&self, name: &str) -> bool {
        name.eq_ignore_ascii_case("user-agent") || name.eq_ignore_ascii_case("x-opencode-session")
    }
}

impl ProviderAdapter for OpenDesignAdapter {
    fn patch_request(
        &self,
        body: &mut Value,
        upstream_protocol: &str,
        _client_name: &str,
    ) -> Vec<String> {
        if upstream_protocol == "chat" {
            quirks::apply_request_quirks(body)
        } else {
            Vec::new()
        }
    }

    fn patch_response(
        &self,
        resp: &mut Value,
        upstream_protocol: &str,
        request_body: &Value,
    ) -> Vec<String> {
        if upstream_protocol == "chat" {
            quirks::apply_chat_response_quirks(resp, quirks::effective_max_tokens(request_body))
        } else {
            Vec::new()
        }
    }
}

impl ProviderAdapter for DeepSeekAdapter {
    fn health_poller(&self) -> HealthPoller {
        HealthPoller::DeepSeekBalance
    }

    fn payment_failure_policy(&self) -> PaymentFailurePolicy {
        PaymentFailurePolicy::InsufficientBalance
    }

    fn patch_request(
        &self,
        body: &mut Value,
        upstream_protocol: &str,
        client_name: &str,
    ) -> Vec<String> {
        if upstream_protocol == "chat" {
            quirks::apply_deepseek_request_quirks(body, client_name)
        } else {
            Vec::new()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn provider(vendor: Option<&str>, quirks: Option<&str>) -> Provider {
        Provider {
            id: "p".into(),
            vendor: vendor.map(str::to_string),
            base_url: "https://example.test/v1".into(),
            anthropic_base_url: None,
            api_key_env: "K".into(),
            auth: "api_key".into(),
            protocols: vec!["chat".into(), "responses".into()],
            quirks: quirks.map(str::to_string),
        }
    }

    #[test]
    fn grok_adapter_owns_oauth_billing_and_cli_headers() {
        let p = provider(Some("grok"), None);
        let adapter = for_provider(&p);
        assert_eq!(adapter.auth_flow(), AuthFlow::GrokOAuth);
        assert_eq!(adapter.health_poller(), HealthPoller::GrokBilling);
        assert!(adapter.strips_client_header("x-grok-model-override"));
        assert!(!adapter.strips_client_header("x-other"));
        let env = EnvFile::default();
        let headers = adapter
            .inference_headers(&p, &env, &json!({"model": "grok-3"}), &HeaderMap::new())
            .unwrap();
        assert!(
            headers
                .iter()
                .any(|(k, v)| k == "x-grok-model-override" && v == "grok-3")
        );
        assert!(headers.iter().any(|(k, _)| k == "x-xai-token-auth"));
    }

    #[test]
    fn chatgpt_adapter_forces_plan_stream_and_collapses_sse() {
        let p = provider(Some("chatgpt"), None);
        let adapter = for_provider(&p);
        assert_eq!(adapter.auth_flow(), AuthFlow::ChatGptOAuth);
        let mut body = json!({"model": "gpt-5", "input": "hi", "stream": false});
        let notes = adapter.patch_request(&mut body, "responses", "cli");
        assert_eq!(notes, vec!["chatgpt_plan_stream"]);
        assert_eq!(body["store"], false);
        assert_eq!(body["stream"], true);

        let sse = b"data: {\"type\":\"response.completed\",\"response\":{\"id\":\"resp_1\"}}\n\n";
        let collapsed = adapter.normalize_response_bytes(sse.to_vec());
        let parsed: Value = serde_json::from_slice(&collapsed).unwrap();
        assert_eq!(parsed["id"], "resp_1");
    }

    #[test]
    fn opencode_adapter_sets_user_agent_and_session() {
        let p = provider(Some("opencode-go"), None);
        let adapter = for_provider(&p);
        assert!(adapter.strips_client_header("User-Agent"));
        assert!(adapter.strips_client_header("x-opencode-session"));
        let headers = adapter
            .inference_headers(
                &p,
                &EnvFile::default(),
                &json!({"model": "m", "messages": [{"role": "user", "content": "hi"}]}),
                &HeaderMap::new(),
            )
            .unwrap();
        assert!(
            headers
                .iter()
                .any(|(k, _)| k == reqwest::header::USER_AGENT)
        );
        assert!(headers.iter().any(|(k, _)| k == "x-opencode-session"));
    }

    #[test]
    fn opendesign_and_deepseek_adapters_follow_quirks_tag() {
        let od = provider(Some("open-design"), Some("opendesign"));
        let mut body = json!({"max_tokens": 10, "max_completion_tokens": 20});
        assert_eq!(
            for_provider(&od).patch_request(&mut body, "chat", "c"),
            vec!["dropped_max_completion_tokens"]
        );
        // Without quirks tag, open-design uses default no-op adapter.
        let od_plain = provider(Some("open-design"), None);
        let mut body2 = json!({"max_tokens": 10, "max_completion_tokens": 20});
        assert!(
            for_provider(&od_plain)
                .patch_request(&mut body2, "chat", "c")
                .is_empty()
        );

        let ds = provider(Some("deepseek"), Some("deepseek"));
        assert_eq!(
            for_provider(&ds).health_poller(),
            HealthPoller::DeepSeekBalance
        );
        assert_eq!(
            for_provider(&ds).payment_failure_policy(),
            PaymentFailurePolicy::InsufficientBalance
        );
    }
}
