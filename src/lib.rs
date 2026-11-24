//! umsp — lightweight local AI gateway: one binary, three protocols.
//!
//! M2 scope: config/auth bootstrap, `/v1/models`, `/healthz`, OpenAI Chat
//! Completions (non-streaming + SSE) and OpenAI Responses (non-streaming + SSE
//! with `event:` line patching) against OpenDesign. The library target exists so
//! integration tests can drive the real router.

pub mod bindings;
pub mod catalog;
pub mod chatgpt_oauth;
pub mod circuit;
pub mod cli;
pub mod commands;
pub mod config;
pub mod envfile;
pub mod error;
pub mod grok_billing;
pub mod health;
pub mod keys;
pub mod metrics;
pub mod model_defaults;
pub mod pidfile;
pub mod quirks;
pub mod registry;
pub mod route;
pub mod server;
pub mod sessions;
pub mod stream;
pub mod translate;
pub mod upstream;
pub mod usage;
pub mod util;
pub mod vendors;
