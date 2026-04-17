// First Playwright integration test: a real browser loads the
// served `dist/`, the bundled `bootstrap.js` picks real-kernel
// mode as its default boot path, the kernel Worker fetches
// `/assets/kernel.wasm` + every `/assets/bin/*.wasm` listed in
// `/manifest.json`, registers a synthetic boot-loader pid,
// dispatches `PROC_SPAWN(/bin/init)`, and `init` runs to
// completion through the real Rust kernel. Init itself then
// fires TWO fire-and-forget `pmos_ext.proc_spawn` calls — first
// `/bin/hello-std`, then `/bin/display-server` — and exits.
// The substrate's spawn router creates a dedicated user Worker
// per child, so init + hello-std + display-server overlap:
// three concurrent pids, three distinct linear memories, three
// per-pid SAB rings serviced round-robin by the kernel Worker's
// dispatch loop.
//
// The observable signal is the page console — `bootstrap.ts`
// in real-kernel mode prefixes every flushed `/dev/console`
// line with `[real-kernel]`. The test scrapes those lines via
// `page.on('console', ...)` and asserts the expected sequence
// from init + hello-std + display-server reaches the browser.
// Ordering is pinned only within each pid (and within init →
// child, since children start only after `proc_spawn` returns);
// between the two children, interleaving is expected.
//
// The bare URL `/index.html` (no hash) is what a fresh visitor
// hits, so the test deliberately uses that: if real-kernel is
// no longer the default, the test fails even when the
// explicit `#real-kernel` hash continues to work.
//
// The second test exercises the input round-trip: under
// `/index.html#input-echo` the bootstrap swaps its boot binary
// to `/bin/hello_input_echo`, which polls `/dev/input_kbd` in a
// tight EAGAIN loop. The test drives `page.keyboard.press(...)`
// to synthesise keydown events, the bootstrap's DOM keydown
// handler posts `input:kbd` messages to the kernel Worker, the
// kernel worker's InputDriver calls `KernelWasmHost.injectInput`
// to deposit the bytes into the kbd ring, and the user Worker's
// next `fd_read` iteration picks them up and echoes them to
// `/dev/console`.

import { expect, test } from "@playwright/test";

