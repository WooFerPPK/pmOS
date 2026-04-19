// First Playwright integration test: a real browser loads the
// served `dist/`, the bundled `bootstrap.js` picks real-kernel
// mode as its default boot path, the kernel Worker fetches
// `/assets/kernel.wasm` + every `/assets/bin/*.wasm` listed in
// `/manifest.json`, registers a synthetic boot-loader pid,
// dispatches `PROC_SPAWN(/bin/init)`, and `init` runs to
// completion through the real Rust kernel. Init itself fires
// THREE fire-and-forget `pmos_ext.proc_spawn` calls — first
// `/bin/hello-std`, then `/bin/display-server`, then
// `/bin/display-client-demo` — and exits. The substrate's spawn
// router creates a dedicated user Worker per child, so init +
// hello-std + display-server + display-client-demo overlap:
// four concurrent pids, four distinct linear memories, four
// per-pid SAB rings serviced round-robin by the kernel Worker's
// dispatch loop.
//
// This is the first slice where two separate WASM binaries in
// separate user Workers actually exchange bytes through a PMos
// IPC socket: display-server binds `/run/display` and spins on
// `ipc_accept` (EAGAIN poll), display-client-demo connects
// (ECONNREFUSED poll) + `fd_write(PIXELS)` + exits, and
// display-server's next accept returns a real server fd → reads
// the 16-byte RGBA payload → relays it to `/dev/fb0` → prints
// `"fb blit ok"` → exits.
//
// The observable signal is the page console — `bootstrap.ts`
// in real-kernel mode prefixes every flushed `/dev/console`
// line with `[real-kernel]`. The test scrapes those lines via
// `page.on('console', ...)` and asserts the expected sequence
// from all four pids reaches the browser. Ordering is pinned
// within each pid (and within init → child, since children can
// only start after `proc_spawn` returns); between children,
// interleaving is expected EXCEPT for the protocol-ordered pair
// — `display-server fb blit ok` MUST come after
// `display-client-demo sent pixels` because the server's
// `fd_read` unblocks only after the client's `fd_write` lands.
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

