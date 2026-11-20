//! Per-provider Grok subscription sessions.
//!
//! Each oauth provider owns one file under `{config_dir}/sessions/<id>.json`
//! (mode 0600). Two `grok` providers never share a refresh token, so a refresh
//! on one account cannot sign the other out. The grant is the public Grok CLI
//! client against `auth.x.ai` (device code, then refresh). Inference uses the
//! access token plus the CLI proxy headers.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use chrono::{DateTime, SecondsFormat, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::error::{Result, UmspError};

/// Public client id shipped with the Grok CLI. xAI rejects unregistered clients.
pub const GROK_CLIENT_ID: &str = "b1a00492-073a-47ea-816f-4c329264a828";
pub const GROK_ISSUER: &str = "https://auth.x.ai";
/// Scopes on a working Grok CLI session that can call the CLI chat proxy.
pub const GROK_SCOPES: &str = "openid profile email offline_access grok-cli:access api:access";
/// Tells `cli-chat-proxy` to validate the bearer as a CLI session token.
pub const CLI_TOKEN_AUTH: &str = "xai-grok-cli";

const DEVICE_PATH: &str = "/oauth2/device/code";
const TOKEN_PATH: &str = "/oauth2/token";
/// Refresh this long before the access token's expiry, matching the CLI.
const REFRESH_SKEW: Duration = Duration::from_secs(300);
const DEFAULT_EXPIRES_IN: u64 = 6 * 60 * 60;

#[derive(Debug)]
pub enum SessionError {
    Missing { provider: String },
    Refresh { provider: String, reason: String },
}

impl std::fmt::Display for SessionError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SessionError::Missing { provider } => {
                write!(
                    f,
                    "provider '{provider}' has no oauth session; run `umsp login {provider}`"
                )
            }
            SessionError::Refresh { provider, reason } => {
                write!(
                    f,
                    "oauth refresh failed for provider '{provider}': {reason}"
                )
            }
        }
    }
}

#[derive(Debug, Clone)]
struct Endpoints {
    device_url: String,
    token_url: String,
    client_id: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
struct Session {
    access_token: String,
    refresh_token: String,
    /// RFC3339 timestamp. A missing or unparseable value is treated as expired.
    expires_at: String,
    #[serde(default)]
    token_type: String,
    #[serde(default)]
    scope: String,
}

impl Session {
    fn expires_soon(&self) -> bool {
        match DateTime::parse_from_rfc3339(&self.expires_at) {
            Ok(when) => {
                when <= Utc::now() + chrono::Duration::seconds(REFRESH_SKEW.as_secs() as i64)
            }
            Err(_) => true,
        }
    }
}

#[derive(Clone)]
pub struct SessionStore {
    inner: Arc<StoreInner>,
}

struct StoreInner {
    dir: PathBuf,
    http: reqwest::Client,
    endpoints: Endpoints,
    locks: Mutex<HashMap<String, Arc<tokio::sync::Mutex<()>>>>,
}

impl SessionStore {
    pub fn new(dir: PathBuf, http: reqwest::Client) -> Self {
        Self::with_endpoints(
            dir,
            http,
            &format!("{GROK_ISSUER}{DEVICE_PATH}"),
            &format!("{GROK_ISSUER}{TOKEN_PATH}"),
            GROK_CLIENT_ID,
        )
    }

    pub fn with_endpoints(
        dir: PathBuf,
        http: reqwest::Client,
        device_url: &str,
        token_url: &str,
        client_id: &str,
    ) -> Self {
        Self {
            inner: Arc::new(StoreInner {
                dir,
                http,
                endpoints: Endpoints {
                    device_url: device_url.to_string(),
                    token_url: token_url.to_string(),
                    client_id: client_id.to_string(),
                },
                locks: Mutex::new(HashMap::new()),
            }),
        }
    }

    pub fn dir(&self) -> &Path {
        &self.inner.dir
    }