test("real kernel is the default boot path and runs init -> hello-std + display-server", async ({ page }) => {
  const consoleLines: string[] = [];
  page.on("console", (msg) => {
    consoleLines.push(msg.text());
  });
  page.on("pageerror", (err) => {
    consoleLines.push(`[pageerror] ${err.message}`);
  });

  await page.goto("/index.html");

  // Poll until display-server's trailing "fb blit ok" line arrives
  // — that's the last observable signal of a successful boot (it
  // only prints after every step 1..7 of the display-server flow
  // succeeds, so its presence implies hello-std's line and every
  // init line have already landed). On a local dev-server the full
  // three-pid boot completes in ~300 ms cold; the 15 s timeout is
  // for cold-start CI.
  const blitOkLine = () =>
    consoleLines.find((l) =>
      l.includes("[real-kernel] display-server fb blit ok"),
    ) ?? null;
  await expect.poll(blitOkLine, { timeout: 15_000 }).not.toBeNull();

  // With the trailing display-server line observed, every other
  // line MUST already be present. Pull the indices and assert the
  // ordering that IS stable (within a single pid + init → child).
  // Ordering BETWEEN hello-std and display-server is NOT asserted —
  // they run concurrently in separate Workers.
  const initStartIdx = consoleLines.findIndex((l) =>
    l.includes("[real-kernel] init starting"),
  );
  const initSpawnHelloStdIdx = consoleLines.findIndex((l) =>
    /\[real-kernel\] init spawned hello-std pid=\d+/.test(l),
  );
  const initSpawnDisplayServerIdx = consoleLines.findIndex((l) =>
    /\[real-kernel\] init spawned display-server pid=\d+/.test(l),
  );
  const initExitIdx = consoleLines.findIndex((l) =>
    l.includes("[real-kernel] init exiting"),
  );
  const helloStdIdx = consoleLines.findIndex((l) =>
    l.includes("[real-kernel] hello from std"),
  );
  const displayServerStartIdx = consoleLines.findIndex((l) =>
    l.includes("[real-kernel] display-server starting"),
  );
  const displayServerBlitIdx = consoleLines.findIndex((l) =>
    l.includes("[real-kernel] display-server fb blit ok"),
  );

  expect(initStartIdx).toBeGreaterThanOrEqual(0);
  expect(initSpawnHelloStdIdx).toBeGreaterThan(initStartIdx);
  expect(initSpawnDisplayServerIdx).toBeGreaterThan(initSpawnHelloStdIdx);
  expect(initExitIdx).toBeGreaterThan(initSpawnDisplayServerIdx);
  // Children start only after init's proc_spawn returns — init's
  // "exiting" line is just three more fd_writes + proc_exit, which
  // finishes long before either child's std startup completes.
  expect(helloStdIdx).toBeGreaterThan(initExitIdx);
  expect(displayServerStartIdx).toBeGreaterThan(initExitIdx);
  // Within display-server's own output, "starting" must precede
  // "fb blit ok" (single pid, sequential prints).
  expect(displayServerBlitIdx).toBeGreaterThan(displayServerStartIdx);

  expect(consoleLines.some((l) => l.includes("real kernel ready"))).toBe(true);
  expect(consoleLines.some((l) => l.includes("real kernel panic"))).toBe(false);

  // DOM surface: real-kernel mode renders each captured line into a
  // `<pre id="pmos-real-console">` so the page itself shows the
  // boot output (not just dev tools). The element's text must
  // include every line all three pids produced.
  const domText = await page.locator("#pmos-real-console").innerText();
  expect(domText).toContain("init starting");
  expect(domText).toMatch(/init spawned hello-std pid=\d+/);
  expect(domText).toMatch(/init spawned display-server pid=\d+/);
  expect(domText).toContain("init exiting");
  expect(domText).toContain("hello from std");
  expect(domText).toContain("display-server starting");
  expect(domText).toContain("display-server fb blit ok");
  expect(domText.indexOf("init starting")).toBeLessThan(
    domText.indexOf("hello from std"),
  );
  expect(domText.indexOf("display-server starting")).toBeLessThan(
    domText.indexOf("display-server fb blit ok"),
  );

  // Three concurrent pids (init + hello-std + display-server) MUST
  // each live in their own user Worker under `createSpawnRouter`'s
  // management. `peakLiveWorkers` is the high-water mark of
  // `router.liveWorkers.size` across every message the bootstrap's
  // listener observes. The peak reaches 3 during the window where
  // init has spawned both children and neither has exited yet; this
  // is the load-bearing evidence that the substrate round-robins
  // across THREE per-pid SAB rings, not just two. The kernel Worker
  // is NOT counted — only user Workers under the router's management.
  const peakAttr = await page
    .locator("body")
    .getAttribute("data-pmos-peak-live-workers");
  expect(peakAttr).not.toBeNull();
  expect(Number(peakAttr)).toBeGreaterThanOrEqual(3);

  // T234: the kernel-wake-slot transport landed before any user Worker
  // spawned. Without this, every spawn would race the SAB-allocation
  // path. The bootstrap stamps `data-pmos-wake-slot-ready="1"` on
  // `<body>` the moment the `kernel:wake-slot` message arrives — the
  // ordering with the four-line console output above implicitly
  // proves it landed first (the FIRST proc:spawn happens AFTER
  // bootRealKernel posts kernel:wake-slot and BEFORE init's println
  // even runs), but checking it explicitly is cheap and forces a
  // clear failure mode if the kernel-worker entry's posting order
  // ever regresses.
  const wakeSlotReady = await page
    .locator("body")
    .getAttribute("data-pmos-wake-slot-ready");
  expect(wakeSlotReady).toBe("1");
});

