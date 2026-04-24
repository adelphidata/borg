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

# Run all tests
test:
    cargo test --all

# Build debug
build:
    cargo build

# Build release
release:
    cargo build --release

# Run format check + clippy (what pre-commit does)
check:
    cargo fmt --check
    cargo clippy -- -D warnings

# Install local CI helpers (cargo-audit, cargo-deny)
install-ci-tools:
    RUSTUP_TOOLCHAIN=stable cargo install cargo-audit --locked
    RUSTUP_TOOLCHAIN=stable cargo install cargo-deny --locked

# Security advisory scan (mirrors CI's Security Audit job)
audit:
    RUSTUP_TOOLCHAIN=stable cargo audit

# Dependency check (mirrors CI's Dependency Check job)
deny:
    cargo deny --all-features check

# Run everything CI runs (format, clippy, test, build, audit, deny)
ci: check
    cargo test --workspace
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

# Run the binary
run *args="":
    cargo run -- {{args}}

# Clean cargo target dirs (main + worktrees)
clean:
    cargo clean
    rm -rf .claude/worktrees/*/target

# Wipe all config/data so you can re-run onboarding from scratch
reset:
    rm -rf ~/.borg
