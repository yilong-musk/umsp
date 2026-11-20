# Contributing

Thanks for taking the time to contribute.

## Before you start

- Read [`AGENTS.md`](AGENTS.md). All authored text must be English;
  `README.zh-CN.md` is the only localized file in the repository.
- Rust 1.85 or newer (edition 2024).
- For a behaviour change, open an issue first so the design can be discussed
  before code is written.

## Development

```bash
make check     # fmt + clippy -D warnings + tests + language check
make build     # static musl binary and release archive under dist/
```

`make check` must be green before a pull request is opened.

## Tests

Add tests for new behaviour:

- unit tests next to the code (`#[cfg(test)]` in the same module);
- integration tests that drive the real router in `tests/integration.rs`;
- CLI end-to-end tests in `tests/cli.rs` (they run the built binary in a
  sandbox with `UMSP_CONFIG` / `UMSP_ENV_FILE`).

A bug fix should include a test that fails before the fix and passes after it.

## Pull requests

- One logical change per pull request; describe the motivation and how you
  verified it (commands and real output).
- Update `README.md` and the relevant `docs/` page in the same pull request.
  Keep `README.zh-CN.md` in sync when the English meaning changes.
- Never include secrets, API keys or key hashes.