test("display-server: std binary binds, accepts a client, relays pixels to /dev/fb0", async ({ page }) => {
  const consoleLines: string[] = [];
  page.on("console", (msg) => {
    consoleLines.push(msg.text());
  });
  page.on("pageerror", (err) => {
    consoleLines.push(`[pageerror] ${err.message}`);
  });

  // `#display-server` selects `/bin/display-server` — the first `std`
  // binary in the workspace to do IPC over the M1 multi-process
  // substrate. The binary plays server, client, AND framebuffer-writer
  // in a single boot pass (mirroring `display-server-lite`'s
  // composition-test pattern, promoted into a real std binary spawned
  // as a real user Worker):
  //
  //   display_bind() → display_connect() → ipc_accept() →
  //   fd_write(client, pixels) → fd_read(server, buf) →
  //   path_open("/dev/fb0") → fd_write(fb_fd, buf) → return
  //
  // Observable signals: two `println!` lines — `"display-server
  // starting"` at entry and `"display-server fb blit ok"` after the
  // final `fd_write(fb_fd)`. The latter only prints on exit code 0
  // (every intermediate failure takes a `std::process::exit(N)` path
  // and never reaches the final println), so its presence implicitly
  // proves the whole chain succeeded.
  await page.goto("/index.html#display-server");

  // Wait for the user Worker to spawn. `data-pmos-peak-live-workers`
  // bumps to `1` when the spawn router instantiates the display-server
  // Worker in response to the kernel's `proc:spawn` — same signal the
  // input-echo test uses.
  await expect
    .poll(
      async () => {
        const attr = await page
          .locator("body")
          .getAttribute("data-pmos-peak-live-workers");
        return attr ? Number(attr) : 0;
      },
      { timeout: 15_000 },
    )
    .toBeGreaterThanOrEqual(1);

  // Wait for the binary's trailing println to arrive. Cold-path on a
  // local dev-server is ~250 ms (std startup + bind + connect + accept
  // + two IPC round trips + path_open + fb write); the generous
  // timeout is for cold-start CI.
  await expect
    .poll(
      () =>
        consoleLines.find((l) =>
          l.includes("[real-kernel] display-server fb blit ok"),
        ) ?? null,
      { timeout: 15_000 },
    )
    .not.toBeNull();

  // With the exit line observed, the starting line MUST already be
  // present AND ordered before the exit line.
  const startIdx = consoleLines.findIndex((l) =>
    l.includes("[real-kernel] display-server starting"),
  );
  const blitIdx = consoleLines.findIndex((l) =>
    l.includes("[real-kernel] display-server fb blit ok"),
  );
  expect(startIdx).toBeGreaterThanOrEqual(0);
  expect(blitIdx).toBeGreaterThan(startIdx);

  // DOM surface: `<pre id="pmos-real-console">` carries the same
  // lines (bootstrap's ConsoleHost.onOutput appends every flushed
  // `/dev/console` line to this element).
  const domText = await page.locator("#pmos-real-console").innerText();
  expect(domText).toContain("display-server starting");
  expect(domText).toContain("display-server fb blit ok");
  expect(domText.indexOf("display-server starting")).toBeLessThan(
    domText.indexOf("display-server fb blit ok"),
  );

  expect(consoleLines.some((l) => l.includes("real kernel panic"))).toBe(false);
});

test("input round-trip: keydown in real-kernel mode echoes to console", async ({ page }) => {
  const consoleLines: string[] = [];
  page.on("console", (msg) => {
    consoleLines.push(msg.text());
  });
  page.on("pageerror", (err) => {
    consoleLines.push(`[pageerror] ${err.message}`);
  });

  // `#input-echo` opts the bootstrap into `bootBinary = /bin/hello_input_echo`
  // (the no_std cdylib under `crates/hello-input-echo/`) instead of `/bin/init`.
  // The binary polls `/dev/input_kbd` in an EAGAIN loop, so the first
  // iteration that finds bytes echoes them straight to stdout and exits.
  await page.goto("/index.html#input-echo");

  // Wait for the wake slot to arrive AND for the user Worker to spawn. The
  // `data-pmos-peak-live-workers` attribute flips to "1" (or higher) once the
  // router instantiates hello-input-echo's Worker in response to the kernel's
  // `proc:spawn`. That's the signal the DOM keydown handler has somewhere to
  // send bytes.
  await expect
    .poll(
      async () => {
        const attr = await page
          .locator("body")
          .getAttribute("data-pmos-peak-live-workers");
        return attr ? Number(attr) : 0;
      },
      { timeout: 15_000 },
    )
    .toBeGreaterThanOrEqual(1);

  // Press "x" then Enter. The bootstrap's keydown listener converts each key
  // to a byte (via the existing `keyToBytes` helper: printable ASCII → UTF-8,
  // Enter → 0x0a) and posts an `input:kbd` message on each. The kernel's
  // console driver line-buffers, so the newline is what forces a flush of the
  // full "x\n" payload to `onConsoleWrite`.
  await page.keyboard.press("x");
  await page.keyboard.press("Enter");

  // hello-input-echo's fd_read picks the queued bytes up on its next loop
  // iteration, writes them all in one fd_write, and exits. bootstrap's
  // ConsoleHost.onOutput appends the decoded text to `#pmos-real-console` AND
  // logs a `[real-kernel] x` line to the page console.
  await expect
    .poll(
      () => consoleLines.find((l) => l === "[real-kernel] x") ?? null,
      { timeout: 10_000 },
    )
    .not.toBeNull();

  // DOM surface: the pre element shows the echoed character too.
  const domText = await page.locator("#pmos-real-console").innerText();
  expect(domText).toContain("x");

  expect(consoleLines.some((l) => l.includes("real kernel panic"))).toBe(false);
});
