//! `~/.config/umsp/env` handling: the CLI reads it directly, systemd loads it as
//! an EnvironmentFile. Process environment wins over the file so systemd and
//! shell overrides both behave as expected.

use std::collections::BTreeMap;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use crate::config::ENV_TEMPLATE;
use crate::error::{Result, UmspError};

#[derive(Debug, Clone, Default)]
pub struct EnvFile {
    pub path: PathBuf,
    vars: BTreeMap<String, String>,
}

impl EnvFile {
    pub fn load(path: &Path) -> Self {
        let mut vars = BTreeMap::new();
        if let Ok(raw) = std::fs::read_to_string(path) {
            for line in raw.lines() {
                let line = line.trim();
                if line.is_empty() || line.starts_with('#') {
                    continue;
                }
                let line = line.strip_prefix("export ").unwrap_or(line);
                if let Some((k, v)) = line.split_once('=') {
                    let key = k.trim().to_string();
                    let mut value = v.trim().to_string();
                    if value.len() >= 2
                        && ((value.starts_with('"') && value.ends_with('"'))
                            || (value.starts_with('\'') && value.ends_with('\'')))
                    {
                        value = value[1..value.len() - 1].to_string();
                    }
                    if !key.is_empty() {
                        vars.insert(key, value);
                    }
                }
            }
        }
        Self {
            path: path.to_path_buf(),
            vars,
        }
    }

    /// Look up a secret: process env first, then the env file.
    pub fn get(&self, key: &str) -> Option<String> {
        if let Ok(v) = std::env::var(key) {
            if !v.trim().is_empty() {
                return Some(v);
            }
        }
        self.vars.get(key).cloned().filter(|v| !v.trim().is_empty())
    }

    pub fn has(&self, key: &str) -> bool {
        self.get(key).is_some()
    }

    /// Variable names present in the file, sorted.
    pub fn names(&self) -> Vec<&String> {
        self.vars.keys().collect()
    }

    /// Create the env file with a comment-only template when missing. Returns true if created.
    pub fn ensure_file(path: &Path) -> Result<bool> {
        if path.exists() {
            return Ok(false);
        }
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| {
                UmspError::other(
                    "ENV_FILE",
                    format!("cannot create {}: {e}", parent.display()),
                )
            })?;
        }
        std::fs::write(path, ENV_TEMPLATE).map_err(|e| {
            UmspError::other("ENV_FILE", format!("cannot write {}: {e}", path.display()))
        })?;
        set_mode_600(path)?;
        Ok(true)
    }

    /// Insert or replace a key in the env file, atomically and with mode 0600.
    pub fn set(path: &Path, key: &str, value: &str) -> Result<()> {
        let existing = std::fs::read_to_string(path).unwrap_or_default();
        let mut out = String::new();
        let mut replaced = false;
        for line in existing.lines() {
            let trimmed = line.trim_start();
            let candidate = trimmed.strip_prefix("export ").unwrap_or(trimmed);
            if let Some((k, _)) = candidate.split_once('=') {
                if k.trim() == key {
                    out.push_str(&format!("{key}={value}\n"));
                    replaced = true;
                    continue;
                }
            }
            // Drop a commented placeholder for the same key.
            if let Some(rest) = trimmed.strip_prefix('#') {
                if let Some((k, _)) = rest.split_once('=') {
                    if k.trim() == key {
                        continue;
                    }
                }
            }
            out.push_str(line);
            out.push('\n');
        }
        if !replaced {
            if !out.ends_with('\n') && !out.is_empty() {
                out.push('\n');
            }
            out.push_str(&format!("{key}={value}\n"));
        }
        write_private(path, &out)
    }

    /// Remove every line (and commented placeholder) for `key`.
    /// Returns whether anything was removed.
    pub fn unset(path: &Path, key: &str) -> Result<bool> {
        let existing = std::fs::read_to_string(path).unwrap_or_default();
        let mut out = String::new();
        let mut removed = false;
        for line in existing.lines() {
            let trimmed = line.trim_start();
            let candidate = trimmed.strip_prefix("export ").unwrap_or(trimmed);
            if !candidate.starts_with('#') {
                if let Some((k, _)) = candidate.split_once('=') {
                    if k.trim() == key {
                        removed = true;
                        continue;
                    }
                }
            } else if let Some(rest) = candidate.strip_prefix('#') {
                if let Some((k, _)) = rest.split_once('=') {
                    if k.trim() == key {
                        removed = true;
                        continue;
                    }
                }
            }
            out.push_str(line);
            out.push('\n');
        }
        if removed {
            write_private(path, &out)?;
        }
        Ok(removed)
    }
}

pub fn write_private(path: &Path, contents: &str) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| {
            UmspError::other(
                "ENV_FILE",
                format!("cannot create {}: {e}", parent.display()),
            )
        })?;
    }
    let tmp = path.with_extension("tmp");
    std::fs::write(&tmp, contents).map_err(|e| {
        UmspError::other("ENV_FILE", format!("cannot write {}: {e}", tmp.display()))
    })?;
    set_mode_600(&tmp)?;
    std::fs::rename(&tmp, path).map_err(|e| {
        UmspError::other(
            "ENV_FILE",
            format!("cannot replace {}: {e}", path.display()),
        )
    })?;
    Ok(())
}

pub fn set_mode_600(path: &Path) -> Result<()> {
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600)).map_err(|e| {
        UmspError::other(
            "ENV_FILE",
            format!("cannot chmod 0600 {}: {e}", path.display()),
        )
    })
}

pub fn file_mode(path: &Path) -> Option<u32> {
    std::fs::metadata(path)
        .ok()
        .map(|m| m.permissions().mode() & 0o777)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    #[test]
    fn unset_removes_the_value_and_its_placeholder() {
        let dir = std::env::temp_dir().join(format!("umsp-env-unset-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("env");
        std::fs::write(&path, "# header\n#KEY=old\nKEY=new\nOTHER=2\n").unwrap();
        assert!(EnvFile::unset(&path, "KEY").unwrap());
        let env = EnvFile::load(&path);
        assert!(env.get("KEY").is_none());
        assert_eq!(env.get("OTHER").as_deref(), Some("2"));
        assert!(!EnvFile::unset(&path, "KEY").unwrap());
    }

    #[test]
    fn parses_env_file() {
        let dir = std::env::temp_dir().join(format!("umsp-env-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("env");
        let mut f = std::fs::File::create(&path).unwrap();
        writeln!(f, "# comment\nA=1\n export B = \"two\"\nC='three'\nEMPTY=").unwrap();
        let env = EnvFile::load(&path);
        assert_eq!(env.get("A").as_deref(), Some("1"));
        assert_eq!(env.get("B").as_deref(), Some("two"));
        assert_eq!(env.get("C").as_deref(), Some("three"));
        assert!(env.get("EMPTY").is_none());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn set_replaces_and_creates() {
        let dir = std::env::temp_dir().join(format!("umsp-env-set-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("env");
        std::fs::write(&path, "#K=old\nOTHER=x\n").unwrap();
        EnvFile::set(&path, "K", "new").unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.contains("K=new"));
        assert!(!text.contains("#K=old"));
        assert_eq!(file_mode(&path), Some(0o600));
        std::fs::remove_dir_all(&dir).ok();
    }
}
