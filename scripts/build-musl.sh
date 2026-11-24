#!/usr/bin/env bash
# Build the static musl release locally and stage the release artifact.
set -euo pipefail

cd "$(dirname "$0")/.."
export PATH="$HOME/.cargo/bin:$PATH"
export CC_x86_64_unknown_linux_musl="${CC_x86_64_unknown_linux_musl:-musl-gcc}"
export AR_x86_64_unknown_linux_musl="${AR_x86_64_unknown_linux_musl:-ar}"
TARGET=x86_64-unknown-linux-musl

if [[ "${SKIP_CHECKS:-0}" != "1" ]]; then
  cargo fmt --all -- --check
  cargo clippy --all-targets -- -D warnings
  cargo test --locked
fi
cargo build --release --locked --target "$TARGET"

size="$(wc -c < "target/${TARGET}/release/umsp" | tr -d ' ')"
echo "binary:  ${size} bytes"
./scripts/stage-release.sh "target/${TARGET}/release/umsp" "$TARGET"
