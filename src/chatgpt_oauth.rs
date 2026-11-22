//! Sign in with ChatGPT for open-source, self-hosted use of a ChatGPT plan.
//!
//! The grant is the documented PKCE loopback flow against `auth.openai.com`.
//! Inference stays on `https://api.openai.com/v1/responses` with the access
//! token. This module does not call ChatGPT's private `backend-api`.

use std::time::Duration;

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use serde_json::Value;
use sha2::{Digest, Sha256};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

use crate::error::{Result, UmspError};
use crate::sessions::SessionStore;

pub const AUTHORIZE_URL: &str = "https://auth.openai.com/api/accounts/authorize";
pub const TOKEN_URL: &str = "https://auth.openai.com/api/accounts/oauth/token";
pub const ISSUER: &str = "https://auth.openai.com";
pub const RESOURCE: &str = "https://api.openai.com/v1";
pub const SCOPES: &str =
    "openid profile email offline_access resource.invoke chatgpt.tokens.use.direct";
pub const AGENT_NAME: &str = "UMSP";
pub const CALLBACK_PATH: &str = "/auth/callback";
pub const REGISTRATION_CLIENT: &str = "dynamic_agent_client";
pub const PLAN_SCOPE: &str = "chatgpt.tokens.use.direct";

const LOGIN_WAIT: Duration = Duration::from_secs(300);

pub fn pkce_challenge(verifier: &str) -> String {
    URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()))
}

pub fn random_token() -> String {
    let mut bytes = [0u8; 32];
    getrandom::fill(&mut bytes).expect("os randomness");
    URL_SAFE_NO_PAD.encode(bytes)
}

pub fn authorize_url(
    redirect_uri: &str,
    host_id: &str,
    state: &str,
    nonce: &str,
    challenge: &str,
    client_id: Option<&str>,
) -> String {
    let client_id = client_id.unwrap_or(REGISTRATION_CLIENT);
    let mut pairs = vec![
        ("client_id", client_id),
        ("response_type", "code"),
        ("redirect_uri", redirect_uri),
        ("scope", SCOPES),
        ("resource", RESOURCE),
        ("state", state),
        ("nonce", nonce),
        ("code_challenge_method", "S256"),
        ("code_challenge", challenge),
        ("ext_agent_host_id", host_id),
    ];
    if client_id == REGISTRATION_CLIENT {
        pairs.push(("agent_name_hint", AGENT_NAME));
    }
    let query = pairs
        .into_iter()
        .map(|(key, value)| format!("{key}={}", encode(value)))
        .collect::<Vec<_>>()
        .join("&");
    format!("{AUTHORIZE_URL}?{query}")
}

pub struct Callback {
    pub code: String,
    pub state: String,
    pub client_id: Option<String>,
    pub error: Option<String>,
}

pub fn parse_callback_target(request_line: &str) -> std::result::Result<Callback, String> {
    let path = request_line
        .split_whitespace()
        .nth(1)
        .ok_or_else(|| "callback request has no path".to_string())?;
    let Some((pathname, query)) = path.split_once('?') else {
        return Err("callback is missing a query".into());
    };
    if pathname != CALLBACK_PATH {
        return Err(format!("callback path {pathname} is not {CALLBACK_PATH}"));
    }
    let mut code = None;
    let mut state = None;
    let mut client_id = None;
    let mut error = None;
    for pair in query.split('&') {
        let Some((key, value)) = pair.split_once('=') else {
            continue;
        };
        let value = decode(value);
        match key {
            "code" => code = Some(value),
            "state" => state = Some(value),
            "client_id" => client_id = Some(value),
            "error" => error = Some(value),
            _ => {}
        }
    }
    let state = state.ok_or_else(|| "callback is missing state".to_string())?;
    Ok(Callback {
        code: code.unwrap_or_default(),
        state,
        client_id,
        error,
    })
}