    /// Device-code login. Progress (URL and user code) goes to stderr so
    /// `--json` keeps stdout to a single result line. The browser is opened
    /// only when `open_browser` is set.
    pub async fn login(&self, provider_id: &str, open_browser: bool) -> Result<()> {
        let path = session_path(&self.inner.dir, provider_id)?;
        let endpoints = &self.inner.endpoints;
        let form = form_encode(&[
            ("client_id", endpoints.client_id.as_str()),
            ("scope", GROK_SCOPES),
        ]);
        let started = self
            .post_form(&endpoints.device_url, &form)
            .await
            .map_err(|reason| {
                UmspError::network(format!(
                    "device authorization for provider '{provider_id}' failed: {reason}"
                ))
            })?;
        let device_code = json_str(&started, "device_code").ok_or_else(|| {
            UmspError::upstream(
                "oauth_device_code",
                format!("provider '{provider_id}' device authorization returned no device_code"),
            )
        })?;
        let user_code = json_str(&started, "user_code").unwrap_or_default();
        let verification_uri = json_str(&started, "verification_uri").unwrap_or_default();
        let complete = json_str(&started, "verification_uri_complete");
        let open_url = complete
            .clone()
            .filter(|url| !url.is_empty())
            .unwrap_or_else(|| verification_uri.clone());
        let interval = json_u64(&started, "interval").unwrap_or(5).clamp(1, 30);
        let expires_in = json_u64(&started, "expires_in").unwrap_or(600);
        eprintln!("Sign in provider '{provider_id}' with the Grok subscription.");
        if !open_url.is_empty() {
            eprintln!("Open: {open_url}");
        } else if !verification_uri.is_empty() {
            eprintln!("Open: {verification_uri}");
        }
        if !user_code.is_empty() {
            eprintln!("Code: {user_code}");
        }
        if open_browser && !open_url.is_empty() {
            try_open_browser(&open_url);
        }
        eprintln!("Waiting for approval...");

        let deadline = std::time::Instant::now() + Duration::from_secs(expires_in);
        let mut wait = interval;
        loop {
            if std::time::Instant::now() >= deadline {
                return Err(UmspError::upstream(
                    "oauth_expired",
                    format!(
                        "device code for provider '{provider_id}' expired; run `umsp login {provider_id}` again"
                    ),
                ));
            }
            tokio::time::sleep(Duration::from_secs(wait)).await;
            let form = form_encode(&[
                ("grant_type", "urn:ietf:params:oauth:grant-type:device_code"),
                ("device_code", device_code.as_str()),
                ("client_id", endpoints.client_id.as_str()),
            ]);
            let token = match self.post_form(&endpoints.token_url, &form).await {
                Ok(value) => value,
                Err(reason) => {
                    return Err(UmspError::network(format!(
                        "device token poll for provider '{provider_id}' failed: {reason}"
                    )));
                }
            };
            if let Some(session) = session_from_token(&token, None) {
                self.write(&path, &session)?;
                return Ok(());
            }
            let error = json_str(&token, "error").unwrap_or_else(|| "unknown_error".into());
            match error.as_str() {
                "authorization_pending" => {}
                "slow_down" => wait = (wait + 5).min(30),
                "access_denied" => {
                    return Err(UmspError::upstream(
                        "oauth_denied",
                        format!("authorization denied for provider '{provider_id}'"),
                    ));
                }
                "expired_token" => {
                    return Err(UmspError::upstream(
                        "oauth_expired",
                        format!(
                            "device code for provider '{provider_id}' expired; run `umsp login {provider_id}` again"
                        ),
                    ));
                }
                other => {
                    return Err(UmspError::upstream(
                        "oauth_token",
                        format!("oauth token endpoint rejected provider '{provider_id}': {other}"),
                    ));
                }
            }
        }
    }

    /// Access token for `provider_id`, refreshing under that provider's lock
    /// when the stored token is inside the skew window.
    pub async fn access_token(
        &self,
        provider_id: &str,
    ) -> std::result::Result<String, SessionError> {
        let lock = self.lock_for(provider_id);
        let _guard = lock.lock().await;
        let path =
            session_path(&self.inner.dir, provider_id).map_err(|_| SessionError::Missing {
                provider: provider_id.to_string(),
            })?;
        let session = self.read(&path).map_err(|_| SessionError::Missing {
            provider: provider_id.to_string(),
        })?;
        if !session.expires_soon() {
            return Ok(session.access_token);
        }
        let updated = self.refresh_and_store(provider_id, &path, &session).await?;
        Ok(updated.access_token)
    }

    /// After the proxy rejected `rejected`, return a token that is not that
    /// one. A concurrent refresh that already rotated the file is reused.
    pub async fn refresh_rejected(
        &self,
        provider_id: &str,
        rejected: &str,
    ) -> std::result::Result<String, SessionError> {
        let lock = self.lock_for(provider_id);
        let _guard = lock.lock().await;
        let path =
            session_path(&self.inner.dir, provider_id).map_err(|_| SessionError::Missing {
                provider: provider_id.to_string(),
            })?;
        let session = self.read(&path).map_err(|_| SessionError::Missing {
            provider: provider_id.to_string(),
        })?;
        if session.access_token != rejected && !session.expires_soon() {
            return Ok(session.access_token);
        }
        let updated = self.refresh_and_store(provider_id, &path, &session).await?;
        Ok(updated.access_token)
    }

