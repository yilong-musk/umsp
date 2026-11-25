//! Configuration model: `~/.config/umsp/config.toml`.

use std::collections::{BTreeSet, HashMap, HashSet};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::error::{Result, UmspError};
use crate::util::{expand_tilde, home_dir};

pub const DEFAULT_LISTEN: &str = "127.0.0.1:3460";
pub const DEFAULT_MAX_BODY_BYTES: usize = 52_428_800; // 50 MiB
pub const DEFAULT_REQUEST_TIMEOUT_S: u64 = 600;
pub const DEFAULT_STREAM_IDLE_TIMEOUT_S: u64 = 300;
pub const DEFAULT_STREAM_HEADER_TIMEOUT_S: u64 = 300;
pub const DEFAULT_STREAM_KEEPALIVE_S: u64 = 15;
pub const DEFAULT_LOG_RETENTION_DAYS: u32 = 30;
pub const DEFAULT_CREATED: i64 = 1_789_026_056;
pub const PROTOCOLS: [&str; 3] = ["chat", "responses", "messages"];

pub fn default_config_path() -> PathBuf {
    home_dir().join(".config/umsp/config.toml")
}

pub fn default_env_path() -> PathBuf {
    home_dir().join(".config/umsp/env")
}

/// Directory of per-provider OAuth sessions, next to the config file.
///
/// `{config_dir}/sessions/<provider-id>.json`. Each oauth provider has its own
/// file, so two Grok subscriptions never share a refresh token.
pub fn sessions_dir(config_path: &Path) -> PathBuf {
    config_path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."))
        .join("sessions")
}

pub fn default_state_dir() -> PathBuf {
    home_dir().join(".local/share/umsp")
}

fn default_listen() -> String {
    DEFAULT_LISTEN.to_string()
}

fn default_max_body_bytes() -> usize {
    DEFAULT_MAX_BODY_BYTES
}

fn default_request_timeout_s() -> u64 {
    DEFAULT_REQUEST_TIMEOUT_S
}

fn default_stream_idle_timeout_s() -> u64 {
    DEFAULT_STREAM_IDLE_TIMEOUT_S
}

fn default_stream_header_timeout_s() -> u64 {
    DEFAULT_STREAM_HEADER_TIMEOUT_S
}

fn default_stream_keepalive_s() -> u64 {
    DEFAULT_STREAM_KEEPALIVE_S
}

fn default_state_dir_str() -> String {
    default_state_dir().to_string_lossy().to_string()
}

fn default_true() -> bool {
    true
}

fn default_log_retention_days() -> u32 {
    DEFAULT_LOG_RETENTION_DAYS
}

fn default_created() -> i64 {
    DEFAULT_CREATED
}

/// Provider ids double as config keys and metrics labels, so they stay a
/// lowercase slug. The guidance is the full product name: abbreviations such
/// as `od` become ambiguous as soon as a second upstream shares the prefix.
fn is_provider_slug(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 64
        && id
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '.' || c == '-')
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Config {
    #[serde(default)]
    pub server: ServerConfig,
    #[serde(default)]
    pub providers: Vec<Provider>,
    #[serde(default)]
    pub models: Vec<Model>,
    #[serde(default)]
    pub clients: Vec<Client>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ServerConfig {
    #[serde(default = "default_listen")]
    pub listen: String,
    #[serde(default = "default_max_body_bytes")]
    pub max_body_bytes: usize,
    #[serde(default = "default_request_timeout_s")]
    pub request_timeout_s: u64,
    #[serde(default = "default_stream_idle_timeout_s")]
    pub stream_idle_timeout_s: u64,
    #[serde(default = "default_stream_header_timeout_s")]
    pub stream_header_timeout_s: u64,
    #[serde(default = "default_stream_keepalive_s")]
    pub stream_keepalive_s: u64,
    #[serde(default = "default_state_dir_str")]
    pub state_dir: String,
    #[serde(default = "default_true")]
    pub responses_emit_event_lines: bool,
    #[serde(default = "default_log_retention_days")]
    pub log_retention_days: u32,
}

impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            listen: default_listen(),
            max_body_bytes: default_max_body_bytes(),
            request_timeout_s: default_request_timeout_s(),
            stream_idle_timeout_s: default_stream_idle_timeout_s(),
            stream_header_timeout_s: default_stream_header_timeout_s(),
            stream_keepalive_s: default_stream_keepalive_s(),
            state_dir: default_state_dir_str(),
            responses_emit_event_lines: true,
            log_retention_days: default_log_retention_days(),
        }
    }
}

impl ServerConfig {
    pub fn state_path(&self) -> PathBuf {
        expand_tilde(&self.state_dir)
    }

    pub fn logs_dir(&self) -> PathBuf {
        self.state_path().join("logs")
    }