test("real kernel is the default boot path and runs init -> hello-std + display-server <-> display-client-demo", async ({ page }) => {
  const consoleLines: string[] = [];
  page.on("console", (msg) => {
    consoleLines.push(msg.text());
  });
  page.on("pageerror", (err) => {
    consoleLines.push(`[pageerror] ${err.message}`);
  });

  await page.goto("/index.html");

  // Slice 2a (kernel ipc_accept blocking) changed the terminal
  // observable signal. Pre-slice: display-server's inner EAGAIN
  // poll exhausted on iteration 2 of the outer loop (no 3rd
  // client ever arrives), the loop broke cleanly on `served_any`,
  // and display-server printed "fb blit ok" before exiting. Post-
  // slice: display-server's ipc_accept on iteration 2 blocks
  // indefinitely in the kernel (parked on the listener with no
  // peer to wake it), so "fb blit ok" NEVER prints and display-
  // server stays live. This is the correct long-running-server
  // shape the arc is building toward — slice 2b's signal-driven
  // exit is what will make display-server exit cleanly on
  // SIGTERM. For this slice, the terminal observable shifts from
  // "fb blit ok" to "display-server served client 1" (the second
  // served-client line) — its presence still implies every
  // earlier line is on the console, and both clients got their
  // pixels relayed to /dev/fb0. 15 s timeout for cold-start CI.
  const secondServedLine = () => {
    const served = consoleLines.filter((l) =>
      /\[real-kernel\] display-server served client \d+/.test(l),
    );
    return served.length >= 2 ? served[1]! : null;
  };
  await expect.poll(secondServedLine, { timeout: 15_000 }).not.toBeNull();

  // With the trailing display-server line observed, every other
  // line MUST already be present. Pull the indices and assert the
  // ordering that IS stable (within a single pid + init → child,
  // plus the protocol-ordered pair client-sent → server-blit).
  // Ordering BETWEEN hello-std and the display pair is NOT asserted
  // — they run concurrently in separate Workers.
  const initStartIdx = consoleLines.findIndex((l) =>
    l.includes("[real-kernel] init starting"),
  );
  const initSpawnHelloStdIdx = consoleLines.findIndex((l) =>
    /\[real-kernel\] init spawned hello-std pid=\d+/.test(l),
  );
  const initSpawnDisplayServerIdx = consoleLines.findIndex((l) =>
    /\[real-kernel\] init spawned display-server pid=\d+/.test(l),
  );
  const initSpawnDisplayClientIdx = consoleLines.findIndex((l) =>
    /\[real-kernel\] init spawned display-client-demo pid=\d+/.test(l),
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
  const displayClientStartIndices = consoleLines
    .map((l, i) =>
      l.includes("[real-kernel] display-client-demo starting") ? i : -1,
    )
    .filter((i) => i >= 0);
  const displayClientSentIndices = consoleLines
    .map((l, i) =>
      l.includes("[real-kernel] display-client-demo sent pixels") ? i : -1,
    )
    .filter((i) => i >= 0);
  const displayClientStartIdx = displayClientStartIndices[0] ?? -1;
  const displayClientSentIdx = displayClientSentIndices[0] ?? -1;
  const displayServerServedIndices = consoleLines
    .map((l, i) =>
      /\[real-kernel\] display-server served client \d+/.test(l) ? i : -1,
    )
    .filter((i) => i >= 0);

  expect(initStartIdx).toBeGreaterThanOrEqual(0);
  expect(initSpawnHelloStdIdx).toBeGreaterThan(initStartIdx);
  expect(initSpawnDisplayServerIdx).toBeGreaterThan(initSpawnHelloStdIdx);
  expect(initSpawnDisplayClientIdx).toBeGreaterThan(initSpawnDisplayServerIdx);
  expect(initExitIdx).toBeGreaterThan(initSpawnDisplayClientIdx);
  // Children start only after init's proc_spawn returns — init's
  // remaining fd_writes + proc_exit finishes long before any
  // child's std startup completes.
  expect(helloStdIdx).toBeGreaterThan(initExitIdx);
  expect(displayServerStartIdx).toBeGreaterThan(initExitIdx);
  expect(displayClientStartIdx).toBeGreaterThan(initExitIdx);
  // Within display-client-demo's own output, "starting" must precede
  // "sent pixels" (single pid, sequential prints).
  expect(displayClientSentIdx).toBeGreaterThan(displayClientStartIdx);
  // Protocol ordering: display-server's accept unblocks only after
  // display-client-demo's ipc_connect lands. display-client-demo
  // prints "sent pixels" immediately after its fd_write, and
  // display-server prints "served client {i}" immediately after
  // relaying the payload to /dev/fb0. So the client's first
  // "sent pixels" MUST come before the server's first
  // "served client" line.
  expect(displayServerServedIndices[0]!).toBeGreaterThan(
    displayClientSentIndices[0]!,
  );
  // Two display-client-demo pids → two "starting" + two "sent
  // pixels" lines on the console.
  expect(displayClientStartIndices).toHaveLength(2);
  expect(displayClientSentIndices).toHaveLength(2);
  // display-server's outer accept loop serves both clients, so
  // two "served client {i}" lines land (indices 0 and 1 from the
  // server's iteration counter — the order relative to client
  // pids is not pinned because the clients run concurrently).
  expect(displayServerServedIndices).toHaveLength(2);
  // Slice 2a deliberately does not assert "fb blit ok" — display-
  // server parks indefinitely on iteration 2 of its outer loop
  // (kernel blocks on the empty listener backlog with no 3rd
  // client). Slice 2b lands signal-driven exit and the trailing
  // "fb blit ok" line becomes observable again.

  expect(consoleLines.some((l) => l.includes("real kernel ready"))).toBe(true);
  expect(consoleLines.some((l) => l.includes("real kernel panic"))).toBe(false);

  // DOM surface: real-kernel mode renders each captured line into a
  // `<pre id="pmos-real-console">` so the page itself shows the
  // boot output (not just dev tools). The element's text must
  // include every line all four pids produced.
  const domText = await page.locator("#pmos-real-console").innerText();
  expect(domText).toContain("init starting");
  expect(domText).toMatch(/init spawned hello-std pid=\d+/);
  expect(domText).toMatch(/init spawned display-server pid=\d+/);
  const domClientSpawnMatches = domText.match(
    /init spawned display-client-demo pid=\d+/g,
  );
  expect(domClientSpawnMatches).not.toBeNull();
  expect(domClientSpawnMatches!).toHaveLength(2);
  expect(domText).toContain("init exiting");
  expect(domText).toContain("hello from std");
  expect(domText).toContain("display-server starting");
  expect(domText).toContain("display-client-demo starting");
  expect(domText).toContain("display-client-demo sent pixels");
  expect(domText).toMatch(/display-server served client \d+/);
  expect(domText.indexOf("init starting")).toBeLessThan(
    domText.indexOf("hello from std"),
  );
  expect(domText.indexOf("display-client-demo starting")).toBeLessThan(
    domText.indexOf("display-client-demo sent pixels"),
  );

  // Five concurrent pids (init + hello-std + display-server +
  // display-client-demo × 2) MUST each live in their own user
  // Worker under `createSpawnRouter`'s management. `peakLiveWorkers`
  // is the high-water mark of `router.liveWorkers.size` across
  // every message the bootstrap's listener observes. The peak
  // reaches 5 during the window where init has spawned all four
  // children and none have exited yet; this is the load-bearing
  // evidence that the substrate round-robins across FIVE per-pid
  // SAB rings. The kernel Worker is NOT counted — only user
  // Workers under the router's management.
  const peakAttr = await page
    .locator("body")
    .getAttribute("data-pmos-peak-live-workers");
  expect(peakAttr).not.toBeNull();
  expect(Number(peakAttr)).toBeGreaterThanOrEqual(5);

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