    async fn refresh_and_store(
        &self,
        provider_id: &str,
        path: &Path,
        session: &Session,
    ) -> std::result::Result<Session, SessionError> {
        let updated = self.exchange_refresh(provider_id, session).await?;
        if let Ok(current) = self.read(path) {
            if current.refresh_token != session.refresh_token {
                return Ok(current);
            }
        }
        self.write(path, &updated)
            .map_err(|err| SessionError::Refresh {
                provider: provider_id.to_string(),
                reason: err.message,
            })?;
        Ok(updated)
    }

    async fn exchange_refresh(
        &self,
        provider_id: &str,
        session: &Session,
    ) -> std::result::Result<Session, SessionError> {
        let form = form_encode(&[
            ("grant_type", "refresh_token"),
            ("refresh_token", session.refresh_token.as_str()),
            ("client_id", self.inner.endpoints.client_id.as_str()),
        ]);
        let value = self
            .post_form(&self.inner.endpoints.token_url, &form)
            .await
            .map_err(|reason| SessionError::Refresh {
                provider: provider_id.to_string(),
                reason,
            })?;
        if let Some(mut fresh) = session_from_token(&value, Some(session)) {
            if fresh.refresh_token.is_empty() {
                fresh.refresh_token = session.refresh_token.clone();
            }
            return Ok(fresh);
        }
        let reason = json_str(&value, "error").unwrap_or_else(|| "invalid_grant".into());
        Err(SessionError::Refresh {
            provider: provider_id.to_string(),
            reason,
        })
    }

    async fn post_form(&self, url: &str, form: &str) -> std::result::Result<Value, String> {
        let response = self
            .inner
            .http
            .post(url)
            .header(
                reqwest::header::CONTENT_TYPE,
                "application/x-www-form-urlencoded",
            )
            .header(reqwest::header::ACCEPT, "application/json")
            .body(form.to_string())
            .send()
            .await
            .map_err(|err| format!("request failed: {err}"))?;
        let status = response.status();
        let bytes = response
            .bytes()
            .await
            .map_err(|err| format!("cannot read response: {err}"))?;
        serde_json::from_slice::<Value>(&bytes).map_err(|_| {
            format!(
                "token endpoint returned http {} with a non-json body",
                status.as_u16()
            )
        })
    }

    fn lock_for(&self, provider_id: &str) -> Arc<tokio::sync::Mutex<()>> {
        let mut locks = self.inner.locks.lock().expect("session lock map poisoned");
        locks
            .entry(provider_id.to_string())
            .or_insert_with(|| Arc::new(tokio::sync::Mutex::new(())))
            .clone()
    }

    fn read(&self, path: &Path) -> std::result::Result<Session, ()> {
        let raw = std::fs::read_to_string(path).map_err(|_| ())?;
        serde_json::from_str(&raw).map_err(|_| ())
    }

    fn write(&self, path: &Path, session: &Session) -> Result<()> {
        persist(path, session)
    }
}

fn persist(path: &Path, session: &Session) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|err| {
            UmspError::other(
                "SESSION",
                format!("cannot create {}: {err}", parent.display()),
            )
        })?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(parent, std::fs::Permissions::from_mode(0o700));
        }
    }
    let body = serde_json::to_string(session)
        .map_err(|err| UmspError::other("SESSION", format!("cannot encode session: {err}")))?;
    crate::envfile::write_private(path, &body)
}

pub fn session_path(dir: &Path, provider_id: &str) -> Result<PathBuf> {
    if !safe_provider_id(provider_id) {
        return Err(UmspError::usage(format!(
            "invalid provider id '{provider_id}'"
        )));
    }
    Ok(dir.join(format!("{provider_id}.json")))
}

pub fn session_ready(dir: &Path, provider_id: &str) -> bool {
    let Ok(path) = session_path(dir, provider_id) else {
        return false;
    };
    let Ok(raw) = std::fs::read_to_string(path) else {
        return false;
    };
    serde_json::from_str::<Session>(&raw)
        .map(|session| !session.refresh_token.is_empty() && !session.access_token.is_empty())
        .unwrap_or(false)
}

