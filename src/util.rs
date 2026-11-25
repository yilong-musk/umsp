//! Small utilities: time, ids, keys, hex, durations, globs.

use crate::error::{Result, UmspError};
use chrono::{DateTime, Local, SecondsFormat, TimeDelta, Utc};

pub fn now_rfc3339() -> String {
    Local::now().to_rfc3339_opts(SecondsFormat::Millis, true)
}

pub fn now_utc() -> DateTime<Utc> {
    Utc::now()
}

/// ULID-shaped request id: 26 chars, Crockford base32 (48-bit ms timestamp + 80 random bits).
pub fn new_request_id() -> String {
    let mut buf = [0u8; 16];
    let millis = Utc::now().timestamp_millis() as u64;
    buf[..6].copy_from_slice(&millis.to_be_bytes()[2..]);
    if getrandom::fill(&mut buf[6..]).is_err() {
        // Extremely unlikely; fall back to a counter-ish value rather than panicking.
        for (i, b) in buf[6..].iter_mut().enumerate() {
            *b = (i as u8).wrapping_mul(37).wrapping_add(11);
        }
    }
    encode_ulid(&buf)
}

fn encode_ulid(bytes: &[u8; 16]) -> String {
    const ALPHABET: &[u8; 32] = b"0123456789ABCDEFGHJKMNPQRSTVWXYZ";
    let mut out = String::with_capacity(26);
    // 128 bits -> 26 base32 chars (first char carries 2 bits).
    let mut acc: u32 = 0;
    let mut bits: u32 = 0;
    for &b in bytes.iter() {
        acc = (acc << 8) | b as u32;
        bits += 8;
        while bits >= 5 {
            let idx = ((acc >> (bits - 5)) & 0x1f) as usize;
            out.push(ALPHABET[idx] as char);
            bits -= 5;
        }
    }
    if bits > 0 {
        let idx = ((acc << (5 - bits)) & 0x1f) as usize;
        out.push(ALPHABET[idx] as char);
    }
    out
}

pub fn hex_encode(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for &b in bytes {
        out.push(HEX[(b >> 4) as usize] as char);
        out.push(HEX[(b & 0x0f) as usize] as char);
    }
    out
}

pub fn hex_decode(s: &str) -> Option<Vec<u8>> {
    if s.len() % 2 != 0 {
        return None;
    }
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(s.len() / 2);
    for pair in bytes.chunks(2) {
        let hi = hex_val(pair[0])?;
        let lo = hex_val(pair[1])?;
        out.push((hi << 4) | lo);
    }
    Some(out)
}

fn hex_val(c: u8) -> Option<u8> {
    match c {
        b'0'..=b'9' => Some(c - b'0'),
        b'a'..=b'f' => Some(c - b'a' + 10),
        b'A'..=b'F' => Some(c - b'A' + 10),
        _ => None,
    }
}

/// Parse a `--since` value: `24h`, `90m`, `7d`, `30d`, or an RFC3339 timestamp.
pub fn parse_since(raw: &str) -> Result<DateTime<Utc>> {
    let s = raw.trim();
    if s.is_empty() {
        return Err(UmspError::usage(
            "--since requires a value like 24h, 7d or 2026-11-01T00:00:00Z",
        ));
    }
    let (num, unit) = s.split_at(s.len() - 1);
    let amount: i64 = match num.parse() {
        Ok(v) => v,
        Err(_) => {
            return DateTime::parse_from_rfc3339(s)
                .map(|dt| dt.with_timezone(&Utc))
                .map_err(|_| UmspError::usage(format!("invalid --since value: {s}")));
        }
    };
    let delta = match unit {
        "s" => TimeDelta::seconds(amount),
        "m" => TimeDelta::minutes(amount),
        "h" => TimeDelta::hours(amount),
        "d" => TimeDelta::days(amount),
        "w" => TimeDelta::weeks(amount),
        other => {
            return Err(UmspError::usage(format!(
                "invalid --since unit '{other}' (use s/m/h/d/w or an RFC3339 timestamp)"
            )));
        }
    };
    Ok(Utc::now() - delta)
}

/// Parse a positive relative duration: `90m`, `24h`, `30d`, `2w` (also `s`).
pub fn parse_duration(raw: &str) -> Result<TimeDelta> {
    let s = raw.trim();
    let invalid = || {
        UmspError::usage(format!(
            "invalid duration '{s}' (use a positive number followed by s/m/h/d/w, e.g. 30d)"
        ))
    };
    let Some((split, _)) = s.char_indices().last() else {
        return Err(invalid());
    };
    let (num, unit) = s.split_at(split);
    let amount: i64 = num.parse().map_err(|_| invalid())?;
    if amount <= 0 {
        return Err(invalid());
    }
    let delta = match unit {
        "s" => TimeDelta::try_seconds(amount),
        "m" => TimeDelta::try_minutes(amount),
        "h" => TimeDelta::try_hours(amount),
        "d" => TimeDelta::try_days(amount),
        "w" => TimeDelta::try_weeks(amount),
        _ => None,
    };
    delta.ok_or_else(invalid)
}

