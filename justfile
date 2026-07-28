# Rust project checks

set positional-arguments
set shell := ["bash", "-euo", "pipefail", "-c"]

# List available commands
default:
    @just --list

# Run project checks through checkle
check:
    checkle run all

# Run check and fail if there are uncommitted changes for CI
check-ci: check
    #!/usr/bin/env bash
    set -euo pipefail
    if ! git diff --quiet || ! git diff --cached --quiet; then
        echo "Error: check caused uncommitted changes"
        echo "Run 'just check' locally and commit the results"
        git diff --stat
        exit 1
    fi

# Install shims into the Git hooks directory
install-hooks:
    scripts/install-git-hook-shims

# Check Rust formatting through checkle
format:
    checkle run format-check

# Check clippy through checkle
clippy:
    checkle run clippy

# Check the build through checkle
build:
    checkle run build

# Run tests through checkle
test:
    checkle run test

# Install release binary globally
install:
    cargo install --offline --path . --locked

# Install debug binary globally via symlink
install-dev:
    cargo build && ln -sf $(pwd)/target/debug/claude-code-proxy ~/.cargo/bin/claude-code-proxy

# Run the application
run *ARGS:
    cargo run -- "$@"

# Build and open the monitor demo in a running CuaBot session
cua-monitor-demo session:
    scripts/cua-monitor-demo '{{session}}'

# Run the docs development server
docs:
    #!/usr/bin/env bash
    set -euo pipefail
    bun install --cwd docs --frozen-lockfile
    preferred_port=4321
    max_port=$((preferred_port + 50))
    for ((port = preferred_port; port <= max_port; port++)); do
        if ! nc -z 127.0.0.1 "$port" >/dev/null 2>&1; then
            exec bun run --cwd docs dev --port "$port"
        fi
    done
    echo "Error: could not find an available docs port between ${preferred_port} and ${max_port}" >&2
    exit 1

# Internal release helper
_release bump *ARGS:
    @cargo-release {{bump}} {{ARGS}}

# Release a new patch version
release *ARGS:
    @just _release patch --skip-publish {{ARGS}}
