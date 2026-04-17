# PMos build orchestrator
#
# Targets are grouped by phase: build, test, dev, release. Every target
# that produces output writes under dist/. Intermediate artefacts under
# build/. Both directories are gitignored.

default: build

# ---------------------------------------------------------------------------
# Build
# ---------------------------------------------------------------------------

# Full build: Rust crates (kernel + userland) + TS (bootstrap + sw) + dist/ assembly
build:
    @echo "[just] building PMos..."
    cargo build --release --target wasm32-unknown-unknown -p kernel --no-default-features
    cargo build --release --target wasm32-wasip1 \
        -p init -p display-server -p display-client-demo -p toolkit \
        -p shell -p sh -p term -p files -p edit -p settings -p sysmon \
        -p sample-app -p toolkit-free-client -p hello-wasi-min \
        -p hello-wasi-spawner -p ipc-self-test -p hello-framebuffer \
        -p display-server-lite -p hello-wasi-bootstrap \
        -p hello-fb-blit -p hello-input-echo \
        -p hello-std -p hello-clock
    cd web && npx esbuild src/bootstrap.ts --bundle --outfile=../build/assets/bootstrap.js --format=esm
    cd web && npx esbuild src/kernel-worker-entry.ts --bundle --outfile=../build/assets/kernel-worker.js --format=esm
    cd web && npx esbuild src/user-worker-entry.ts --bundle --outfile=../build/assets/user-worker.js --format=esm
    cd web && npx esbuild src/sw.ts --bundle --outfile=../build/sw.js --format=esm
    cargo run -p xtask -- assemble-dist

# Serve dist/ locally on http://localhost:8080 with COOP/COEP headers
dev: build
    cargo run -p xtask -- dev-server --dir=dist --port=8080

# ---------------------------------------------------------------------------
# Test
# ---------------------------------------------------------------------------

# Run every test layer in sequence (the CI gate)
test: test-kernel test-display-server test-toolkit test-drivers test-integration test-perf

# Kernel isolation tests (native host target via the Platform abstraction)
test-kernel:
    cargo test -p kernel

# Display server isolation tests (mock client + mock framebuffer)
test-display-server:
    cargo test -p display-server

# Toolkit tests against mock display server
test-toolkit:
    cargo test -p toolkit

# TypeScript driver unit tests
test-drivers:
    cd web && npx vitest run

# Playwright integration tests (includes layering test and window-close)
test-integration: build
    cd web && npx playwright test

# Native-Rust perf harness (input latency p95 gate, per T220)
test-perf: build
    cargo run --release -p integration-tests --bin input-latency

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

# ---------------------------------------------------------------------------
# Housekeeping
# ---------------------------------------------------------------------------

clean:
    cargo clean
    rm -rf build dist web/node_modules web/test-results web/playwright-report
