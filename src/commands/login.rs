//! `umsp login` / `umsp logout` for one oauth provider at a time.
//!
//! The session file is `{config_dir}/sessions/<provider-id>.json`. Logging in
//! `grok-2` does not read or write `grok`'s file.

use std::path::Path;

use serde_json::json;

use crate::cli::{LoginArgs, LogoutArgs};
use crate::config::Config;
use crate::error::{Result, UmspError};
use crate::sessions::SessionStore;

pub fn login(config_path: &Path, args: LoginArgs, json: bool) -> Result<i32> {
    let cfg = Config::load(config_path)?;
    let provider = cfg.provider(&args.provider).ok_or_else(|| {
        UmspError::not_found(format!(
            "provider '{}' not found; add one with `umsp provider add --vendor grok`",
            args.provider
        ))
    })?;
    if !provider.uses_oauth() {
        return Err(UmspError::usage(format!(
            "provider '{}' uses an API key ({}), not a subscription session",
            provider.id, provider.api_key_env
        )));
    }
    let provider_id = provider.id.clone();
    let dir = crate::config::sessions_dir(config_path);
    let http = crate::upstream::build_login_client()?;
    let store = SessionStore::new(dir.clone(), http);
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|err| UmspError::other("RUNTIME", format!("tokio: {err}")))?;
    let auth_flow = crate::adapter::for_provider(provider).auth_flow();
    rt.block_on(async {
        match auth_flow {
            crate::adapter::AuthFlow::ChatGptOAuth => {
                store.login_chatgpt(&provider_id, !args.no_browser).await
            }
            crate::adapter::AuthFlow::GrokOAuth | crate::adapter::AuthFlow::ApiKey => {
                store.login(&provider_id, !args.no_browser).await
            }
        }
    })?;
    let session = dir.join(format!("{provider_id}.json"));
    crate::commands::emit(
        json,
        json!({
            "provider": provider_id,
            "session": session,
        }),
        || format!("signed in provider {provider_id}"),
    )
}

pub fn logout(config_path: &Path, args: LogoutArgs, json: bool) -> Result<i32> {
    let cfg = Config::load(config_path)?;
    if cfg.provider(&args.provider).is_none() {
        return Err(UmspError::not_found(format!(
            "provider '{}' not found",
            args.provider
        )));
    }
    let removed =
        crate::sessions::remove_session(&crate::config::sessions_dir(config_path), &args.provider)?;
    if !removed {
        return Err(UmspError::not_found(format!(
            "provider '{}' has no oauth session",
            args.provider
        )));
    }
    crate::commands::emit(
        json,
        json!({"provider": args.provider, "removed": true}),
        || format!("signed out provider {}", args.provider),
    )
}