pub fn remove_session(dir: &Path, provider_id: &str) -> Result<bool> {
    let path = session_path(dir, provider_id)?;
    match std::fs::remove_file(&path) {
        Ok(()) => Ok(true),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(err) => Err(UmspError::other(
            "SESSION",
            format!("cannot remove {}: {err}", path.display()),
        )),
    }
}

/// Install a session file without going through the network. Used by tests
/// and not by the CLI.
pub fn write_session(
    dir: &Path,
    provider_id: &str,
    access_token: &str,
    refresh_token: &str,
    expires_at: &str,
) -> Result<()> {
    let path = session_path(dir, provider_id)?;
    persist(
        &path,
        &Session {
            access_token: access_token.to_string(),
            refresh_token: refresh_token.to_string(),
            expires_at: expires_at.to_string(),
            token_type: "Bearer".into(),
            scope: GROK_SCOPES.into(),
        },
    )
}

fn safe_provider_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 64
        && !id.contains("..")
        && id
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '.' || c == '-')
}

fn session_from_token(value: &Value, previous: Option<&Session>) -> Option<Session> {
    let access_token = json_str(value, "access_token")?;
    if access_token.is_empty() {
        return None;
    }
    let refresh_token = json_str(value, "refresh_token")
        .filter(|token| !token.is_empty())
        .or_else(|| previous.map(|session| session.refresh_token.clone()))
        .unwrap_or_default();
    if refresh_token.is_empty() {
        return None;
    }
    let expires_in = json_u64(value, "expires_in").unwrap_or(DEFAULT_EXPIRES_IN);
    let expires_at = (Utc::now() + chrono::Duration::seconds(expires_in as i64))
        .to_rfc3339_opts(SecondsFormat::Secs, true);
    Some(Session {
        access_token,
        refresh_token,
        expires_at,
        token_type: json_str(value, "token_type").unwrap_or_else(|| "Bearer".into()),
        scope: json_str(value, "scope").unwrap_or_default(),
    })
}

fn json_str(value: &Value, key: &str) -> Option<String> {
    value.get(key).and_then(Value::as_str).map(str::to_string)
}

fn json_u64(value: &Value, key: &str) -> Option<u64> {
    value.get(key).and_then(Value::as_u64)
}

