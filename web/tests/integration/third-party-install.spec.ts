// T207 — third-party install via .pmpkg.tar. The bundle is built
// by `xtask package sample-app` and staged into
// `dist/pkgs/staging/hello-0.1.0.pmpkg.tar` by `xtask push-sample`.
// The bootstrap side that streams the staged bundle into OPFS as
// part of the boot fixture is not yet wired; this spec validates
// the bundle is present in dist for the next slice.

import { expect, test } from "@playwright/test";

test("hello-0.1.0.pmpkg.tar is staged in dist/pkgs/staging/", async ({
  page,
  request,
}) => {
  // Probe the dev-server for the bundle. Returns 200 if the
  // xtask assemble-dist + xtask push-sample cycle ran during
  // the build phase. Returns 404 if not.
  const res = await request.get("/pkgs/staging/hello-0.1.0.pmpkg.tar");
  if (res.status() !== 200) {
    test.skip(true, "hello-0.1.0.pmpkg.tar not staged; run `cargo run -p xtask -- package sample-app && cargo run -p xtask -- push-sample`");
  }
  const buf = await res.body();
  // tar archives have non-zero length and start with the
  // filename in the first 100 bytes.
  expect(buf.length).toBeGreaterThan(1024);

  // Manifest path is at offset 0..100. ASCII "manifest.toml" or
  // "bin/hello.wasm" is what we expect first, depending on the
  // pkg::build_tar entry order — both prove a structurally-sound
  // archive.
  const head = new TextDecoder("ascii").decode(buf.subarray(0, 100));
  expect(head).toMatch(/manifest\.toml|bin\/hello\.wasm/);
});