/// Absolute expiry for a `--expires-in` value, as a whole-second UTC RFC3339
/// string (the form stored in `config.toml`).
pub fn expiry_from_now(raw: &str) -> Result<String> {
    let delta = parse_duration(raw)?;
    let at = Utc::now()
        .checked_add_signed(delta)
        .ok_or_else(|| UmspError::usage(format!("duration '{}' is too large", raw.trim())))?;
    Ok(at.to_rfc3339_opts(SecondsFormat::Secs, true))
}

/// Minimal glob: `*` matches any run of characters (including `/`), `?` matches one char.
pub fn glob_match(pattern: &str, text: &str) -> bool {
    let p: Vec<char> = pattern.chars().collect();
    let t: Vec<char> = text.chars().collect();
    let (mut pi, mut ti) = (0usize, 0usize);
    let (mut star, mut mark) = (None::<usize>, 0usize);
    while ti < t.len() {
        if pi < p.len() && (p[pi] == '?' || p[pi] == t[ti]) {
            pi += 1;
            ti += 1;
        } else if pi < p.len() && p[pi] == '*' {
            star = Some(pi);
            mark = ti;
            pi += 1;
        } else if let Some(s) = star {
            pi = s + 1;
            mark += 1;
            ti = mark;
        } else {
            return false;
        }
    }
    while pi < p.len() && p[pi] == '*' {
        pi += 1;
    }
    pi == p.len()
}

/// True when at least one allowed pattern matches (empty list means "allow all").
pub fn any_glob_match(patterns: &[String], text: &str) -> bool {
    if patterns.is_empty() {
        return true;
    }
    patterns.iter().any(|p| glob_match(p, text))
}

pub fn parse_list(raw: &str) -> Vec<String> {
    raw.split(',')
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect()
}

pub fn home_dir() -> std::path::PathBuf {
    std::env::var_os("HOME")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| std::path::PathBuf::from("/"))
}

/// Expand a leading `~` in a path string.
pub fn expand_tilde(value: &str) -> std::path::PathBuf {
    if value == "~" {
        return home_dir();
    }
    if let Some(rest) = value.strip_prefix("~/") {
        return home_dir().join(rest);
    }
    std::path::PathBuf::from(value)
}

