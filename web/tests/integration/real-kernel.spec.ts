// First Playwright integration test: a real browser loads the
// served `dist/`, the bundled `bootstrap.js` picks real-kernel
// mode as its default boot path, the kernel Worker fetches
// `/assets/kernel.wasm` + every `/assets/bin/*.wasm` listed in
// `/manifest.json`, registers a synthetic boot-loader pid,
// dispatches `PROC_SPAWN(/bin/init)`, and `init` runs to
// completion through the real Rust kernel. Init itself fires
// FOUR fire-and-forget `pmos_ext.proc_spawn` calls — first
// `/bin/hello-std`, then `/bin/display-server`, then
// `/bin/display-client-demo` twice — then enters a blocking
// `proc_wait` supervision loop (T095). The substrate's spawn
// router creates a dedicated user Worker per child, so init +
// hello-std + display-server + display-client-demo × 2 overlap:
// five concurrent pids, five distinct linear memories, five
// per-pid SAB rings serviced round-robin by the kernel Worker's
// dispatch loop.
//
// IPC round-trip: display-server binds `/run/display` and parks
// on blocking `ipc_accept` (slice 2a kernel park/wake);
// display-client-demo connects (ECONNREFUSED poll) +
// `fd_write(PIXELS)` + exits; display-server's accept wakes →
// reads the 16-byte RGBA payload → relays it to `/dev/fb0` →
// prints `"display-server served client N"` → re-parks on the
// next accept. After both clients exit, init's reap loop drains
// their Zombie states via `proc_wait`, prints
// `"init reaped child pid=..."` for each, then fires
// `proc_kill(ds_pid, SIGTERM)` to signal display-server. The
// display-server's pre-accept fd-3 poll picks up the SIGTERM,
// breaks the accept loop, and prints `"display-server fb blit ok"`
// before exiting. Init's final reaps collect display-server and
// hello-std, then prints `"init exiting"` and exits PID 1.
//
// The observable signal is the page console — `bootstrap.ts`
// in real-kernel mode prefixes every flushed `/dev/console`
// line with `[real-kernel]`. The test scrapes those lines via
// `page.on('console', ...)` and asserts the expected sequence
// from all five pids reaches the browser. Ordering is pinned
// within each pid (and within init → child, since children can
// only start after `proc_spawn` returns); between children,
// interleaving is expected EXCEPT for the protocol-ordered chain
// — `display-server served client` MUST come after
// `display-client-demo sent pixels`, `init sent SIGTERM` MUST
// come after both clients' exits (and therefore both
// `served client` lines), and `display-server fb blit ok` MUST
// come after `init sent SIGTERM`.
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

  // T095/T110 slice: the trailing observable is `init exiting`,
  // printed after init's proc_wait supervision loop has reaped
  // every spawned child. The reap order is driven by child exit
  // timing: display-client-demo × 2 exit first (after fd_write
  // landing the pixels), hello-std exits early (self-contained
  // std startup + one println + exit), display-server exits last
  // (only after init's SIGTERM interrupts its pre-accept fd-3
  // poll + it prints `fb blit ok`). `init exiting` arriving
  // means every earlier line — including the restored
  // `display-server fb blit ok` and the two `display-server
  // served client {i}` lines — is already on the console. 15 s
  // timeout for cold-start CI.
  const initExitingLine = () =>
    consoleLines.find((l) => l === "[real-kernel] init exiting") ?? null;
  await expect.poll(initExitingLine, { timeout: 15_000 }).not.toBeNull();

  // With init's trailing line observed, every other line MUST
  // already be present. Pull the indices and assert the ordering
  // that IS stable (within a single pid + init → child, plus the
  // protocol-ordered chain client-sent → server-served →
  // SIGTERM → fb-blit). Ordering BETWEEN hello-std and the
  // display pair is NOT asserted — they run concurrently in
  // separate Workers.
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

  // T095/T110 observables: init's SIGTERM line, display-server's
  // fb-blit-ok line, and init's reap lines are each located here
  // so the ordering assertions below can reference them.
  const initSigtermIdx = consoleLines.findIndex((l) =>
    /\[real-kernel\] init sent SIGTERM to display-server pid=\d+/.test(l),
  );
  const fbBlitOkIdx = consoleLines.findIndex((l) =>
    l.includes("[real-kernel] display-server fb blit ok"),
  );
  const initReapedIndices = consoleLines
    .map((l, i) =>
      /\[real-kernel\] init reaped child pid=\d+/.test(l) ? i : -1,
    )
    .filter((i) => i >= 0);

  expect(initStartIdx).toBeGreaterThanOrEqual(0);
  expect(initSpawnHelloStdIdx).toBeGreaterThan(initStartIdx);
  expect(initSpawnDisplayServerIdx).toBeGreaterThan(initSpawnHelloStdIdx);
  expect(initSpawnDisplayClientIdx).toBeGreaterThan(initSpawnDisplayServerIdx);
  // init's trailing line comes after every child's final print —
  // init's supervision loop is the last thing to finish.
  expect(initExitIdx).toBeGreaterThan(initSpawnDisplayClientIdx);
  // Children start only after init's proc_spawn returns; under
  // Playwright's concurrent-Worker scheduling the children's
  // starts interleave with init's supervision loop, so we only
  // assert their starts come after init's first spawn (not after
  // init's final "exiting" line).
  expect(helloStdIdx).toBeGreaterThan(initSpawnHelloStdIdx);
  expect(displayServerStartIdx).toBeGreaterThan(initSpawnDisplayServerIdx);
  expect(displayClientStartIdx).toBeGreaterThan(initSpawnDisplayClientIdx);
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
  // T095: init's SIGTERM to display-server fires after both
  // clients have been reaped — therefore AFTER both `sent pixels`
  // AND AFTER at least the first `served client` line (the
  // servers serve before the clients exit). Asserting `>` both
  // `sent pixels` indices and the first `served client` index
  // pins the ordering.
  expect(initSigtermIdx).toBeGreaterThanOrEqual(0);
  expect(initSigtermIdx).toBeGreaterThan(displayClientSentIndices[1]!);
  expect(initSigtermIdx).toBeGreaterThan(displayServerServedIndices[0]!);
  // T110: display-server's "fb blit ok" prints only after SIGTERM
  // breaks the accept loop. Restored observable (deferred in
  // slice 2a/2b).
  expect(fbBlitOkIdx).toBeGreaterThanOrEqual(0);
  expect(fbBlitOkIdx).toBeGreaterThan(initSigtermIdx);
  // T095: at least two `init reaped child pid=N` lines land
  // before `init exiting`. Exact count depends on Playwright
  // timing (children can exit before or during init's first
  // proc_wait), but the supervision loop must reap at least the
  // two clients before firing SIGTERM.
  expect(initReapedIndices.length).toBeGreaterThanOrEqual(2);
  for (const idx of initReapedIndices) {
    expect(idx).toBeLessThan(initExitIdx);
  }

  expect(consoleLines.some((l) => l.includes("real kernel ready"))).toBe(true);
  expect(consoleLines.some((l) => l.includes("real kernel panic"))).toBe(false);

  // DOM surface: real-kernel mode renders each captured line into a
  // `<pre id="pmos-real-console">` so the page itself shows the
  // boot output (not just dev tools). The element's text must
  // include every line all five pids produced.
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
  // T095/T110: the DOM also shows init's reap + SIGTERM lines and
  // display-server's fb-blit-ok trailing line.
  expect(domText).toMatch(/init reaped child pid=\d+/);
  expect(domText).toMatch(/init sent SIGTERM to display-server pid=\d+/);
  expect(domText).toContain("display-server fb blit ok");
  expect(domText.indexOf("init starting")).toBeLessThan(
    domText.indexOf("hello from std"),
  );
  expect(domText.indexOf("display-client-demo starting")).toBeLessThan(
    domText.indexOf("display-client-demo sent pixels"),
  );
  // T110: SIGTERM precedes fb blit ok on the DOM too.
  expect(
    domText.indexOf("init sent SIGTERM to display-server"),
  ).toBeLessThan(domText.indexOf("display-server fb blit ok"));

  // At least four concurrent pids (init + at least three of
  // hello-std / display-server / display-client-demo × 2) MUST
  // each live in their own user Worker under
  // `createSpawnRouter`'s management. `peakLiveWorkers` is the
  // high-water mark of `router.liveWorkers.size` across every
  // message the bootstrap's listener observes. The peak lands
  // between 4 and 5 depending on scheduling — under slow CI,
  // hello-std can sometimes self-terminate before init finishes
  // emitting the last proc_spawn, leaving the peak at 4 rather
  // than the ideal 5. The assertion requires >= 4 because that
  // is sufficient evidence that the substrate round-robins
  // across multiple per-pid SAB rings (Principle V physical
  // isolation); the exact high-water depends on scheduling in
  // ways that a test shouldn't pin. The kernel Worker is NOT
  // counted — only user Workers under the router's management.
  const peakAttr = await page
    .locator("body")
    .getAttribute("data-pmos-peak-live-workers");
  expect(peakAttr).not.toBeNull();
  expect(Number(peakAttr)).toBeGreaterThanOrEqual(4);

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
