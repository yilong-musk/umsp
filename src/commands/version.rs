//! `umsp version`.

use serde_json::json;

use crate::error::Result;

pub fn run(json: bool) -> Result<i32> {
    let target = option_env!("UMSP_TARGET").unwrap_or("unknown");
    let profile = option_env!("UMSP_PROFILE").unwrap_or("unknown");
    let value = json!({
        "version": crate::server::VERSION,
        "target": target,
        "profile": profile,
        "os": std::env::consts::OS,
        "arch": std::env::consts::ARCH,
    });
    if json {
        crate::commands::emit_json(crate::server::success_json(value));
    } else {
        println!("umsp {} ({} {})", crate::server::VERSION, target, profile);
    }
    Ok(0)
}
