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

#[cfg(test)]
mod tests {
    use super::*;

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
