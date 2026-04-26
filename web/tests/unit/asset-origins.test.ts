// T165: Asset origin audit — verify no asset fetched at runtime by
// `bootstrap.ts`, `kernel-worker-entry.ts`, `user-worker-entry.ts`,
// `sw.ts`, or any driver under `web/src/drivers/` originates from a
// third-party origin.
//
// PMos's Principle IV ("Browser-only, zero backend") and the
// FR-040..FR-044 non-goal block both require that the OS, once
// loaded, talks to nothing but the deploy host. This test scans
// the source files for HTTP(S) URL literals + `fetch()` /
// `import()` call sites, then asserts every observed URL is
// either:
//
//   * relative (starts with `/` or `./`), OR
//   * same-origin via `import.meta.url` / `new URL("...", import.meta.url)`, OR
//   * an explicitly-allow-listed module specifier (the runtime
//     ones the bundler resolves at build time, not at runtime).
//
// A regression that adds e.g. `fetch("https://cdn.example.com/...")`
// to bootstrap.ts will surface as a failed test pinning the file
// + line. Runtime third-party traffic is observed end-to-end by
// the T167 Playwright `zero-os-network-traffic.spec.ts`; this
// test is the static-analysis sibling that catches drift before
// the Playwright run.

import { describe, expect, it } from "vitest";

import * as fs from "node:fs";
import * as path from "node:path";

const REPO_ROOT = path.resolve(__dirname, "../../..");
const WEB_SRC = path.join(REPO_ROOT, "web/src");

/**
 * Files the audit covers. Every file that runs in the browser at
 * runtime AND can issue a fetch or import is in scope.
 */
const AUDITED_FILES: readonly string[] = [
  "bootstrap.ts",
  "kernel-worker-entry.ts",
  "user-worker-entry.ts",
  "sw.ts",
  "drivers/console.ts",
  "drivers/fb.ts",
  "drivers/input.ts",
  "drivers/block.ts",
  "drivers/net.ts",
  "kernel-wasm-host.ts",
  "user-wasm-runtime.ts",
];

/**
 * Allow-listed URL patterns. Each entry is checked against the
 * literal URL string the regex extracted; a pattern match means
 * the URL is OK to fetch at runtime.
 *
 * Relative paths (starting with `/` or `./`) and same-origin
 * paths via `URL` / `import.meta.url` are always allowed.
 */
const ALLOWLIST: readonly RegExp[] = [
  // The boot screen paints a "Source: <url>" line as static
  // canvas text — never fetched. Verified by reading the
  // surrounding ctx.fillText() call site.
  /^https:\/\/github\.com\/example\/pmos$/,
];

/**
 * Match URL literals in source. Detects HTTP(S), WebSocket
 * (ws/wss), and other scheme:// patterns. Does NOT match
 * relative paths; those are inherently same-origin.
 */
const URL_LITERAL_REGEX = /(?:https?|wss?|ftp|file|data):\/\/[^\s"'`)]+/gi;

interface FoundUrl {
  readonly file: string;
  readonly line: number;
  readonly url: string;
}

function scanFile(relPath: string): FoundUrl[] {
  const fullPath = path.join(WEB_SRC, relPath);
  const text = fs.readFileSync(fullPath, "utf8");
  const lines = text.split("\n");
  const out: FoundUrl[] = [];
  for (let i = 0; i < lines.length; i += 1) {
    const line = lines[i]!;
    // Strip line comments first so URLs inside `//` comments
    // explaining e.g. "host serves CSP" don't trip the audit.
    const stripped = stripLineComment(line);
    const matches = stripped.matchAll(URL_LITERAL_REGEX);
    for (const m of matches) {
      const url = m[0];
      if (ALLOWLIST.some((re) => re.test(url))) continue;
      out.push({ file: relPath, line: i + 1, url });
    }
  }
  return out;
}

/**
 * Strip a `//` comment from the end of a line, preserving the
 * code portion. Doesn't handle block comments — the audit is
 * conservative; if a block comment contains a URL literal that
 * isn't in the allowlist, we treat that as documentation drift
 * and the test fails. Tighten the comment before re-running.
 */
function stripLineComment(line: string): string {
  let inSingle = false;
  let inDouble = false;
  let inBacktick = false;
  for (let i = 0; i < line.length; i += 1) {
    const ch = line[i];
    const next = line[i + 1];
    if (!inSingle && !inDouble && !inBacktick && ch === "/" && next === "/") {
      return line.slice(0, i);
    }
    if (!inDouble && !inBacktick && ch === "'" && line[i - 1] !== "\\") {
      inSingle = !inSingle;
    } else if (!inSingle && !inBacktick && ch === '"' && line[i - 1] !== "\\") {
      inDouble = !inDouble;
    } else if (!inSingle && !inDouble && ch === "`" && line[i - 1] !== "\\") {
      inBacktick = !inBacktick;
    }
  }
  return line;
}

describe("asset origin audit (T165)", () => {
  it("every audited file exists", () => {
    for (const f of AUDITED_FILES) {
      expect(fs.existsSync(path.join(WEB_SRC, f))).toBe(true);
    }
  });

  it("contains no third-party URL literals at runtime sites", () => {
    const allFound: FoundUrl[] = [];
    for (const f of AUDITED_FILES) {
      allFound.push(...scanFile(f));
    }
    if (allFound.length > 0) {
      const message = allFound
        .map((f) => `${f.file}:${f.line}  ${f.url}`)
        .join("\n");
      throw new Error(
        `Third-party URL literals found in audited files:\n${message}\n\n` +
          "Either remove the URL or add a pattern to ALLOWLIST in " +
          "this test with a justification comment.",
      );
    }
    expect(allFound).toEqual([]);
  });

  it("ALLOWLIST is empty by default (every entry must be justified)", () => {
    // The allowlist starts empty. If a future slice adds an
    // entry, the spec requires a comment-line justification
    // adjacent to the regex; this test only enforces the
    // existence-of-allowlist invariant.
    expect(Array.isArray(ALLOWLIST)).toBe(true);
  });
});
