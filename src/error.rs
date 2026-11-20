//! Error type shared by CLI commands.
//!
//! Exit codes follow the spec (section 5.3):
//!   0 success, 2 usage, 3 network/timeout, 4 upstream ban/limit/balance,
//!   5 target missing, 6 circuit open, 10 other.

use std::fmt;

pub const EXIT_OK: i32 = 0;
pub const EXIT_USAGE: i32 = 2;
pub const EXIT_NETWORK: i32 = 3;
pub const EXIT_UPSTREAM: i32 = 4;
pub const EXIT_NOT_FOUND: i32 = 5;
pub const EXIT_CIRCUIT: i32 = 6;
pub const EXIT_OTHER: i32 = 10;

#[derive(Debug, Clone)]
pub struct UmspError {
    pub code: String,
    pub message: String,
    pub exit: i32,
}

impl UmspError {
    pub fn new(code: &str, message: impl Into<String>, exit: i32) -> Self {
        Self {
            code: code.to_string(),
            message: message.into(),
            exit,
        }
    }

    pub fn usage(message: impl Into<String>) -> Self {
        Self::new("USAGE", message, EXIT_USAGE)
    }

    pub fn config(message: impl Into<String>) -> Self {
        Self::new("CONFIG_INVALID", message, EXIT_OTHER)
    }

    pub fn not_found(message: impl Into<String>) -> Self {
        Self::new("NOT_FOUND", message, EXIT_NOT_FOUND)
    }

    pub fn network(message: impl Into<String>) -> Self {
        Self::new("NETWORK", message, EXIT_NETWORK)
    }

    pub fn upstream(code: &str, message: impl Into<String>) -> Self {
        Self::new(code, message, EXIT_UPSTREAM)
    }

    pub fn other(code: &str, message: impl Into<String>) -> Self {
        Self::new(code, message, EXIT_OTHER)
    }

    pub fn to_json(&self) -> serde_json::Value {
        serde_json::json!({
            "ok": false,
            "error": { "code": self.code, "message": self.message }
        })
    }
}

impl fmt::Display for UmspError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "[{}] {}", self.code, self.message)
    }
}

impl std::error::Error for UmspError {}

pub type Result<T> = std::result::Result<T, UmspError>;