/// Checks the claims the Sign in with ChatGPT docs require before a plan token
/// is stored. Signature verification against the issuer JWKS is not done here:
/// the callback is accepted only on the loopback listener this process bound.
pub fn plan_claims_ok(
    id_token: &str,
    nonce: &str,
    audience: &str,
) -> std::result::Result<(), String> {
    let mut parts = id_token.split('.');
    let header = parts
        .next()
        .ok_or_else(|| "id token is not a jwt".to_string())?;
    let payload = parts
        .next()
        .ok_or_else(|| "id token is not a jwt".to_string())?;
    if parts.next().is_none() {
        return Err("id token is not a jwt".into());
    }
    let header: Value = decode_json(header)?;
    let alg = header.get("alg").and_then(Value::as_str).unwrap_or("");
    if alg.eq_ignore_ascii_case("none") || alg.is_empty() {
        return Err("id token algorithm is not allowed".into());
    }
    let claims: Value = decode_json(payload)?;
    if claims.get("iss").and_then(Value::as_str) != Some(ISSUER) {
        return Err("id token issuer is not auth.openai.com".into());
    }
    if claims.get("aud").and_then(Value::as_str) != Some(audience) {
        return Err("id token audience does not match the issued client".into());
    }
    if claims.get("nonce").and_then(Value::as_str) != Some(nonce) {
        return Err("id token nonce does not match this login".into());
    }
    let exp = claims.get("exp").and_then(Value::as_i64).unwrap_or(0);
    let now = chrono::Utc::now().timestamp();
    if exp <= now {
        return Err("id token is expired".into());
    }
    if claims
        .get("sub")
        .and_then(Value::as_str)
        .unwrap_or("")
        .is_empty()
    {
        return Err("id token has no subject".into());
    }
    Ok(())
}

pub fn scope_allows_plan(scope: &str) -> bool {
    scope.split_whitespace().any(|item| item == PLAN_SCOPE)
}

/// Collapse a Responses SSE body into the final response object. `None` when
/// the stream never completed.
pub fn collapse_responses_stream(bytes: &[u8]) -> Option<Value> {
    let text = std::str::from_utf8(bytes).ok()?;
    let mut completed = None;
    for line in text.lines() {
        let line = line.trim();
        let Some(data) = line.strip_prefix("data:") else {
            continue;
        };
        let data = data.trim();
        if data.is_empty() || data == "[DONE]" {
            continue;
        }
        let Ok(event) = serde_json::from_str::<Value>(data) else {
            continue;
        };
        if event.get("type").and_then(Value::as_str) == Some("response.completed") {
            completed = event.get("response").cloned();
        }
    }
    completed
}

pub async fn login(store: &SessionStore, provider_id: &str, open_browser: bool) -> Result<()> {
    let host_id = load_or_create_host_id(store.dir())?;
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .map_err(|err| UmspError::network(format!("chatgpt callback listen failed: {err}")))?;
    let port = listener
        .local_addr()
        .map_err(|err| UmspError::network(format!("chatgpt callback has no local address: {err}")))?
        .port();
    let redirect_uri = format!("http://127.0.0.1:{port}{CALLBACK_PATH}");
    let state = random_token();
    let nonce = random_token();
    let verifier = random_token();
    let url = authorize_url(
        &redirect_uri,
        &host_id,
        &state,
        &nonce,
        &pkce_challenge(&verifier),
        None,
    );
    eprintln!("Sign in provider '{provider_id}' with the ChatGPT subscription.");
    eprintln!("Open: {url}");
    eprintln!("Waiting on {redirect_uri}");
    eprintln!("On a machine without a browser, forward that port and open the URL there.");
    if open_browser {
        crate::sessions::launch_browser(&url);
    }
    let callback = accept_callback(&listener, &state).await?;
    if let Some(error) = callback.error {
        return Err(UmspError::upstream(
            "oauth_denied",
            format!("chatgpt login for provider '{provider_id}' was not approved: {error}"),
        ));
    }
    let issued = callback.client_id.ok_or_else(|| {
        UmspError::upstream(
            "oauth_client",
            format!("chatgpt registration for provider '{provider_id}' returned no client_id"),
        )
    })?;
    if issued == REGISTRATION_CLIENT || issued.is_empty() {
        return Err(UmspError::upstream(
            "oauth_client",
            format!("chatgpt registration for provider '{provider_id}' did not issue a client id"),
        ));
    }
    let form = token_exchange_form(&issued, &callback.code, &verifier, &redirect_uri);
    let token = store
        .post_form_public(TOKEN_URL, &form)
        .await
        .map_err(|reason| {
            UmspError::network(format!(
                "chatgpt token exchange for provider '{provider_id}' failed: {reason}"
            ))
        })?;
    let id_token = json_str(&token, "id_token").ok_or_else(|| {
        UmspError::upstream(
            "oauth_id_token",
            format!("chatgpt token response for provider '{provider_id}' has no id_token"),
        )
    })?;
    plan_claims_ok(&id_token, &nonce, &issued).map_err(|reason| {
        UmspError::upstream(
            "oauth_id_token",
            format!("chatgpt id token for provider '{provider_id}' was rejected: {reason}"),
        )
    })?;
    let scope = json_str(&token, "scope").unwrap_or_default();
    if !scope_allows_plan(&scope) {
        return Err(UmspError::upstream(
            "oauth_scope",
            format!("chatgpt login for provider '{provider_id}' did not grant {PLAN_SCOPE}"),
        ));
    }
    store.save_chatgpt(provider_id, &token, &issued)?;
    Ok(())
}

