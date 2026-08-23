default:
    @just --list

# Format all code
fmt:
    cargo fmt --all

# Run clippy auto-fix on a specific crate
fix crate="":
    @if [ -z "{{crate}}" ]; then \
        cargo clippy --fix --all-features --tests --allow-dirty; \
    else \
        cargo clippy --fix --all-features --tests --allow-dirty -p {{crate}}; \
    fi

# Run lib + bin tests (fast feedback loop, mirrors CLAUDE.md). Uses nextest when available.
test:
    @if command -v cargo-nextest >/dev/null 2>&1; then \
        cargo nextest run --workspace --lib --bins 2>&1 | tee /tmp/borg-test.log; \
    else \
        cargo test --workspace --lib --bins 2>&1 | tee /tmp/borg-test.log; \
    fi

# Run the full workspace test suite (integration tests included). Use before shipping.
# `test-support` enables the gated fixture binary for codex_app_server integration tests.
test-all:
    cargo test --workspace --features cortex/test-support 2>&1 | tee /tmp/borg-test-all.log

# Run tests for a single crate. Example: just test-crate cortex
test-crate crate:
    cargo test -p {{crate}} --lib 2>&1 | tee /tmp/borg-test-{{crate}}.log

# Compile-check the workspace (fast; no codegen). Optional crate filter.
check crate="":
    @if [ -z "{{crate}}" ]; then \
        cargo check --workspace --all-targets; \
    else \
        cargo check -p {{crate}} --all-targets; \
    fi

# Watch source files and re-run a cargo subcommand on save. Default: check.
# Examples:
#   just watch
#   just watch "check -p cortex"
#   just watch "test --workspace --lib --bins"
watch cmd="check":
    cargo watch -c -x "{{cmd}}"

# Build debug
build:
    cargo build

# Build release
release:
    cargo build --release

# Run format check + clippy (what pre-commit does)
lint:
    cargo fmt --check
    cargo clippy -- -D warnings

# Install the repo pre-commit hook (one-time setup)
install-hooks:
    git config core.hooksPath .githooks
    @echo "pre-commit hook active (.githooks/pre-commit). Bypass with --no-verify."

# Install local CI helpers (cargo-audit, cargo-deny, cargo-nextest, cargo-watch)
install-ci-tools:
    RUSTUP_TOOLCHAIN=stable cargo install cargo-audit --locked
    RUSTUP_TOOLCHAIN=stable cargo install cargo-deny --locked
    RUSTUP_TOOLCHAIN=stable cargo install cargo-nextest --locked
    RUSTUP_TOOLCHAIN=stable cargo install cargo-watch --locked

# Security advisory scan (mirrors CI's Security Audit job)
audit:
    RUSTUP_TOOLCHAIN=stable cargo audit

# Dependency check (mirrors CI's Dependency Check job)
deny:
    cargo deny --all-features check

# Run everything CI runs (format, clippy, test, build, audit, deny)
ci: lint
    cargo test --workspace --features cortex/test-support
    cargo build
    just audit
    just deny

# Generate code coverage report (HTML)
coverage:
    cargo llvm-cov --workspace --html
    @echo "Report: target/llvm-cov/html/index.html"

# Generate code coverage summary (text)
coverage-summary:
    cargo llvm-cov --workspace

# Cyclomatic complexity report (functions with CCN > 15; needs uv)
complexity:
    uvx lizard -l rust crates/ -C 15 -w

# Default RUST_LOG for dev commands: surfaces cortex::llm lifecycle logs
# (request start → response headers → first byte → stream end) and other
# cortex-internal debug output. Override by exporting RUST_LOG before invoking.
export RUST_LOG := env_var_or_default("RUST_LOG", "info,cortex=debug,borgd=debug,interface=debug,gateway=debug")

# Run the TUI/CLI client (borg)
run *args="":
    cargo run --bin borg -- {{args}}

# Run the daemon (borgd)
daemon *args="":
    cargo run --bin borgd -- {{args}}

# Kill any running borgd + borg TUI from this repo's target/ so the next
# `just daemon`/`just run` picks up the latest build. Path-scoped so we never
# touch unrelated binaries (e.g. borgbackup's `borg` from Homebrew).
stop:
    -@if [ -f ~/.borg/borgd.pid ]; then kill "$(cat ~/.borg/borgd.pid)" 2>/dev/null; fi
    -@pkill -f "{{justfile_directory()}}/target/[^ ]*/borgd?( |$)" 2>/dev/null
    @i=0; while [ -e ~/.borg/borgd.pid ] && kill -0 "$(cat ~/.borg/borgd.pid)" 2>/dev/null; do \
        i=$((i+1)); \
        if [ $i -gt 50 ]; then kill -9 "$(cat ~/.borg/borgd.pid)" 2>/dev/null; break; fi; \
        sleep 0.2; \
    done
    @echo "stopped"

# Clean cargo target dirs (main + worktrees)
clean:
    cargo clean
    rm -rf .claude/worktrees/*/target

# Wipe all config/data so you can re-run onboarding from scratch
reset:
    rm -rf ~/.borg

# ── Web surfaces (apps/, packages/) ──────────────────────────────────────────
# The desktop app, the browser SPA, and their shared packages are one Bun
# workspace. `just gen` regenerates the TypeScript client from the same protos
# `crates/protocols/build.rs` compiles for Rust.

# Install workspace dependencies
web-install:
    bun install

# Regenerate the TypeScript gRPC client from crates/protocols/proto
gen:
    bun x buf generate

# Fail if the committed client is out of date with the protos
gen-check: gen
    git diff --exit-code packages/client/src/gen

# Typecheck + format-check every TypeScript package
web-lint:
    bun x prettier --check "apps/**/*.{ts,tsx,css,html}" "packages/**/*.{ts,tsx}"
    bun run --filter '*' typecheck

# Unit-test the TypeScript packages and the Electron main process (Vitest).
# Coverage is on: each package's vitest.config carries the 90% threshold, so a
# change that drops coverage fails here rather than being noticed a month later.
web-test:
    bun run --cwd packages/ui test --coverage.enabled
    bun run --cwd packages/app test --coverage.enabled
    bun run --cwd apps/desktop test --coverage.enabled
    bun run --cwd apps/web test --coverage.enabled

# Build the browser SPA. Run before `cargo build` — borgd embeds apps/web/dist.
web-build:
    bun run --cwd apps/web build

# Browser SPA dev server (proxies /grpc to a local borgd on :7842)
web-dev:
    bun run --cwd apps/web dev

# Desktop app in dev (Vite + Electron with hot reload)
desktop-dev:
    bun run --cwd apps/desktop dev

# Build the desktop app (renderer + Electron main)
desktop-build:
    bun run --cwd apps/desktop build

# Everything CI runs for the web surfaces
web-ci: web-install gen-check web-lint web-test web-build desktop-build
