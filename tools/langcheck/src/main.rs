//! Fails when authored text contains non-Latin scripts.
//!
//! AGENTS.md requires every authored string in this repository to be English.
//! This check walks `git ls-files`, skips binaries, the licence text and the
//! localized artifacts allowed by the policy (`README.<tag>.md`,
//! `docs/<tag>/…`), and reports characters from Cyrillic, Hebrew, Arabic,
//! Devanagari, Thai, Kana, CJK and Hangul ranges.
//!
//! A line carrying the marker `i18n-ignore` is skipped; use it only for data
//! that must stay byte-faithful (upstream payload fixtures).
//!
//! Run it with `make lang`; CI runs the same command.

use std::process::ExitCode;

struct ScriptRange {
    lo: char,
    hi: char,
    name: &'static str,
}

const RANGES: &[ScriptRange] = &[
    ScriptRange {
        lo: '\u{0400}',
        hi: '\u{04FF}',
        name: "Cyrillic",
    },
    ScriptRange {
        lo: '\u{0590}',
        hi: '\u{05FF}',
        name: "Hebrew",
    },
    ScriptRange {
        lo: '\u{0600}',
        hi: '\u{06FF}',
        name: "Arabic",
    },
    ScriptRange {
        lo: '\u{0900}',
        hi: '\u{097F}',
        name: "Devanagari",
    },
    ScriptRange {
        lo: '\u{0E00}',
        hi: '\u{0E7F}',
        name: "Thai",
    },
    ScriptRange {
        lo: '\u{3040}',
        hi: '\u{30FF}',
        name: "Kana",
    },
    ScriptRange {
        lo: '\u{3400}',
        hi: '\u{4DBF}',
        name: "CJK",
    },
    ScriptRange {
        lo: '\u{4E00}',
        hi: '\u{9FFF}',
        name: "CJK",
    },
    ScriptRange {
        lo: '\u{F900}',
        hi: '\u{FAFF}',
        name: "CJK",
    },
    ScriptRange {
        lo: '\u{AC00}',
        hi: '\u{D7AF}',
        name: "Hangul",
    },
];

const SKIP_FILES: &[&str] = &["LICENSE"];
const SKIP_SUFFIXES: &[&str] = &[".png", ".jpg", ".jpeg", ".gif", ".webp", ".ico", ".pdf"];

fn main() -> ExitCode {
    let output = match std::process::Command::new("git")
        .args(["ls-files", "-z"])
        .output()
    {
        Ok(output) => output,
        Err(err) => {
            eprintln!("langcheck: cannot run git: {err}");
            return ExitCode::from(2);
        }
    };
    if !output.status.success() {
        eprintln!("langcheck: git ls-files failed");
        return ExitCode::from(2);
    }

    let mut violations = 0usize;
    for raw in output.stdout.split(|byte| *byte == 0) {
        if raw.is_empty() {
            continue;
        }
        let path = String::from_utf8_lossy(raw);
        if skip(&path) {
            continue;
        }
        let Ok(bytes) = std::fs::read(path.as_ref()) else {
            continue;
        };
        if bytes.contains(&0) {
            continue; // binary file
        }
        let text = String::from_utf8_lossy(&bytes);
        for (index, line) in text.lines().enumerate() {
            if line.contains("i18n-ignore") {
                continue;
            }
            if let Some((ch, name)) = first_foreign(line) {
                println!("{}:{}: {} character '{}'", path, index + 1, name, ch);
                violations += 1;
            }
        }
    }

    if violations > 0 {
        eprintln!(
            "langcheck: {violations} line(s) contain non-English text; see AGENTS.md §1 (English only)"
        );
        return ExitCode::FAILURE;
    }
    println!("langcheck: ok (English-only policy holds)");
    ExitCode::SUCCESS
}

fn first_foreign(line: &str) -> Option<(char, &'static str)> {
    for ch in line.chars() {
        for range in RANGES {
            if ch >= range.lo && ch <= range.hi {
                return Some((ch, range.name));
            }
        }
    }
    None
}

fn skip(path: &str) -> bool {
    if SKIP_FILES.contains(&path) {
        return true;
    }
    if SKIP_SUFFIXES.iter().any(|suffix| path.ends_with(suffix)) {
        return true;
    }
    // Allowed localized artifacts: README.<tag>.md and docs/<tag>/…
    if path
        .rsplit('/')
        .next()
        .is_some_and(|name| name.starts_with("README.") && name.ends_with(".md"))
    {
        return true;
    }
    if let Some(rest) = path.strip_prefix("docs/") {
        let segment = rest.split('/').next().unwrap_or("");
        if segment != rest && is_language_tag(segment) {
            return true;
        }
    }
    false
}

fn is_language_tag(segment: &str) -> bool {
    let mut parts = segment.split('-');
    let Some(language) = parts.next() else {
        return false;
    };
    if !(2..=3).contains(&language.len()) || !language.chars().all(|c| c.is_ascii_lowercase()) {
        return false;
    }
    parts.all(|part| {
        (2..=8).contains(&part.len()) && part.chars().all(|c| c.is_ascii_alphanumeric())
    })
}
