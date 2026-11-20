CARGO   ?= cargo
PREFIX  ?= $(HOME)/.local
BIN     := umsp
VERSION := $(shell git describe --tags --always 2>/dev/null || echo dev)
DIST    := dist/umsp-$(VERSION)-x86_64-unknown-linux-musl

.PHONY: all check fmt clippy test lang build install clean

all: check build

check: fmt clippy test lang

fmt:
	$(CARGO) fmt --all -- --check

clippy:
	$(CARGO) clippy --all-targets -- -D warnings

test:
	$(CARGO) test --locked

# Authored text must stay English; see AGENTS.md §1.
lang:
	$(CARGO) run --quiet --manifest-path tools/langcheck/Cargo.toml

build:
	./scripts/build-musl.sh

install: build
	install -Dm755 $(DIST)/$(BIN) $(PREFIX)/bin/$(BIN)

clean:
	$(CARGO) clean
	rm -rf dist