    pub fn pid_file(&self) -> PathBuf {
        self.state_path().join("umsp.pid")
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Provider {
    pub id: String,
    /// Built-in vendor this instance was created from (`umsp vendor list`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub vendor: Option<String>,
    pub base_url: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub anthropic_base_url: Option<String>,
    /// Env variable holding the upstream key. Empty when `auth` is `oauth`.
    #[serde(default)]
    pub api_key_env: String,
    /// `api_key` (env file) or `oauth` (one Grok subscription session per provider).
    #[serde(default = "default_auth")]
    pub auth: String,
    #[serde(default = "default_all_protocols")]
    pub protocols: Vec<String>,
    #[serde(default)]
    pub quirks: Option<String>,
}

fn default_all_protocols() -> Vec<String> {
    PROTOCOLS.iter().map(|s| s.to_string()).collect()
}

fn default_auth() -> String {
    "api_key".to_string()
}

pub const AUTH_API_KEY: &str = "api_key";
pub const AUTH_OAUTH: &str = "oauth";

impl Provider {
    pub fn supports(&self, protocol: &str) -> bool {
        self.protocols.iter().any(|p| p == protocol)
    }

    /// Grok subscription session, stored per provider id. Not an env API key.
    pub fn uses_oauth(&self) -> bool {
        self.auth == AUTH_OAUTH
    }

    /// Whether `POST /v1/messages/count_tokens` can be proxied to this provider.
    ///
    /// OpenDesign answers 404 on it (the gateway turns that into a
    /// protocol-native 501); DeepSeek's Anthropic-compatible endpoint
    /// implements it (spec 2.3). The capability rides the quirks tag because it
    /// is a provider behaviour, not a per-request setting.
    pub fn supports_count_tokens(&self) -> bool {
        self.quirks.as_deref() == Some("deepseek") && self.anthropic_base_url.is_some()
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Model {
    pub id: String,
    #[serde(default)]
    pub aliases: Vec<String>,
    #[serde(default)]
    pub context_length: u64,
    /// Every context tier an upstream published, ascending and without duplicates.
    /// `context_length` is the largest of these when more than one is present,
    /// so a caller that reads only one number can use the full window.
    #[serde(default)]
    pub context_lengths: Vec<u64>,
    #[serde(default)]
    pub max_completion_tokens: u64,
    #[serde(default)]
    pub capabilities: Vec<String>,
    #[serde(default)]
    pub display_name: Option<String>,
    #[serde(default = "default_created")]
    pub created: i64,
    /// Upstream backends that can serve this model. Configuration order is the
    /// tie-break after usage and balance. A model is one logical entry for
    /// clients; providers are an implementation detail.
    #[serde(default)]
    pub backends: Vec<Backend>,
    #[serde(default)]
    pub description: Option<String>,
}

/// One way to serve a model: a provider plus that provider's upstream id.
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
pub struct Backend {
    pub provider: String,
    pub upstream: String,
}

impl Model {
    pub fn display_name(&self) -> String {
        self.display_name.clone().unwrap_or_else(|| self.id.clone())
    }
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct Client {
    pub name: String,
    #[serde(default)]
    pub key_hash: String,
    #[serde(default)]
    pub key_prefix: String,
    #[serde(default)]
    pub allowed_models: Vec<String>,
    #[serde(default = "default_all_protocols")]
    pub allowed_protocols: Vec<String>,
    /// RFC3339 instant from which the key is rejected. Absent: never expires.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expires_at: Option<String>,
    /// Model calls this client may have in flight at once. Absent: unlimited.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_concurrency: Option<u32>,
}

impl Client {
    /// Parsed `expires_at`. `None` when unset or malformed (`validate`
    /// reports the malformed case).
    pub fn expires_at_utc(&self) -> Option<chrono::DateTime<chrono::Utc>> {
        self.expires_at.as_deref().and_then(parse_expiry)
    }

    /// True once `now` reaches `expires_at`. A malformed value counts as
    /// expired: an unreadable limit must fail closed, not open.
    pub fn is_expired(&self, now: chrono::DateTime<chrono::Utc>) -> bool {
        match self.expires_at.as_deref() {
            None => false,
            Some(raw) => parse_expiry(raw).is_none_or(|at| now >= at),
        }
    }
}

fn parse_expiry(raw: &str) -> Option<chrono::DateTime<chrono::Utc>> {
    chrono::DateTime::parse_from_rfc3339(raw.trim())
        .ok()
        .map(|dt| dt.with_timezone(&chrono::Utc))
}

#[derive(Debug, Clone, Serialize)]
pub struct Issue {
    pub level: &'static str,
    pub code: String,
    pub message: String,
}

fn err(code: &str, message: impl Into<String>) -> Issue {
    Issue {
        level: "error",
        code: code.to_string(),
        message: message.into(),
    }
}

fn warn(code: &str, message: impl Into<String>) -> Issue {
    Issue {
        level: "warning",
        code: code.to_string(),
        message: message.into(),
    }
}

impl Config {
    pub fn resolve_path(flag: Option<&Path>) -> PathBuf {
        if let Some(p) = flag {
            return p.to_path_buf();
        }
        if let Some(p) = std::env::var_os("UMSP_CONFIG") {
            return PathBuf::from(p);
        }
        default_config_path()
    }

    pub fn resolve_env_path() -> PathBuf {
        if let Some(p) = std::env::var_os("UMSP_ENV_FILE") {
            return PathBuf::from(p);
        }
        default_env_path()
    }

    /// Parse a config file. Does not run validation; callers decide how strict to be.
    pub fn load(path: &Path) -> Result<Self> {
        let raw = std::fs::read_to_string(path).map_err(|e| {
            UmspError::config(format!("cannot read config {}: {e}", path.display()))
        })?;
        let cfg: Config = toml::from_str(&raw).map_err(|e| {
            UmspError::config(format!("cannot parse config {}: {e}", path.display()))
        })?;
        let mut cfg = cfg;
        cfg.apply_env_overrides();
        Ok(cfg)
    }

    /// Environment overrides below CLI flags (spec 5.2): `UMSP_STATE_DIR`.
    fn apply_env_overrides(&mut self) {
        if let Ok(dir) = std::env::var("UMSP_STATE_DIR") {
            let dir = dir.trim();
            if !dir.is_empty() {
                self.server.state_dir = dir.to_string();
            }
        }
    }

    /// Parse + validate, failing on any error-level issue. Used by `serve`.
    pub fn load_checked(path: &Path) -> Result<Self> {
        let cfg = Self::load(path)?;
        let issues = cfg.validate();
        let errors: Vec<&Issue> = issues.iter().filter(|i| i.level == "error").collect();
        if !errors.is_empty() {
            let joined = errors
                .iter()
                .map(|i| format!("{}: {}", i.code, i.message))
                .collect::<Vec<_>>()
                .join("; ");
            return Err(UmspError::config(joined));
        }
        Ok(cfg)
    }

    /// Load a config for CLI commands: falls back to defaults when the file is absent.
    pub fn load_or_default(path: &Path) -> Result<Self> {
        if path.exists() {
            Self::load(path)
        } else {
            let mut cfg = Config {
                server: ServerConfig::default(),
                providers: Vec::new(),
                models: Vec::new(),
                clients: Vec::new(),
            };
            cfg.apply_env_overrides();
            Ok(cfg)
        }
    }

    pub fn validate(&self) -> Vec<Issue> {
        let mut issues = Vec::new();

        if self.server.listen.parse::<std::net::SocketAddr>().is_err() {
            issues.push(err(
                "listen_invalid",
                format!(
                    "server.listen '{}' is not a valid host:port",
                    self.server.listen
                ),
            ));
        }
        if self.server.max_body_bytes == 0 {
            issues.push(err("max_body_invalid", "server.max_body_bytes must be > 0"));
        }
        if self.server.request_timeout_s == 0 {
            issues.push(err(
                "timeout_invalid",
                "server.request_timeout_s must be > 0",
            ));
        }
        if self.server.stream_idle_timeout_s == 0 {
            issues.push(err(
                "stream_idle_timeout_invalid",
                "server.stream_idle_timeout_s must be > 0",
            ));
        }
        if self.server.stream_header_timeout_s == 0 {
            issues.push(err(
                "stream_header_timeout_invalid",
                "server.stream_header_timeout_s must be > 0",
            ));
        }
        if self.server.stream_keepalive_s == 0 {
            issues.push(err(
                "stream_keepalive_invalid",
                "server.stream_keepalive_s must be > 0",
            ));
        }

        let mut provider_ids = BTreeSet::new();
        for p in &self.providers {
            if p.id.trim().is_empty() {
                issues.push(err("provider_id_empty", "provider id must not be empty"));
            } else if !is_provider_slug(&p.id) {
                issues.push(err(
                    "provider_id_format",
                    format!(
                        "provider id '{}' must be lowercase letters, digits, '.' or '-', and should be the full product name (e.g. 'open-design', not 'od')",
                        p.id
                    ),
                ));
            } else if p.id.chars().count() <= 2 {
                issues.push(warn(
                    "provider_id_short",
                    format!(
                        "provider id '{}' looks like an abbreviation; use the full product name (e.g. 'open-design')",
                        p.id
                    ),
                ));
            }
            if !provider_ids.insert(p.id.clone()) {
                issues.push(err(
                    "provider_duplicate",
                    format!("duplicate provider id '{}'", p.id),
                ));
            }
            match &p.vendor {
                Some(vendor) => {
                    if crate::vendors::find(vendor).is_none() {
                        issues.push(err(
                            "provider_vendor_unknown",
                            format!(
                                "provider '{}' uses unknown vendor '{}'; run `umsp vendor list`",
                                p.id, vendor
                            ),
                        ));
                    }
                }
                None => issues.push(warn(
                    "provider_vendor_missing",
                    format!(
                        "provider '{}' has no vendor; set `vendor = \"...\"` (see `umsp vendor list`)",
                        p.id
                    ),
                )),
            }
            if !(p.base_url.starts_with("http://") || p.base_url.starts_with("https://")) {
                issues.push(err(
                    "provider_base_url",
                    format!(
                        "provider '{}' base_url must start with http:// or https://",
                        p.id
                    ),
                ));
            }
            if p.auth != AUTH_API_KEY && p.auth != AUTH_OAUTH {
                issues.push(err(
                    "provider_auth",
                    format!(
                        "provider '{}' auth must be \"{}\" or \"{}\"",
                        p.id, AUTH_API_KEY, AUTH_OAUTH
                    ),
                ));
            }
            let vendor_spec = p.vendor.as_deref().and_then(crate::vendors::find);
            let vendor_requires_oauth = vendor_spec.is_some_and(|v| v.auth == AUTH_OAUTH);
            if vendor_requires_oauth && !p.uses_oauth() {
                issues.push(err(
                    "provider_auth",
                    format!(
                        "provider '{}' uses vendor {} and needs auth = \"oauth\"",
                        p.id,
                        p.vendor.as_deref().unwrap_or("oauth")
                    ),
                ));
            }
            if p.uses_oauth() && !vendor_requires_oauth {
                let oauth_vendors: Vec<&str> = crate::vendors::VENDORS
                    .iter()
                    .filter(|v| v.auth == AUTH_OAUTH)
                    .map(|v| v.id)
                    .collect();
                issues.push(err(
                    "provider_auth",
                    format!(
                        "provider '{}' auth = \"oauth\" is only valid for vendor {}",
                        p.id,
                        oauth_vendors.join(" or ")
                    ),
                ));
            }
            if p.uses_oauth() {
                if !p.api_key_env.trim().is_empty() {
                    issues.push(warn(
                        "provider_api_key_env",
                        format!(
                            "provider '{}' auth is oauth; api_key_env is ignored (run `umsp login {}`)",
                            p.id, p.id
                        ),
                    ));
                }
            } else if p.api_key_env.trim().is_empty() {
                issues.push(err(
                    "provider_api_key_env",
                    format!("provider '{}' needs api_key_env", p.id),
                ));
            }
            if p.protocols.is_empty() {
                issues.push(err(
                    "provider_protocols",
                    format!("provider '{}' supports no protocols", p.id),
                ));
            }
            for proto in &p.protocols {
                if !PROTOCOLS.contains(&proto.as_str()) {
                    issues.push(err(
                        "provider_protocol_unknown",
                        format!("provider '{}' has unknown protocol '{proto}'", p.id),
                    ));
                }
            }
            if let Some(q) = &p.quirks {
                if !["opendesign", "deepseek", "none"].contains(&q.as_str()) {
                    issues.push(warn(
                        "provider_quirks_unknown",
                        format!("provider '{}' has unknown quirks '{q}'", p.id),
                    ));
                }
            }
        }

        let mut model_ids: HashSet<&str> = HashSet::new();
        for m in &self.models {
            if m.id.trim().is_empty() {
                issues.push(err("model_id_empty", "model id must not be empty"));
            }
            if !model_ids.insert(m.id.as_str()) {
                issues.push(err(
                    "model_duplicate",
                    format!("duplicate model id '{}'", m.id),
                ));
            }
            if m.backends.is_empty() {
                issues.push(err(
                    "model_backends_empty",
                    format!("model '{}' needs at least one backend", m.id),
                ));
            }
            let mut seen_providers = BTreeSet::new();
            let mut seen_routes = BTreeSet::new();
            for b in &m.backends {
                if !provider_ids.contains(&b.provider) {
                    issues.push(err(
                        "model_provider_missing",
                        format!(
                            "model '{}' references unknown provider '{}'",
                            m.id, b.provider
                        ),
                    ));
                }
                if b.upstream.trim().is_empty() {
                    issues.push(err(
                        "model_upstream_empty",
                        format!("model '{}' has a backend without upstream id", m.id),
                    ));
                }
                if !seen_providers.insert(b.provider.as_str()) {
                    issues.push(warn(
                        "backend_duplicate_provider",
                        format!(
                            "model '{}' lists provider '{}' twice; the later backend only runs after the first one fails",
                            m.id, b.provider
                        ),
                    ));
                }
                if !seen_routes.insert((b.provider.as_str(), b.upstream.as_str())) {
                    issues.push(warn(
                        "backend_duplicate",
                        format!(
                            "model '{}' repeats the backend '{}:{}'",
                            m.id, b.provider, b.upstream
                        ),
                    ));
                }
            }
            if m.context_length == 0 {
                issues.push(warn(
                    "model_context_length_missing",
                    format!("model '{}' has no context_length", m.id),
                ));
            }
            if m.max_completion_tokens == 0 {
                issues.push(warn(
                    "model_max_completion_missing",
                    format!("model '{}' has no max_completion_tokens", m.id),
                ));
            }
        }

        // Second pass: aliases are checked only once every model id is known,
        // so an alias declared before the model it collides with is still caught.
        let mut alias_owner: HashMap<&str, &str> = HashMap::new();
        for m in &self.models {
            for alias in &m.aliases {
                if alias == &m.id {
                    issues.push(warn(
                        "alias_self",
                        format!("model '{}' lists its own id as alias", m.id),
                    ));
                    continue;
                }
                if model_ids.contains(alias.as_str()) {
                    issues.push(err(
                        "alias_collides_with_model",
                        format!(
                            "alias '{alias}' of model '{}' collides with a model id",
                            m.id
                        ),
                    ));
                }
                if let Some(owner) = alias_owner.insert(alias.as_str(), m.id.as_str()) {
                    if owner != m.id {
                        issues.push(err(
                            "alias_duplicate",
                            format!("alias '{alias}' is used by both '{owner}' and '{}'", m.id),
                        ));
                    }
                }
            }
        }

        let mut client_names = BTreeSet::new();
        for c in &self.clients {
            if c.name.trim().is_empty() {
                issues.push(err("client_name_empty", "client name must not be empty"));
            }
            if !client_names.insert(c.name.clone()) {
                issues.push(err(
                    "client_duplicate",
                    format!("duplicate client name '{}'", c.name),
                ));
            }
            if !valid_key_hash(&c.key_hash) {
                issues.push(err(
                    "client_key_hash",
                    format!(
                        "client '{}' key_hash must look like 'sha256:<64 hex chars>'",
                        c.name
                    ),
                ));
            }
            for proto in &c.allowed_protocols {
                if !PROTOCOLS.contains(&proto.as_str()) {
                    issues.push(err(
                        "client_protocol_unknown",
                        format!("client '{}' has unknown protocol '{proto}'", c.name),
                    ));
                }
            }
            if let Some(raw) = &c.expires_at {
                if parse_expiry(raw).is_none() {
                    issues.push(err(
                        "client_expires_at",
                        format!(
                            "client '{}' expires_at must be an RFC3339 timestamp, got '{raw}'",
                            c.name
                        ),
                    ));
                }
            }
            if let Some(limit) = c.max_concurrency {
                if !crate::limits::MAX_CONCURRENCY_RANGE.contains(&limit) {
                    issues.push(err(
                        "client_max_concurrency",
                        format!(
                            "client '{}' max_concurrency must be between 1 and 9, got {limit}",
                            c.name
                        ),
                    ));
                }
            }
        }

        if self.providers.is_empty() {
            issues.push(warn(
                "no_providers",
                "no providers configured; add one with `umsp provider add`",
            ));
        }
        if self.models.is_empty() {
            issues.push(warn(
                "no_models",
                "no models configured; add one with `umsp model add`",
            ));
        }
        if self.clients.is_empty() {
            issues.push(warn(
                "no_clients",
                "no clients configured; every request will be rejected with 401 until `umsp client add`",
            ));
        }

        issues
    }

    pub fn provider(&self, id: &str) -> Option<&Provider> {
        self.providers.iter().find(|p| p.id == id)
    }

    pub fn model(&self, id: &str) -> Option<&Model> {
        self.models.iter().find(|m| m.id == id)
    }

    pub fn client(&self, name: &str) -> Option<&Client> {
        self.clients.iter().find(|c| c.name == name)
    }
}

pub fn valid_key_hash(hash: &str) -> bool {
    match hash.strip_prefix("sha256:") {
        Some(hex) => hex.len() == 64 && hex.bytes().all(|b| b.is_ascii_hexdigit()),
        None => false,
    }
}

pub const CONFIG_TEMPLATE: &str = r#"# umsp — AI Gateway configuration
# Secrets live in ~/.config/umsp/env (mode 0600) and are referenced by api_key_env.
#
# Provider ids are full product names, not in-house abbreviations: once several
# upstreams exist, "od" could mean anything. Lowercase letters, digits, "." and
# "-" only.

[server]
listen = "127.0.0.1:3460"
max_body_bytes = 52428800
request_timeout_s = 600
stream_idle_timeout_s = 300
stream_header_timeout_s = 300
stream_keepalive_s = 15
state_dir = "~/.local/share/umsp"
responses_emit_event_lines = true
log_retention_days = 30

[[providers]]
id = "open-design"
vendor = "open-design"
base_url = "https://amr-link.open-design.ai/v1"
api_key_env = "OPEN_DESIGN_API_KEY"
protocols = ["chat", "responses", "messages"]
quirks = "opendesign"

[[providers]]
id = "deepseek"
vendor = "deepseek"
base_url = "https://api.deepseek.com"
anthropic_base_url = "https://api.deepseek.com/anthropic"
api_key_env = "DEEPSEEK_API_KEY"
protocols = ["chat", "responses", "messages"]
quirks = "deepseek"

# Grok subscription. Each `umsp provider add --vendor grok` is its own account
# (grok, grok-2, …) with its own session file. Sign in with `umsp login <id>`.
# Protocols: responses only. Do not set api_key_env.
# [[providers]]
# id = "grok"
# vendor = "grok"
# auth = "oauth"
# base_url = "https://cli-chat-proxy.grok.com/v1"
# protocols = ["responses"]

# A model is one logical entry for clients. `backends` lists the upstreams that
# can serve it. Healthy providers are ordered by usage, then unknown capacity,
# then balance; this list is only the tie-break. Later backends cover retryable
# failures (same protocol, one hop each, never a loop).
# Providers and upstream ids stay internal; clients only see `id`/aliases.
[[models]]
id = "deepseek-v4.1-flash"
aliases = ["deepseek-flash"]
context_length = 1048000
max_completion_tokens = 128000
capabilities = ["text", "vision", "tools", "reasoning"]
display_name = "DeepSeek V4.1 Flash"
backends = [
  { provider = "open-design", upstream = "deepseek-v4.1-flash" },
  { provider = "deepseek", upstream = "deepseek-flash" },
]

# `claude`-named so Claude Code's model discovery keeps it (it only lists ids
# containing "claude"/"anthropic").
[[models]]
id = "claude-haiku-5.5"
context_length = 1000000
max_completion_tokens = 128000
capabilities = ["text", "vision", "tools", "reasoning"]
display_name = "Claude Haiku 5.5"
backends = [
  { provider = "open-design", upstream = "claude-haiku-5.5" },
]

# Client keys are created with `umsp client add --name <name>`; only hashes are stored.
# [[clients]]
# name = "planner"
# key_hash = "sha256:<64 hex chars>"
# key_prefix = "umsp_ab12cd34"
# allowed_models = ["deepseek-*", "claude-*"]
# allowed_protocols = ["chat", "responses", "messages"]
"#;

pub const ENV_TEMPLATE: &str = r#"# umsp environment file (mode 0600) — loaded by systemd and by the umsp CLI.
# Never commit this file. Set the real upstream keys below.
#OPEN_DESIGN_API_KEY=
#DEEPSEEK_API_KEY=
"#;

#[cfg(test)]
mod tests {
    use super::*;

    fn base_config() -> Config {
        Config {
            server: ServerConfig::default(),
            providers: vec![Provider {
                id: "od".into(),
                vendor: Some("open-design".into()),
                base_url: "https://example.test/v1".into(),
                anthropic_base_url: None,
                api_key_env: "OD_KEY".into(),
                auth: AUTH_API_KEY.into(),
                protocols: default_all_protocols(),
                quirks: Some("opendesign".into()),
            }],
            models: vec![Model {
                id: "od/m1".into(),
                backends: vec![Backend {
                    provider: "od".into(),
                    upstream: "m1".into(),
                }],
                aliases: vec!["m1".into()],
                context_length: 1000,
                context_lengths: Vec::new(),
                max_completion_tokens: 100,
                capabilities: vec!["text".into()],
                display_name: None,
                created: DEFAULT_CREATED,
                description: None,
            }],
            clients: vec![Client {
                name: "c1".into(),
                key_hash: format!("sha256:{}", "a".repeat(64)),
                key_prefix: "umsp_aaaa".into(),
                allowed_models: vec!["od/*".into()],
                allowed_protocols: default_all_protocols(),
                ..Default::default()
            }],
        }
    }

    #[test]
    fn valid_config_has_no_errors() {
        let issues = base_config().validate();
        let errors: Vec<_> = issues.iter().filter(|i| i.level == "error").collect();
        assert!(errors.is_empty(), "unexpected errors: {errors:?}");
    }

    #[test]
    fn duplicate_alias_is_error() {
        let mut cfg = base_config();
        cfg.models.push(Model {
            id: "od/m2".into(),
            backends: vec![Backend {
                provider: "od".into(),
                upstream: "m2".into(),
            }],
            aliases: vec!["m1".into()],
            context_length: 1000,
            context_lengths: Vec::new(),
            max_completion_tokens: 100,
            capabilities: vec![],
            display_name: None,
            created: DEFAULT_CREATED,
            description: None,
        });
        let issues = cfg.validate();
        assert!(issues.iter().any(|i| i.code == "alias_duplicate"));
    }

    fn model_with(id: &str, aliases: &[&str]) -> Model {
        Model {
            id: id.into(),
            backends: vec![Backend {
                provider: "od".into(),
                upstream: id.to_string(),
            }],
            aliases: aliases.iter().map(|a| (*a).to_string()).collect(),
            context_length: 1000,
            context_lengths: Vec::new(),
            max_completion_tokens: 100,
            capabilities: vec![],
            display_name: None,
            created: DEFAULT_CREATED,
            description: None,
        }
    }

    /// Two-letter ids are almost always in-house abbreviations, and two
    /// vendors can easily collide on one (`od`). Warn, don't reject: real
    /// vendor names may be short.
    #[test]
    fn abbreviated_provider_id_warns() {
        let cfg = base_config();
        let issues = cfg.validate();
        assert!(
            issues.iter().any(|i| i.code == "provider_id_short"),
            "expected provider_id_short, got {issues:?}"
        );
        assert!(
            !issues.iter().any(|i| i.code == "provider_id_format"),
            "a two-letter slug is still a valid slug"
        );
    }

    /// alias is declared before the model id it collides with — this order used
    /// to slip through the single-pass check.
    #[test]
    fn alias_colliding_with_later_model_id_is_error() {
        let mut cfg = base_config();
        cfg.models[0].aliases = vec!["shared".into()];
        cfg.models.push(model_with("shared", &[]));
        let issues = cfg.validate();
        assert!(
            issues.iter().any(|i| i.code == "alias_collides_with_model"),
            "expected alias_collides_with_model, got {issues:?}"
        );
    }

    /// Same collision with the model id declared first — must behave identically.
    #[test]
    fn alias_colliding_with_earlier_model_id_is_error() {
        let mut cfg = base_config();
        cfg.models.insert(0, model_with("shared", &[]));
        cfg.models[1].aliases = vec!["shared".into()];
        let issues = cfg.validate();
        assert!(
            issues.iter().any(|i| i.code == "alias_collides_with_model"),
            "expected alias_collides_with_model, got {issues:?}"
        );
    }

    /// A model may keep its own canonical id as an alias (warn, not error), and
    /// unrelated aliases must stay clean.
    #[test]
    fn alias_self_is_only_a_warning() {
        let mut cfg = base_config();
        cfg.models[0].aliases = vec!["od/m1".into(), "m1".into()];
        let issues = cfg.validate();
        assert!(issues.iter().any(|i| i.code == "alias_self"));
        assert!(!issues.iter().any(|i| i.code == "alias_collides_with_model"));
        assert!(issues.iter().filter(|i| i.level == "error").count() == 0);
    }

    #[test]
    fn unknown_provider_is_error() {
        let mut cfg = base_config();
        cfg.models[0].backends[0].provider = "nope".into();
        let issues = cfg.validate();
        assert!(issues.iter().any(|i| i.code == "model_provider_missing"));
    }

    #[test]
    fn bad_key_hash_is_error() {
        let mut cfg = base_config();
        cfg.clients[0].key_hash = "sha256:xyz".into();
        let issues = cfg.validate();
        assert!(issues.iter().any(|i| i.code == "client_key_hash"));
    }

    #[test]
    fn client_expiry_parses_validates_and_fails_closed() {
        let mut cfg = base_config();
        let now = chrono::Utc::now();
        assert!(!cfg.clients[0].is_expired(now), "no expiry = never expires");

        cfg.clients[0].expires_at = Some("2999-01-01T00:00:00Z".into());
        assert!(!cfg.clients[0].is_expired(now));
        assert!(cfg.validate().iter().all(|i| i.code != "client_expires_at"));

        cfg.clients[0].expires_at = Some("2020-01-01T00:00:00+08:00".into());
        assert!(cfg.clients[0].is_expired(now));

        cfg.clients[0].expires_at = Some("tomorrow".into());
        assert!(cfg.clients[0].is_expired(now), "malformed must fail closed");
        assert!(cfg.validate().iter().any(|i| i.code == "client_expires_at"));
    }

    #[test]
    fn client_max_concurrency_must_be_1_to_9() {
        let mut cfg = base_config();
        for ok in [1, 9] {
            cfg.clients[0].max_concurrency = Some(ok);
            assert!(
                cfg.validate()
                    .iter()
                    .all(|i| i.code != "client_max_concurrency")
            );
        }
        for bad in [0, 10] {
            cfg.clients[0].max_concurrency = Some(bad);
            assert!(
                cfg.validate()
                    .iter()
                    .any(|i| i.code == "client_max_concurrency")
            );
        }
    }

    #[test]
    fn model_needs_a_backend() {
        let mut cfg = base_config();
        cfg.models[0].backends.clear();
        let issues = cfg.validate();
        assert!(issues.iter().any(|i| i.code == "model_backends_empty"));
    }

    fn add_second_provider(cfg: &mut Config, protocols: Vec<&str>) {
        cfg.providers.push(Provider {
            id: "deepseek".into(),
            vendor: Some("deepseek".into()),
            base_url: "https://api.deepseek.com".into(),
            anthropic_base_url: Some("https://api.deepseek.com/anthropic".into()),
            api_key_env: "DS_KEY".into(),
            auth: AUTH_API_KEY.into(),
            protocols: protocols.iter().map(|p| (*p).to_string()).collect(),
            quirks: Some("deepseek".into()),
        });
        cfg.models.push(Model {
            id: "deepseek/m1".into(),
            backends: vec![Backend {
                provider: "deepseek".into(),
                upstream: "m1".into(),
            }],
            aliases: vec![],
            context_length: 1000,
            context_lengths: Vec::new(),
            max_completion_tokens: 100,
            capabilities: vec![],
            display_name: None,
            created: DEFAULT_CREATED,
            description: None,
        });
    }

    #[test]
    fn multi_backend_model_is_valid() {
        let mut cfg = base_config();
        add_second_provider(&mut cfg, vec!["chat", "responses", "messages"]);
        cfg.models[0].backends.push(Backend {
            provider: "deepseek".into(),
            upstream: "m1".into(),
        });
        let issues = cfg.validate();
        let errors: Vec<_> = issues.iter().filter(|i| i.level == "error").collect();
        assert!(errors.is_empty(), "unexpected errors: {errors:?}");
    }

    #[test]
    fn duplicate_backend_provider_is_only_a_warning() {
        let mut cfg = base_config();
        cfg.models[0].backends.push(Backend {
            provider: "od".into(),
            upstream: "m1-backup".into(),
        });
        let issues = cfg.validate();
        assert!(
            issues
                .iter()
                .any(|i| i.code == "backend_duplicate_provider")
        );
        assert!(issues.iter().filter(|i| i.level == "error").count() == 0);
    }

    #[test]
    fn backend_with_partial_protocol_support_is_allowed() {
        let mut cfg = base_config();
        add_second_provider(&mut cfg, vec!["chat"]);
        cfg.models[0].backends.push(Backend {
            provider: "deepseek".into(),
            upstream: "m1".into(),
        });
        let errors: Vec<_> = cfg
            .validate()
            .into_iter()
            .filter(|i| i.level == "error")
            .collect();
        assert!(errors.is_empty(), "unexpected errors: {errors:?}");
    }

    #[test]
    fn oauth_provider_does_not_need_an_api_key() {
        let mut cfg = base_config();
        cfg.providers.push(Provider {
            id: "grok".into(),
            vendor: Some("grok".into()),
            base_url: "https://cli-chat-proxy.grok.com/v1".into(),
            anthropic_base_url: None,
            api_key_env: String::new(),
            auth: AUTH_OAUTH.into(),
            protocols: vec!["responses".into()],
            quirks: None,
        });
        cfg.providers.push(Provider {
            id: "grok-2".into(),
            vendor: Some("grok".into()),
            base_url: "https://cli-chat-proxy.grok.com/v1".into(),
            anthropic_base_url: None,
            api_key_env: String::new(),
            auth: AUTH_OAUTH.into(),
            protocols: vec!["responses".into()],
            quirks: None,
        });
        let errors: Vec<_> = cfg
            .validate()
            .into_iter()
            .filter(|issue| issue.level == "error")
            .collect();
        assert!(errors.is_empty(), "unexpected errors: {errors:?}");
        assert!(cfg.providers[1].uses_oauth());
        assert!(!cfg.providers[0].uses_oauth());
    }

    #[test]
    fn grok_vendor_with_an_api_key_auth_is_rejected() {
        let mut cfg = base_config();
        cfg.providers.push(Provider {
            id: "grok".into(),
            vendor: Some("grok".into()),
            base_url: "https://cli-chat-proxy.grok.com/v1".into(),
            anthropic_base_url: None,
            api_key_env: "XAI_API_KEY".into(),
            auth: AUTH_API_KEY.into(),
            protocols: vec!["responses".into()],
            quirks: None,
        });
        assert!(
            cfg.validate()
                .iter()
                .any(|issue| issue.code == "provider_auth")
        );
    }

    #[test]
    fn oauth_without_a_grok_vendor_is_rejected() {
        let mut cfg = base_config();
        cfg.providers.push(Provider {
            id: "rogue".into(),
            vendor: None,
            base_url: "https://evil.example/v1".into(),
            anthropic_base_url: None,
            api_key_env: String::new(),
            auth: AUTH_OAUTH.into(),
            protocols: vec!["chat".into()],
            quirks: None,
        });
        assert!(
            cfg.validate()
                .iter()
                .any(|issue| issue.code == "provider_auth")
        );
    }

    #[test]
    fn template_parses() {
        let cfg: Config = toml::from_str(CONFIG_TEMPLATE).unwrap();
        assert_eq!(cfg.server.listen, DEFAULT_LISTEN);
        assert_eq!(cfg.providers[0].id, "open-design");
        assert_eq!(cfg.models[0].backends[0].upstream, "deepseek-v4.1-flash");
        assert_eq!(cfg.models[0].backends[1].provider, "deepseek");
        assert!(cfg.clients.is_empty());
    }
}