/// Atomically replace `path` with `bytes`.
///
/// The temporary file is created next to `path` under a random name with
/// `create_new` and `mode` from the start, so secrets are never readable by
/// other users, concurrent writers never share a temp file, and a crash leaves
/// either the old or the new content. Data and directory are fsynced so the
/// rename survives power loss.
pub fn atomic_write(path: &std::path::Path, bytes: &[u8], mode: u32) -> std::io::Result<()> {
    use std::io::{Error, ErrorKind, Write};
    use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};

    let dir = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| std::path::Path::new("."));
    std::fs::create_dir_all(dir)?;
    let name = path
        .file_name()
        .ok_or_else(|| Error::new(ErrorKind::InvalidInput, "path has no file name"))?
        .to_string_lossy();

    for _ in 0..16 {
        let mut suffix = [0u8; 8];
        if getrandom::fill(&mut suffix).is_err() {
            let nanos = Utc::now().timestamp_nanos_opt().unwrap_or_default() as u64;
            suffix = (nanos ^ u64::from(std::process::id())).to_le_bytes();
        }
        let tmp = dir.join(format!(".{name}.{}.tmp", hex_encode(&suffix)));
        let mut file = match std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(mode)
            .open(&tmp)
        {
            Ok(file) => file,
            Err(err) if err.kind() == ErrorKind::AlreadyExists => continue,
            Err(err) => return Err(err),
        };
        let written = file
            .write_all(bytes)
            // `mode` passed to open() is filtered by the umask; make it exact.
            .and_then(|()| file.set_permissions(std::fs::Permissions::from_mode(mode)))
            .and_then(|()| file.sync_all())
            .and_then(|()| std::fs::rename(&tmp, path));
        if let Err(err) = written {
            let _ = std::fs::remove_file(&tmp);
            return Err(err);
        }
        if let Ok(dir) = std::fs::File::open(dir) {
            let _ = dir.sync_all();
        }
        return Ok(());
    }
    Err(Error::new(
        ErrorKind::AlreadyExists,
        "could not pick a unique temporary file name",
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn atomic_dir(tag: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("umsp-atomic-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn mode_of(path: &std::path::Path) -> u32 {
        use std::os::unix::fs::PermissionsExt;
        std::fs::metadata(path).unwrap().permissions().mode() & 0o777
    }

    #[test]
    fn atomic_write_replaces_content_with_the_requested_mode() {
        let dir = atomic_dir("mode");
        let path = dir.join("env");
        std::fs::write(&path, "OLD=1\n").unwrap();
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
        }
        atomic_write(&path, b"NEW=2\n", 0o600).unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "NEW=2\n");
        assert_eq!(mode_of(&path), 0o600);
        let leftovers: Vec<_> = std::fs::read_dir(&dir)
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .filter(|name| name != "env")
            .collect();
        assert!(leftovers.is_empty(), "temp files left: {leftovers:?}");
    }

    #[test]
    fn atomic_write_creates_missing_parents() {
        let dir = atomic_dir("parents");
        let path = dir.join("a/b/state.json");
        atomic_write(&path, b"{}", 0o600).unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "{}");
    }

    #[test]
    fn concurrent_atomic_writers_never_corrupt_the_file() {
        let dir = atomic_dir("concurrent");
        let path = dir.join("session.json");
        let payloads: Vec<String> = (0..8).map(|i| format!("{i}").repeat(4096)).collect();
        std::thread::scope(|scope| {
            for payload in &payloads {
                let path = &path;
                scope.spawn(move || {
                    for _ in 0..20 {
                        atomic_write(path, payload.as_bytes(), 0o600).unwrap();
                    }
                });
            }
        });
        let final_text = std::fs::read_to_string(&path).unwrap();
        assert!(payloads.contains(&final_text), "torn write");
        assert_eq!(std::fs::read_dir(&dir).unwrap().count(), 1);
    }

    #[test]
    fn glob_basics() {
        assert!(glob_match("od/*", "od/deepseek-v4.1-flash"));
        assert!(!glob_match("od/*", "deepseek/deepseek-flash"));
        assert!(glob_match("*", "anything/at/all"));
        assert!(glob_match("deepseek-*", "deepseek-flash"));
        assert!(glob_match("*-flash", "deepseek-v4.1-flash"));
        assert!(glob_match(
            "od/deepseek-v4.1-flash",
            "od/deepseek-v4.1-flash"
        ));
        assert!(!glob_match("od/?", "od/ab"));
        assert!(glob_match("od/?", "od/a"));
    }

    #[test]
    fn allowed_list_semantics() {
        let none: Vec<String> = Vec::new();
        assert!(any_glob_match(&none, "x"));
        let pats = vec!["od/*".to_string(), "!bad".to_string()];
        assert!(any_glob_match(&pats, "od/x"));
        assert!(!any_glob_match(&pats, "ds/x"));
    }

    #[test]
    fn since_parsing() {
        assert!(parse_since("24h").is_ok());
        assert!(parse_since("7d").is_ok());
        assert!(parse_since("2026-11-01T00:00:00Z").is_ok());
        assert!(parse_since("nope").is_err());
        assert!(parse_since("12x").is_err());
    }

    #[test]
    fn duration_parsing() {
        assert_eq!(parse_duration("30d").unwrap(), TimeDelta::days(30));
        assert_eq!(parse_duration(" 12h ").unwrap(), TimeDelta::hours(12));
        assert_eq!(parse_duration("2w").unwrap(), TimeDelta::weeks(2));
        assert_eq!(parse_duration("90m").unwrap(), TimeDelta::minutes(90));
        // A multi-byte trailing char must be rejected, not split mid-codepoint.
        for bad in ["", "d", "0d", "-1d", "12x", "1.5d", "30", "3\u{00e9}"] {
            assert!(parse_duration(bad).is_err(), "{bad:?} should be rejected");
        }
        assert!(parse_duration("999999999999w").is_err());
    }

    #[test]
    fn expiry_is_whole_seconds_utc_in_the_future() {
        let raw = expiry_from_now("1h").unwrap();
        assert!(raw.ends_with('Z'), "{raw}");
        assert!(!raw.contains('.'), "{raw}");
        let at = DateTime::parse_from_rfc3339(&raw).unwrap();
        let ahead = at.with_timezone(&Utc) - Utc::now();
        assert!(ahead > TimeDelta::minutes(59) && ahead <= TimeDelta::hours(1));
    }

    #[test]
    fn hex_roundtrip() {
        let raw = [0u8, 1, 15, 16, 255];
        let enc = hex_encode(&raw);
        assert_eq!(enc, "00010f10ff");
        assert_eq!(hex_decode(&enc).unwrap(), raw);
        assert!(hex_decode("abc").is_none());
    }

    #[test]
    fn request_id_shape() {
        let id = new_request_id();
        assert_eq!(id.len(), 26);
        assert!(
            id.chars()
                .all(|c| "0123456789ABCDEFGHJKMNPQRSTVWXYZ".contains(c))
        );
    }
}
