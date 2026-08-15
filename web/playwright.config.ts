// Playwright configuration for the PMos integration tests.
//
// The tests under `tests/integration/` exercise the full stack
// from a real browser: the bundled bootstrap, the kernel Worker,
// the real Rust kernel cdylib, and the user wasm binaries — all
// served from the assembled `dist/` directory by the xtask
// dev-server (which adds the COOP/COEP headers
// SharedArrayBuffer + Atomics.wait require).
//
// The dev-server is started by Playwright via the `webServer`
// entry below; the binary `cargo run -p xtask -- dev-server` is
// expected to exist by the time these tests run, which in turn
// requires the workspace to have been built. The Justfile's
// `test-integration` target sequences `just build` first to make
// that happen.

import { defineConfig } from "@playwright/test";

const PORT = 8081;

export default defineConfig({
  testDir: "./tests/integration",
  fullyParallel: false,
  workers: 1,
  reporter: "list",
  retries: 0,
  use: {
    baseURL: `http://127.0.0.1:${PORT}`,
    trace: "off",
  },
  webServer: {
    // Run the xtask dev-server out of the workspace root so the
    // binary's `--dir=dist` resolves relative to the right tree.
    // The dev sandbox does not put `cargo` on the default PATH, so
    // prefix the command with the standard rustup install location;
    // a fully-installed dev environment with cargo already on PATH
    // is a strict superset of what this prefix supplies.
    command: `cd .. && PATH="$HOME/.cargo/bin:$PATH" cargo run --locked --quiet -p xtask -- dev-server --dir=dist --port=${PORT}`,
    url: `http://127.0.0.1:${PORT}/index.html`,
    // Never attach the release gate to a potentially stale developer server.
    reuseExistingServer: false,
    timeout: 60_000,
    stdout: "pipe",
    stderr: "pipe",
  },
  projects: [
    {
      name: "chromium",
      testIgnore: /unsupported-browser\.spec\.ts/,
      use: {
        // Headless chromium with the COOP/COEP-friendly defaults.
        // No special launch flags needed — the dev-server's
        // headers do the cross-origin isolation work, not the
        // browser config.
        browserName: "chromium",
      },
    },
    {
      name: "firefox",
      testIgnore: /unsupported-browser\.spec\.ts/,
      use: {
        browserName: "firefox",
      },
    },
    {
      name: "webkit",
      // Playwright's Linux WebKit build exposes no OPFS API at all.
      // The product contract classifies that substrate as unsupported;
      // this project gates the explicit stop screen instead of treating
      // a volatile tmpfs desktop as persistence coverage.
      testMatch: /unsupported-browser\.spec\.ts/,
      use: {
        browserName: "webkit",
      },
    },
  ],
});