fn form_encode(pairs: &[(&str, &str)]) -> String {
    pairs
        .iter()
        .map(|(key, value)| format!("{}={}", encode(key), encode(value)))
        .collect::<Vec<_>>()
        .join("&")
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

fn try_open_browser(url: &str) {
    if !(url.starts_with("https://") || url.starts_with("http://")) {
        return;
    }
    let _ = std::process::Command::new("xdg-open")
        .arg(url)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn();
}

pub fn is_cli_session_header(name: &str) -> bool {
    matches!(name, "x-xai-token-auth" | "x-grok-model-override")
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::Router;
    use axum::extract::State;
    use axum::routing::post;
    use std::sync::atomic::{AtomicU64, Ordering};

    fn temp_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("umsp-sessions-{}-{tag}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn session_files_are_per_provider_and_private() {
        let dir = temp_dir("split");
        write_session(
            &dir,
            "grok",
            "access-a",
            "refresh-a",
            "2099-01-01T00:00:00Z",
        )
        .unwrap();
        write_session(
            &dir,
            "grok-2",
            "access-b",
            "refresh-b",
            "2099-01-01T00:00:00Z",
        )
        .unwrap();
        let a = std::fs::read_to_string(session_path(&dir, "grok").unwrap()).unwrap();
        let b = std::fs::read_to_string(session_path(&dir, "grok-2").unwrap()).unwrap();
        assert!(a.contains("access-a"));
        assert!(a.contains("refresh-a"));
        assert!(!a.contains("access-b"));
        assert!(b.contains("refresh-b"));
        assert!(!b.contains("refresh-a"));
        let mode = crate::envfile::file_mode(&session_path(&dir, "grok").unwrap()).unwrap();
        assert_eq!(mode, 0o600, "session mode {mode:o}");
        assert!(remove_session(&dir, "grok").unwrap());
        assert!(!session_ready(&dir, "grok"));
        assert!(session_ready(&dir, "grok-2"));
        assert!(session_path(&dir, "../grok").is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn form_encoding_escapes_refresh_tokens() {
        let body = form_encode(&[
            ("grant_type", "refresh_token"),
            ("refresh_token", "a b+c/d"),
            ("client_id", GROK_CLIENT_ID),
        ]);
        assert!(body.contains("grant_type=refresh_token"));
        assert!(body.contains("refresh_token=a%20b%2Bc%2Fd"));
        assert!(!body.contains("a b+c/d"));
    }

    #[derive(Clone)]
    struct TokenFake {
        hits: Arc<AtomicU64>,
    }

    async fn token_fake(State(fake): State<TokenFake>, body: String) -> String {
        let n = fake.hits.fetch_add(1, Ordering::SeqCst);
        assert!(body.contains("client_id=test-client"), "{body}");
        if body.contains("grant_type=urn%3Aietf%3Aparams%3Aoauth%3Agrant-type%3Adevice_code")
            || body.contains("grant_type=urn:ietf:params:oauth:grant-type:device_code")
        {
            return r#"{"access_token":"device-access","refresh_token":"device-refresh","expires_in":3600,"token_type":"Bearer"}"#.into();
        }
        assert!(body.contains("refresh_token="), "{body}");
        format!(
            r#"{{"access_token":"refreshed-{n}","refresh_token":"refresh-{n}","expires_in":3600,"token_type":"Bearer"}}"#
        )
    }

    async fn device_fake() -> &'static str {
        r#"{"device_code":"dev","user_code":"ABCD-1234","verification_uri":"https://auth.example/device","expires_in":600,"interval":1}"#
    }

    async fn start_oauth() -> (String, TokenFake) {
        let fake = TokenFake {
            hits: Arc::new(AtomicU64::new(0)),
        };
        let app = Router::new()
            .route("/oauth2/device/code", post(device_fake))
            .route("/oauth2/token", post(token_fake))
            .with_state(fake.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        (format!("http://{addr}"), fake)
    }

    fn http_client() -> reqwest::Client {
        let _ = rustls::crypto::ring::default_provider().install_default();
        crate::upstream::build_login_client().unwrap()
    }

    #[tokio::test]
    async fn login_and_refresh_do_not_cross_providers() {
        let dir = temp_dir("oauth");
        let (base, fake) = start_oauth().await;
        let store = SessionStore::with_endpoints(
            dir.clone(),
            http_client(),
            &format!("{base}/oauth2/device/code"),
            &format!("{base}/oauth2/token"),
            "test-client",
        );
        store.login("grok", false).await.unwrap();
        write_session(
            &dir,
            "grok-2",
            "other-access",
            "other-refresh",
            "2099-01-01T00:00:00Z",
        )
        .unwrap();
        let before = std::fs::read_to_string(session_path(&dir, "grok-2").unwrap()).unwrap();

        let token = store.access_token("grok").await.unwrap();
        assert_eq!(token, "device-access");
        // Force a refresh by pretending the proxy rejected the current token.
        let refreshed = store
            .refresh_rejected("grok", "device-access")
            .await
            .unwrap();
        assert!(refreshed.starts_with("refreshed-"), "{refreshed}");
        let grok = std::fs::read_to_string(session_path(&dir, "grok").unwrap()).unwrap();
        assert!(grok.contains(&refreshed));
        assert!(!grok.contains("other-refresh"));
        let after = std::fs::read_to_string(session_path(&dir, "grok-2").unwrap()).unwrap();
        assert_eq!(before, after, "the other account's session must stay put");
        assert!(fake.hits.load(Ordering::SeqCst) >= 1);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn a_rotated_session_is_not_overwritten() {
        let dir = temp_dir("race");
        let (base, _) = start_oauth().await;
        let store = SessionStore::with_endpoints(
            dir.clone(),
            http_client(),
            &format!("{base}/oauth2/device/code"),
            &format!("{base}/oauth2/token"),
            "test-client",
        );
        write_session(
            &dir,
            "grok",
            "old-access",
            "old-refresh",
            "2000-01-01T00:00:00Z",
        )
        .unwrap();
        // Simulate a login that lands while refresh is in flight: the file no
        // longer holds the refresh token we started with, so the store keeps it.
        let path = session_path(&dir, "grok").unwrap();
        let stale = store.read(&path).unwrap();
        write_session(
            &dir,
            "grok",
            "login-access",
            "login-refresh",
            "2099-01-01T00:00:00Z",
        )
        .unwrap();
        let kept = store
            .refresh_and_store("grok", &path, &stale)
            .await
            .unwrap();
        assert_eq!(kept.access_token, "login-access");
        assert_eq!(kept.refresh_token, "login-refresh");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
