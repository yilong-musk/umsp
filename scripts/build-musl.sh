#!/usr/bin/env bash
# Build the static musl release locally and stage the release artifact.
set -euo pipefail

cd "$(dirname "$0")/.."
export PATH="$HOME/.cargo/bin:$PATH"
export CC_x86_64_unknown_linux_musl="${CC_x86_64_unknown_linux_musl:-musl-gcc}"
export AR_x86_64_unknown_linux_musl="${AR_x86_64_unknown_linux_musl:-ar}"
TARGET=x86_64-unknown-linux-musl

cargo fmt --all -- --check
cargo clippy --all-targets -- -D warnings
cargo test --locked
cargo build --release --locked --target "$TARGET"

VERSION="$(awk -F'"' '/^\[package\]/{p=1;next} p && /^version *=/{print $2; exit}' Cargo.toml)"
STAGE="dist/umsp-${VERSION}-${TARGET}"
rm -rf "$STAGE"
mkdir -p "$STAGE"
install -m 755 "target/${TARGET}/release/umsp" "$STAGE/umsp"
install -m 644 config.example.toml "$STAGE/config.example.toml"
install -m 644 deploy/umsp.service "$STAGE/umsp.service"
install -m 644 README.md "$STAGE/README.md"
tar -C dist -czf "dist/umsp-${VERSION}-${TARGET}.tar.gz" "umsp-${VERSION}-${TARGET}"
( cd dist && sha256sum "umsp-${VERSION}-${TARGET}.tar.gz" > "umsp-${VERSION}-${TARGET}.tar.gz.sha256" )

echo "binary:  $(stat -c%s "target/${TARGET}/release/umsp") bytes"
echo "archive: dist/umsp-${VERSION}-${TARGET}.tar.gz"
cat "dist/umsp-${VERSION}-${TARGET}.tar.gz.sha256"
