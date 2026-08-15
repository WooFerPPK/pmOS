# PMos build orchestrator
#
# Targets are grouped by phase: build, test, dev, release. Every target
# that produces output writes under dist/. Intermediate artefacts under
# build/. Both directories are gitignored.

default: build

# ---------------------------------------------------------------------------
# Build
# ---------------------------------------------------------------------------

# Install the exact lock-backed web toolchain only when it is absent, stale, or
# incomplete. The copied lockfile lives inside node_modules, so `just clean`
# removes it with the install and the next Node-backed recipe restores both.
node-deps:
    @set -eu; \
        stamp=web/node_modules/.pmos-package-lock.json; \
        if [ ! -f "$stamp" ] || ! cmp -s web/package-lock.json "$stamp" \
            || [ ! -x web/node_modules/.bin/esbuild ] \
            || [ ! -x web/node_modules/.bin/tsc ] \
            || [ ! -x web/node_modules/.bin/vitest ] \
            || [ ! -x web/node_modules/.bin/playwright ]; then \
            echo "[just] installing locked web dependencies..."; \
            (cd web && npm ci); \
            cp web/package-lock.json "$stamp"; \
        fi

# Full build: Rust crates (kernel + userland) + TS (bootstrap + sw) + dist/ assembly
build: node-deps
    @echo "[just] building PMos..."
    cargo build --locked --release --target wasm32-unknown-unknown -p kernel --no-default-features
    cargo build --locked --release --target wasm32-wasip1 \
        -p init -p display-server -p display-client-demo -p toolkit \
        -p shell -p alt-shell -p sh -p term -p files -p edit -p settings -p sysmon \
        -p coreutils -p pkginstall \
        -p sample-app -p toolkit-free-client -p hello-wasi-min \
        -p hello-wasi-spawner -p ipc-self-test -p hello-framebuffer \
        -p display-server-lite -p hello-wasi-bootstrap \
        -p hello-fb-blit -p hello-input-echo \
        -p hello-std -p hello-clock -p hello-toplevel \
        -p hello-sigchld -p hello-kill-probe -p hello-pid \
        -p hello-self-probe -p hello-self-kill -p hello-ppid -p hello-caps \
        -p hello-raise -p hello-wait-noop -p hello-cap-check \
        -p hello-random -p hello-fd-close-bad -p hello-fd-close-good \
        -p hello-yield-loop -p hello-cap-list -p hello-trap -p mem-adversary
    cd web && npx --no-install esbuild src/bootstrap.ts --bundle --outfile=../build/assets/bootstrap.js --format=esm
    cd web && npx --no-install esbuild src/kernel-worker-entry.ts --bundle --outfile=../build/assets/kernel-worker.js --format=esm
    cd web && npx --no-install esbuild src/user-worker-entry.ts --bundle --outfile=../build/assets/user-worker.js --format=esm
    cd web && npx --no-install esbuild src/sw.ts --bundle --outfile=../build/sw.js --format=esm
    cargo run --locked -p xtask -- assemble-dist

# Serve dist/ locally on http://localhost:8080 with COOP/COEP headers
dev: build
    cargo run -p xtask -- dev-server --dir=dist --port=8080

# ---------------------------------------------------------------------------
# Test
# ---------------------------------------------------------------------------

# Run every test layer in sequence (the CI gate). The workspace test includes
# the focused Rust isolation suites below, so they are not run twice here.
# Audit matches remain non-blocking in v1; audit-harness correctness is blocking.
test: test-format test-clippy test-rust-workspace test-typescript test-node-audit test-drivers test-integration test-idle-cpu test-perf test-non-goal-audit

# Rust source must stay mechanically formatted across the whole workspace.
test-format:
    cargo fmt --all -- --check

# Treat every Clippy finding as a release-gate failure, including tests/binaries.
test-clippy:
    cargo clippy --locked --workspace --all-targets -- -D warnings

# Complete locked Rust gate, including every workspace crate and isolation suite
test-rust-workspace:
    cargo test --locked --workspace

# Strict TypeScript compiler gate; esbuild alone does not type-check
test-typescript: node-deps
    cd web && npx --no-install tsc --noEmit

# Build/test dependencies execute in developer and CI environments
test-node-audit:
    cd web && npm audit --audit-level=moderate

# Distribution assembly isolation tests (required-artifact and manifest integrity)
test-build-tools:
    cargo test --locked -p xtask

# Kernel isolation tests (native host target via the Platform abstraction)
test-kernel:
    cargo test --locked -p kernel

# Display server isolation tests (mock client + mock framebuffer)
test-display-server:
    cargo test --locked -p display-server

# Toolkit tests against mock display server
test-toolkit:
    cargo test --locked -p toolkit

# TypeScript integration-style unit tests load the freshly compiled kernel and
# userland WASM fixtures, so this gate must build them on a clean checkout.
test-drivers: build
    cd web && npx --no-install vitest run

# Playwright integration tests (includes layering test and window-close)
test-integration: build
    cd web && npx --no-install playwright test

# Linux release gate for settled blank, shell-only, fresh six-app, and restored six-app browser CPU.
test-idle-cpu: build
    cd web && node --test scripts/idle-cpu-accounting.test.mjs scripts/idle-cpu-gate.test.mjs
    cd web && node scripts/idle-cpu-gate.mjs

# Native-Rust perf harness (input latency p95 gate, per T220)
test-perf: build
    cargo run --locked --release -p integration-tests --bin input-latency

# Non-goal compliance audit (T222). Its deterministic fixture test is blocking;
# emitted audit matches remain non-blocking and are classified by hand in
# docs/non-goal-compliance.md.
test-non-goal-audit:
    @bash scripts/non-goal-audit.test.sh
    @bash scripts/non-goal-audit.sh > /tmp/pmos-non-goal-audit.log
    @echo "[just] non-goal audit: $(wc -l < /tmp/pmos-non-goal-audit.log) lines across $(grep -c '^##' /tmp/pmos-non-goal-audit.log) categories (see docs/non-goal-compliance.md)"

# ---------------------------------------------------------------------------
# Packaging and release
# ---------------------------------------------------------------------------

# Build a .pmpkg.tar bundle for a given crate (e.g. `just package sample-app`)
package crate:
    cargo run -p xtask -- package {{crate}}

# Copy a freshly-built sample-app bundle into the running PMos OPFS (dev only)
push-sample: build
    cargo run -p xtask -- package sample-app
    cargo run -p xtask -- push-sample

# Regenerate deterministic binary/bitmap release assets after editing their source tables.
generate-release-assets:
    cargo run --locked -p xtask -- gen-keymap-assets
    cargo run --locked -p xtask -- gen-font-assets

# ---------------------------------------------------------------------------
# Housekeeping
# ---------------------------------------------------------------------------

clean:
    cargo clean
    rm -rf build dist web/node_modules web/test-results web/playwright-report
