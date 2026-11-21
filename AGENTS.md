# AGENTS.md — repository constraints for coding agents

These rules apply to every change in this repository, whether it is made by a
human or by an automated agent. A pull request that breaks a rule is rejected,
not patched up during review.

## 1. English only

All text authored as part of this project must be English. "Authored text"
means everything a maintainer writes:

- identifiers, comments, log lines, error messages, test names and assertions
- CLI help, flags, exit-code descriptions, config keys and their defaults
- README, `docs/`, code comments, `deploy/` unit comments, `Makefile`, shell
  scripts and CI workflow files
- commit messages, branch names, pull-request titles and bodies, issue text

No other language may appear in those places, not even a single word or a
parenthetical translation. Mixed-language files are a violation.

### 1.1 What the rule does not cover

- **Upstream payloads.** Content fetched from an upstream API is data, not
  prose. Test fixtures that reproduce upstream responses must stay
  byte-faithful; mark such a line with `i18n-ignore`.
- **Runtime output.** Anything the gateway returns is derived from upstream or
  from configured values — never an inline literal in another language.
- **Third-party files.** `LICENSE` text and vendored assets keep their
  original wording.

### 1.2 Localized versions

A non-English version is allowed only as a separate, clearly named artifact:

| Kind | Path |
|---|---|
| README | `README.<tag>.md` (e.g. `README.zh-CN.md`) |
| Docs | `docs/<tag>/…` (e.g. `docs/zh-CN/API.md`) |

`<tag>` is a BCP 47 language tag. English is the source of truth; a localized
file is self-contained, is updated in the same pull request that changes the
English meaning, and never replaces the English entry point.

`make lang` (and CI) enforces the rule with `tools/langcheck`.

## 2. Build, test, release

- `make check` runs `fmt`, `clippy -D warnings`, `test` and `lang`; it is the
  gate for every change.
- `make build` produces a static musl binary and a release archive under
  `dist/` via `scripts/build-musl.sh`.
- `make install` copies the binary to `$(PREFIX)/bin/umsp` (default
  `~/.local/bin`).
- Version bumps are a single `Cargo.toml` change plus an annotated tag
  `v<version>`. Pushing that tag runs `.github/workflows/release.yml`: it
  publishes the musl archive and installs it on the configured host. The tag
  number must equal `version`, and the tagged commit must already be on `main`.

## 3. Secrets

- Never commit API keys, client keys, key hashes or tokens.
- Upstream keys live in `~/.config/umsp/env` (mode 0600); `config.toml` only
  names the variable via `api_key_env`.
- `.gitignore` excludes runtime material (`/config.toml`, `/env`, `*.key`,
  `*.tar.gz`, `/logs/`); keep it that way.

## 4. Commits and pull requests

- One logical change per commit; the subject is an imperative English
  sentence ("add vendor catalogue", not "added").
- Every commit must leave `make check` green.
- Pull requests state what changed, why, and how it was verified (commands and
  real output).
- Merging is a human decision: an agent may open a PR and push to a branch, but
  never merges without an explicit instruction from the repository owner given
  after CI finished for the current head commit.