pub fn token_exchange_form(
    client_id: &str,
    code: &str,
    verifier: &str,
    redirect_uri: &str,
) -> String {
    form_encode(&[
        ("grant_type", "authorization_code"),
        ("client_id", client_id),
        ("code", code),
        ("code_verifier", verifier),
        ("redirect_uri", redirect_uri),
        ("resource", RESOURCE),
    ])
}

fn load_or_create_host_id(dir: &std::path::Path) -> Result<String> {
    let path = dir.join("chatgpt-host-id");
    if let Ok(existing) = std::fs::read_to_string(&path) {
        let existing = existing.trim();
        if existing.starts_with("urn:uuid:") {
            return Ok(existing.to_string());
        }
    }
    std::fs::create_dir_all(dir)
        .map_err(|err| UmspError::other("IO", format!("cannot create {}: {err}", dir.display())))?;
    let id = format!("urn:uuid:{}", uuid_v4());
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(true)
        .open(&path)
        .map_err(|err| UmspError::other("IO", format!("cannot write host id: {err}")))?;
    use std::io::Write;
    file.write_all(id.as_bytes())
        .map_err(|err| UmspError::other("IO", format!("cannot write host id: {err}")))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600));
    }
    Ok(id)
}

fn uuid_v4() -> String {
    let mut bytes = [0u8; 16];
    getrandom::fill(&mut bytes).expect("os randomness");
    bytes[6] = (bytes[6] & 0x0f) | 0x40;
    bytes[8] = (bytes[8] & 0x3f) | 0x80;
    format!(
        "{:02x}{:02x}{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}{:02x}{:02x}{:02x}{:02x}",
        bytes[0],
        bytes[1],
        bytes[2],
        bytes[3],
        bytes[4],
        bytes[5],
        bytes[6],
        bytes[7],
        bytes[8],
        bytes[9],
        bytes[10],
        bytes[11],
        bytes[12],
        bytes[13],
        bytes[14],
        bytes[15]
    )
}

async fn accept_callback(listener: &TcpListener, expected_state: &str) -> Result<Callback> {
    let accept = tokio::time::timeout(LOGIN_WAIT, listener.accept())
        .await
        .map_err(|_| {
            UmspError::upstream(
                "oauth_expired",
                "chatgpt login timed out before the browser returned".to_string(),
            )
        })?
        .map_err(|err| UmspError::network(format!("chatgpt callback accept failed: {err}")))?;
    let mut socket = accept.0;
    let mut buf = vec![0u8; 8192];
    let n = tokio::time::timeout(Duration::from_secs(10), socket.read(&mut buf))
        .await
        .map_err(|_| UmspError::network("chatgpt callback read timed out".to_string()))?
        .map_err(|err| UmspError::network(format!("chatgpt callback read failed: {err}")))?;
    let text = String::from_utf8_lossy(&buf[..n]);
    let request_line = text.lines().next().unwrap_or("");
    let callback = parse_callback_target(request_line)
        .map_err(|reason| UmspError::upstream("oauth_callback", reason))?;
    let page = if callback.state != expected_state {
        "Login failed: state did not match. Close this tab and try again."
    } else if callback.error.is_some() {
        "Login was not approved. You can close this tab."
    } else {
        "UMSP signed in with ChatGPT. You can close this tab."
    };
    let body = format!("<html><body><p>{page}</p></body></html>");
    let response = format!(
        "HTTP/1.1 200 OK\r\ncontent-type: text/html; charset=utf-8\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
        body.len()
    );
    let _ = socket.write_all(response.as_bytes()).await;
    if callback.state != expected_state {
        return Err(UmspError::upstream(
            "oauth_state",
            "chatgpt callback state did not match this login".to_string(),
        ));
    }
    Ok(callback)
}

