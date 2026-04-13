// web/src/bootstrap.ts
var BOOT_VERSION = "0.1.0-demo";
function hasSharedArrayBuffer() {
  return typeof SharedArrayBuffer !== "undefined";
}
function hasAtomicsWait() {
  return typeof Atomics !== "undefined" && typeof Atomics.wait === "function";
}
function isCrossOriginIsolated() {
  return typeof crossOriginIsolated !== "undefined" && crossOriginIsolated;
}
function hasOpfs() {
  return typeof navigator !== "undefined" && typeof navigator.storage !== "undefined" && typeof navigator.storage.getDirectory === "function";
}
function hasServiceWorker() {
  return typeof navigator !== "undefined" && "serviceWorker" in navigator;
}
function hasOffscreenCanvas() {
  return typeof OffscreenCanvas !== "undefined";
}
var PALETTE = {
  bg: "#0a0e14",
  dim: "#1a1f26",
  fg: "#e6e6e6",
  accent: "#7cb7ff",
  ok: "#6ddf6d",
  warn: "#f2c045",
  fail: "#ff6b6b",
  muted: "#808591"
};
function setupCanvas() {
  const canvas = document.getElementById("pmos-fb");
  if (!canvas) {
    throw new Error("pmos-fb canvas element missing from index.html");
  }
  const dpr = window.devicePixelRatio || 1;
  const resize = () => {
    const w = Math.floor(window.innerWidth * dpr);
    const h = Math.floor(window.innerHeight * dpr);
    if (canvas.width !== w || canvas.height !== h) {
      canvas.width = w;
      canvas.height = h;
    }
  };
  resize();
  window.addEventListener("resize", resize);
  const ctx = canvas.getContext("2d");
  if (!ctx) {
    throw new Error("2D canvas context unavailable");
  }
  return { canvas, ctx, dpr };
}
function paintBoot(c, rows, animationFrame) {
  const { ctx, canvas, dpr } = c;
  const W = canvas.width;
  const H = canvas.height;
  ctx.fillStyle = PALETTE.bg;
  ctx.fillRect(0, 0, W, H);
  const padX = 48 * dpr;
  const padY = 48 * dpr;
  const lineHeight = 22 * dpr;
  const mono = `${14 * dpr}px ui-monospace, "SF Mono", Menlo, Consolas, monospace`;
  const monoBig = `bold ${20 * dpr}px ui-monospace, "SF Mono", Menlo, Consolas, monospace`;
  const monoSmall = `${12 * dpr}px ui-monospace, "SF Mono", Menlo, Consolas, monospace`;
  ctx.font = monoBig;
  ctx.fillStyle = PALETTE.accent;
  ctx.fillText(`PMos ${BOOT_VERSION}`, padX, padY);
  ctx.font = monoSmall;
  ctx.fillStyle = PALETTE.muted;
  ctx.fillText(
    "browser-hosted operating system \u2014 demo build",
    padX,
    padY + 18 * dpr
  );
  const rowsX = padX;
  const rowsY = padY + 70 * dpr;
  for (let i = 0; i < rows.length; i++) {
    const row = rows[i];
    const y = rowsY + i * lineHeight;
    let tag = "  --  ";
    let tagColor = PALETTE.muted;
    switch (row.status) {
      case "ok":
        tag = "  OK  ";
        tagColor = PALETTE.ok;
        break;
      case "fail":
        tag = " FAIL ";
        tagColor = PALETTE.fail;
        break;
      case "warn":
        tag = " WARN ";
        tagColor = PALETTE.warn;
        break;
      case "running":
        tag = ` ${"*".repeat(animationFrame % 3 + 1).padEnd(3, ".")}  `;
        tagColor = PALETTE.accent;
        break;
      case "stalled":
        tag = " WAIT ";
        tagColor = PALETTE.warn;
        break;
      case "pending":
      default:
        tag = "  --  ";
        tagColor = PALETTE.muted;
        break;
    }
    ctx.font = mono;
    ctx.fillStyle = PALETTE.muted;
    ctx.fillText("[", rowsX, y);
    ctx.fillStyle = tagColor;
    ctx.fillText(tag, rowsX + 10 * dpr, y);
    ctx.fillStyle = PALETTE.muted;
    ctx.fillText("]", rowsX + 70 * dpr, y);
    ctx.fillStyle = row.status === "fail" ? PALETTE.fail : row.status === "warn" || row.status === "stalled" ? PALETTE.warn : PALETTE.fg;
    ctx.fillText(row.label, rowsX + 90 * dpr, y);
    if (row.detail) {
      ctx.fillStyle = PALETTE.muted;
      ctx.fillText(row.detail, rowsX + 340 * dpr, y);
    }
  }
  const footerY = H - padY;
  ctx.font = monoSmall;
  ctx.fillStyle = PALETTE.muted;
  ctx.fillText(
    "This is the PMos boot-screen demo. The kernel WASM is not yet",
    padX,
    footerY - 3 * lineHeight
  );
  ctx.fillText(
    "compiled \u2014 reaching the desktop requires running `just build`",
    padX,
    footerY - 2 * lineHeight
  );
  ctx.fillText(
    "against the PMos source tree (Rust + Node + wasm32 target).",
    padX,
    footerY - 1 * lineHeight
  );
  ctx.fillText(
    "Source: https://github.com/example/pmos  \u2022  specs/001-browser-os-v1/",
    padX,
    footerY
  );
}
function main() {
  console.log(`[pmos-bootstrap] PMos ${BOOT_VERSION} starting`);
  const rows = [
    { label: "Cross-origin isolation (COOP/COEP)", status: "pending", detail: "" },
    { label: "SharedArrayBuffer", status: "pending", detail: "" },
    { label: "Atomics.wait", status: "pending", detail: "" },
    { label: "Origin Private Filesystem (OPFS)", status: "pending", detail: "" },
    { label: "Service worker", status: "pending", detail: "" },
    { label: "OffscreenCanvas", status: "pending", detail: "" },
    { label: "Kernel WASM load (/assets/kernel.wasm)", status: "pending", detail: "" },
    { label: "Display server", status: "pending", detail: "" },
    { label: "Desktop shell", status: "pending", detail: "" }
  ];
  let canvas;
  try {
    canvas = setupCanvas();
  } catch (e) {
    console.error("[pmos-bootstrap] cannot set up canvas:", e);
    showFallbackMessage(String(e));
    return;
  }
  let frame = 0;
  const repaint = () => {
    paintBoot(canvas, rows, frame++);
  };
  repaint();
  const step = (i, delay, fn) => {
    setTimeout(() => {
      rows[i].status = "running";
      repaint();
      setTimeout(() => {
        fn();
        repaint();
      }, 200);
    }, delay);
  };
  step(0, 300, () => {
    if (isCrossOriginIsolated()) {
      rows[0].status = "ok";
      rows[0].detail = "crossOriginIsolated === true";
    } else {
      rows[0].status = "fail";
      rows[0].detail = "COOP/COEP headers missing";
    }
  });
  step(1, 600, () => {
    if (hasSharedArrayBuffer()) {
      rows[1].status = "ok";
      rows[1].detail = "typeof SharedArrayBuffer === 'function'";
    } else {
      rows[1].status = "fail";
      rows[1].detail = "undefined";
    }
  });
  step(2, 900, () => {
    if (hasAtomicsWait()) {
      rows[2].status = "ok";
      rows[2].detail = "Atomics.wait available";
    } else {
      rows[2].status = "fail";
      rows[2].detail = "Atomics.wait missing";
    }
  });
  step(3, 1200, () => {
    if (hasOpfs()) {
      rows[3].status = "ok";
      rows[3].detail = "navigator.storage.getDirectory ok";
    } else {
      rows[3].status = "fail";
      rows[3].detail = "navigator.storage.getDirectory missing";
    }
  });
  step(4, 1500, () => {
    if (hasServiceWorker()) {
      rows[4].status = "ok";
      rows[4].detail = "navigator.serviceWorker present";
    } else {
      rows[4].status = "fail";
      rows[4].detail = "navigator.serviceWorker missing";
    }
  });
  step(5, 1800, () => {
    if (hasOffscreenCanvas()) {
      rows[5].status = "ok";
      rows[5].detail = "transfer-to-worker supported";
    } else {
      rows[5].status = "warn";
      rows[5].detail = "falling back to main-thread putImageData";
    }
  });
  step(6, 2200, () => {
    void attemptKernelFetch().then((result) => {
      if (result.ok) {
        rows[6].status = "ok";
        rows[6].detail = `${result.size} bytes`;
        rows[7].status = "stalled";
        rows[7].detail = "not wired in the demo";
        rows[8].status = "stalled";
        rows[8].detail = "not wired in the demo";
      } else {
        rows[6].status = "stalled";
        rows[6].detail = result.reason;
        rows[7].status = "pending";
        rows[8].status = "pending";
      }
      repaint();
    });
  });
  setInterval(repaint, 300);
  window.addEventListener("error", (event) => showPanic(event.message));
  window.addEventListener(
    "unhandledrejection",
    (event) => showPanic(String(event.reason))
  );
}
async function attemptKernelFetch() {
  try {
    const res = await fetch("/assets/kernel.wasm", { method: "HEAD" });
    if (!res.ok) {
      return { ok: false, reason: `HTTP ${res.status} \u2014 not yet built` };
    }
    const size = Number(res.headers.get("content-length") || "0");
    return { ok: true, size };
  } catch (e) {
    return { ok: false, reason: `fetch failed: ${String(e)}` };
  }
}
function showFallbackMessage(error) {
  document.body.innerHTML = `
    <div style="padding:2rem;font-family:ui-monospace,monospace;color:#e6e6e6;background:#0a0e14;height:100vh">
      <h1 style="color:#ff6b6b">PMos bootstrap failed</h1>
      <p>${escapeHtml(error)}</p>
      <p style="color:#808591">See devtools console for details.</p>
    </div>`;
}
function showPanic(message) {
  const panel = document.getElementById("pmos-panic");
  const msg = document.getElementById("pmos-panic-message");
  if (panel && msg) {
    msg.textContent = message;
    panel.style.display = "block";
  }
  let n = 5;
  const countdown = document.getElementById("pmos-panic-countdown");
  const tick = () => {
    if (countdown) countdown.textContent = String(n);
    if (n <= 0) {
      window.location.reload();
      return;
    }
    n--;
    setTimeout(tick, 1e3);
  };
  tick();
}
function escapeHtml(s) {
  return s.replace(/&/g, "&amp;").replace(/</g, "&lt;").replace(/>/g, "&gt;").replace(/"/g, "&quot;").replace(/'/g, "&#39;");
}
if (document.readyState === "loading") {
  document.addEventListener("DOMContentLoaded", main);
} else {
  main();
}
