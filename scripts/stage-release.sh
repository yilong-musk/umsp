#!/usr/bin/env bash
# Pack an already-built umsp binary into the release archive.
# Usage: scripts/stage-release.sh <binary> [rust-target-triple]
set -euo pipefail

cd "$(dirname "$0")/.."

binary="${1:?usage: scripts/stage-release.sh <binary> [triple]}"
triple="${2:-x86_64-unknown-linux-musl}"
test -f "$binary"

version="$(awk -F'"' '/^\[package\]/{p=1; next} p && /^version *=/{print $2; exit}' Cargo.toml)"
test -n "$version"

name="umsp-${version}-${triple}"
stage="dist/${name}"
rm -rf "$stage"
mkdir -p "$stage"
install -m 755 "$binary" "${stage}/umsp"
install -m 644 config.example.toml "${stage}/config.example.toml"
install -m 644 deploy/umsp.service "${stage}/umsp.service"
install -m 644 README.md "${stage}/README.md"
tar -C dist -czf "dist/${name}.tar.gz" "${name}"
(
  cd dist
  if command -v sha256sum >/dev/null 2>&1; then
    sha256sum "${name}.tar.gz" > "${name}.tar.gz.sha256"
  else
    shasum -a 256 "${name}.tar.gz" > "${name}.tar.gz.sha256"
  fi
)

echo "archive: dist/${name}.tar.gz"
cat "dist/${name}.tar.gz.sha256"