fn decode_json(segment: &str) -> std::result::Result<Value, String> {
    let bytes = URL_SAFE_NO_PAD
        .decode(segment)
        .or_else(|_| base64::engine::general_purpose::URL_SAFE.decode(segment))
        .map_err(|_| "id token segment is not base64url".to_string())?;
    serde_json::from_slice(&bytes).map_err(|_| "id token segment is not json".to_string())
}

fn json_str(value: &Value, key: &str) -> Option<String> {
    value.get(key).and_then(Value::as_str).map(str::to_string)
}

fn encode(value: &str) -> String {
    let mut out = String::new();
    for byte in value.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(byte as char);
            }
            _ => out.push_str(&format!("%{byte:02X}")),
        }
    }
    out
}

fn decode(value: &str) -> String {
    let mut out = Vec::new();
    let bytes = value.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            if let Ok(byte) = u8::from_str_radix(&value[i + 1..i + 3], 16) {
                out.push(byte);
                i += 3;
                continue;
            }
        }
        if bytes[i] == b'+' {
            out.push(b' ');
        } else {
            out.push(bytes[i]);
        }
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn form_encode(pairs: &[(&str, &str)]) -> String {
    pairs
        .iter()
        .map(|(key, value)| format!("{}={}", encode(key), encode(value)))
        .collect::<Vec<_>>()
        .join("&")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pkce_challenge_is_unpadded_base64url() {
        let challenge = pkce_challenge("verifier");
        assert!(!challenge.contains('='));
        assert!(!challenge.contains('+'));
        assert!(!challenge.contains('/'));
        assert_eq!(challenge, pkce_challenge("verifier"));
    }

    #[test]
    fn authorize_url_registers_a_new_agent_on_the_loopback_callback() {
        let url = authorize_url(
            "http://127.0.0.1:1455/auth/callback",
            "urn:uuid:00000000-0000-4000-8000-000000000000",
            "state",
            "nonce",
            "challenge",
            None,
        );
        assert!(url.starts_with(AUTHORIZE_URL));
        assert!(url.contains("client_id=dynamic_agent_client"));
        assert!(url.contains("agent_name_hint=UMSP"));
        assert!(url.contains("redirect_uri=http%3A%2F%2F127.0.0.1%3A1455%2Fauth%2Fcallback"));
        assert!(url.contains("code_challenge_method=S256"));
        assert!(url.contains("chatgpt.tokens.use.direct"));
        assert!(!url.contains("localhost"));
    }

    #[test]
    fn callback_rejects_the_wrong_path_and_keeps_the_issued_client() {
        let callback = parse_callback_target(
            "GET /auth/callback?code=abc&state=state&client_id=oaiapp_1 HTTP/1.1",
        )
        .unwrap();
        assert_eq!(callback.code, "abc");
        assert_eq!(callback.client_id.as_deref(), Some("oaiapp_1"));
        assert!(parse_callback_target("GET /callback?code=abc&state=state HTTP/1.1").is_err());
    }

    #[test]
    fn plan_claims_accept_a_matching_token_and_reject_none() {
        let header = URL_SAFE_NO_PAD.encode(br#"{"alg":"RS256"}"#);
        let exp = chrono::Utc::now().timestamp() + 600;
        let payload = URL_SAFE_NO_PAD.encode(
            format!(
                r#"{{"iss":"{ISSUER}","aud":"oaiapp_1","nonce":"nonce","exp":{exp},"sub":"user"}}"#
            )
            .as_bytes(),
        );
        let token = format!("{header}.{payload}.sig");
        assert!(plan_claims_ok(&token, "nonce", "oaiapp_1").is_ok());
        assert!(plan_claims_ok(&token, "other", "oaiapp_1").is_err());
        let none = URL_SAFE_NO_PAD.encode(br#"{"alg":"none"}"#);
        assert!(plan_claims_ok(&format!("{none}.{payload}.x"), "nonce", "oaiapp_1").is_err());
    }

    #[test]
    fn a_responses_stream_collapses_to_the_completed_response() {
        let body = b"data: {\"type\":\"response.output_text.delta\",\"delta\":\"Hi\"}\n\ndata: {\"type\":\"response.completed\",\"response\":{\"id\":\"resp_1\",\"output_text\":\"Hi\"}}\n\n";
        let value = collapse_responses_stream(body).unwrap();
        assert_eq!(value["id"], "resp_1");
    }
}
