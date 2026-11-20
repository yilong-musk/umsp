# Security policy

## Supported versions

The latest release on `main` is supported. UMSP is pre-1.0: fixes land on
`main` and in a patch release; older tags are not maintained.

## Reporting a vulnerability

Use GitHub's private vulnerability reporting:

<https://github.com/yilong-musk/umsp/security/advisories/new>

Please do not open a public issue for a suspected vulnerability. Include the
version (`umsp version`), a minimal reproduction, and the impact you observed.
You can expect an initial response within seven days.

## Scope notes

- UMSP listens on `127.0.0.1` by default and keeps upstream API keys in
  `~/.config/umsp/env` (mode 0600). A key leaking through a log line, a ledger
  row or an HTTP response is in scope; a compromised host is not.
- Client keys are stored as SHA-256 hashes; the plaintext is printed once and
  never again.
- Provider identity is internal: response headers and error bodies must not
  reveal which upstream served a request. A leak there is a bug worth
  reporting.
