//! `umsp env` — manage upstream API keys without echoing them.
//!
//! Values live in `~/.config/umsp/env` (0600); `config.toml` only names the
//! variable via `api_key_env`. Nothing here ever prints a full secret.

use std::io::{self, BufRead, IsTerminal, Write};
use std::path::Path;

use serde_json::json;

use crate::cli::EnvCmd;
use crate::config::Config;
use crate::envfile::EnvFile;
use crate::error::{Result, UmspError};

pub fn run(config_path: &Path, cmd: EnvCmd, json: bool, no_reload: bool) -> Result<i32> {
    let path = Config::resolve_env_path();
    match cmd {
        EnvCmd::List => list(&path, json),
        EnvCmd::Set { name, stdin } => set(config_path, &path, &name, stdin, json, no_reload),
        EnvCmd::Unset { name } => unset(config_path, &path, &name, json, no_reload),
    }
}

fn maybe_reload(config_path: &Path, no_reload: bool) {
    if no_reload {
        return;
    }
    if let Ok(cfg) = Config::load(config_path) {
        crate::server::reload_running_server(&cfg);
    }
}

/// `sk-abcd…yz` — enough to recognise a key, not enough to use it.
pub fn mask(value: &str) -> String {
    let chars: Vec<char> = value.chars().collect();
    if chars.len() <= 8 {
        return "****".to_string();
    }
    let head: String = chars.iter().take(5).collect();
    let tail: String = chars
        .iter()
        .rev()
        .take(2)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect();
    format!("{head}…{tail}")
}

fn valid_name(name: &str) -> bool {
    let mut chars = name.chars();
    match chars.next() {
        Some(c) if c.is_ascii_alphabetic() || c == '_' => {}
        _ => return false,
    }
    chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
}

fn list(path: &Path, json: bool) -> Result<i32> {
    let env = EnvFile::load(path);
    let mut rows = Vec::new();
    for name in env.names() {
        let value = env.get(name).unwrap_or_default();
        let source = if std::env::var(name)
            .ok()
            .is_some_and(|v| !v.trim().is_empty())
        {
            "process"
        } else {
            "file"
        };
        rows.push(json!({
            "name": name,
            "masked": mask(&value),
            "source": source,
        }));
    }
    let value = json!({
        "env_file": path.display().to_string(),
        "exists": path.exists(),
        "vars": rows,
    });
    crate::commands::emit(json, value, || {
        let mut out = format!("{}\n", path.display());
        for row in &rows {
            out.push_str(&format!(
                "  {} = {} ({})\n",
                row["name"].as_str().unwrap_or(""),
                row["masked"].as_str().unwrap_or(""),
                row["source"].as_str().unwrap_or("")
            ));
        }
        if rows.is_empty() {
            out.push_str("  (no variables)\n");
        }
        out
    })
}

fn set(
    config_path: &Path,
    path: &Path,
    name: &str,
    from_stdin: bool,
    json: bool,
    no_reload: bool,
) -> Result<i32> {
    if !valid_name(name) {
        return Err(UmspError::usage(format!(
            "invalid variable name '{name}' (use letters, digits and _)"
        )));
    }
    let value = if from_stdin || !io::stdin().is_terminal() {
        read_line_stdin()?
    } else {
        read_secret(&format!("{name}: "))?
    };
    let value = value.trim().to_string();
    if value.is_empty() {
        return Err(UmspError::usage("the value must not be empty"));
    }
    EnvFile::set(path, name, &value)?;
    maybe_reload(config_path, no_reload);
    let masked = mask(&value);
    crate::commands::emit(
        json,
        json!({"name": name, "masked": masked, "env_file": path.display().to_string()}),
        || format!("set {name} = {masked} in {}", path.display()),
    )
}

fn unset(config_path: &Path, path: &Path, name: &str, json: bool, no_reload: bool) -> Result<i32> {
    if !EnvFile::unset(path, name)? {
        return Err(UmspError::not_found(format!(
            "{name} is not set in {}",
            path.display()
        )));
    }
    maybe_reload(config_path, no_reload);
    crate::commands::emit(json, json!({"name": name, "removed": true}), || {
        format!("removed {name} from {}", path.display())
    })
}

fn read_line_stdin() -> Result<String> {
    let mut line = String::new();
    io::stdin()
        .lock()
        .read_line(&mut line)
        .map_err(|e| UmspError::other("ENV_READ", format!("cannot read stdin: {e}")))?;
    Ok(line)
}

/// Read one secret. On a TTY the prompt is shown and echo is disabled; when
/// stdin is a pipe the line is read directly, so scripts can do
/// `printf '%s\n' "$KEY" | umsp env set NAME`.
pub fn read_secret(prompt: &str) -> Result<String> {
    if !io::stdin().is_terminal() {
        return read_line_stdin();
    }
    let stdin = io::stdin();
    #[cfg(unix)]
    {
        use std::os::fd::AsRawFd;
        let fd = stdin.as_raw_fd();
        let mut term = std::mem::MaybeUninit::<libc::termios>::uninit();
        // If termios is unavailable, fall back to a plain (echoing) read.
        if unsafe { libc::tcgetattr(fd, term.as_mut_ptr()) } != 0 {
            return read_line_stdin();
        }
        let original = unsafe { term.assume_init() };
        let mut hidden = original;
        hidden.c_lflag &= !libc::ECHO;
        unsafe {
            libc::tcsetattr(fd, libc::TCSANOW, &hidden);
        }
        eprint!("{prompt}");
        let _ = io::stderr().flush();
        let result = read_line_stdin();
        unsafe {
            libc::tcsetattr(fd, libc::TCSANOW, &original);
        }
        eprintln!();
        result
    }
    #[cfg(not(unix))]
    {
        eprint!("{prompt}");
        let _ = io::stderr().flush();
        read_line_stdin()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn masks_without_revealing_the_key() {
        assert_eq!(mask("sk-abcdefghijklmnop"), "sk-ab…op");
        assert_eq!(mask("short"), "****");
        assert_eq!(mask(""), "****");
    }

    #[test]
    fn names_are_conservative() {
        for ok in ["DEEPSEEK_API_KEY", "A", "_X1"] {
            assert!(valid_name(ok), "{ok} should be valid");
        }
        for bad in ["", "1KEY", "A-B", "A B", "K="] {
            assert!(!valid_name(bad), "{bad} should be invalid");
        }
    }
}
