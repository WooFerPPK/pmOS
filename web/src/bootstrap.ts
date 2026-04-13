// PMos bootstrap.
//
// This is the first JS to run in the browser. Its job is:
//
//   1. Verify cross-origin isolation (COOP/COEP must be active so
//      that SharedArrayBuffer is available for the syscall transport).
//   2. Register the service worker (precaches the entire OS for
//      offline subsequent loads, per FR-016).
//   3. Instantiate each TypeScript driver (framebuffer, input, block,
//      net, console) and hand MessagePorts to the kernel Worker.
//   4. Spawn the kernel Worker and hand it the driver ports, the
//      canvas OffscreenCanvas (where supported), and a fresh SAB for
//      the syscall transport.
//   5. Install a top-level error handler that paints the kernel panic
//      overlay and auto-reloads after ~5 s (FR-009a).
//
// The actual implementation of steps 2..5 lands in T085 (Phase 2).
// This file is the Phase 1 skeleton — it verifies cross-origin
// isolation and logs the result to the devtools console, so
// `just dev` has a demonstrable success signal.

const OK = "[pmos-bootstrap]";

function main(): void {
  console.log(`${OK} starting`);

  if (typeof crossOriginIsolated === "undefined" || !crossOriginIsolated) {
    console.error(
      `${OK} crossOriginIsolated === false — COOP/COEP headers missing. ` +
        "SharedArrayBuffer will not be available. See specs/001-browser-os-v1/contracts/driver-kernel.md §1.",
    );
    showFatal(
      "This browser tab is not cross-origin-isolated. " +
        "The COOP/COEP headers are required for PMos to boot. " +
        "See quickstart.md §8 for the deployment configuration.",
    );
    return;
  }

  console.log(`${OK} crossOriginIsolated === true`);
  console.log(`${OK} SharedArrayBuffer:`, typeof SharedArrayBuffer);
  console.log(`${OK} Atomics.waitAsync:`, typeof Atomics.waitAsync);

  // T085 will replace this with:
  //   - service worker registration (T087)
  //   - driver instantiation (T079..T089)
  //   - kernel Worker spawn (T090..T091)
  //   - panic overlay wiring (T094)
  console.log(`${OK} Phase 1 skeleton — rest of boot lands in T085`);
}

function showFatal(message: string): void {
  const panel = document.getElementById("pmos-panic");
  const msg = document.getElementById("pmos-panic-message");
  const countdown = document.getElementById("pmos-panic-countdown");
  if (panel && msg) {
    msg.textContent = message;
    panel.style.display = "block";
  }
  if (countdown) {
    countdown.textContent = "—";
  }
}

main();
