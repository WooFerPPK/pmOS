// T175 — Principle V browser gate. Launch the shipped adversarial program as
// an ordinary app through the real desktop shell. The probe runs in its own
// Worker/WASM instance and exercises eight cross-process/capability escape
// attempts; every one must be rejected by the production kernel boundary.

import { expect, test } from "@playwright/test";

import {
  bootDesktop,
  launchTerminal,
  runTerminalCommand,
  submitTerminalCommand,
} from "./guest-terminal";

test.use({ viewport: { width: 1280, height: 900 } });

test("an ordinary isolated Worker rejects every adversarial memory probe", async ({
  page,
  browserName,
}) => {
  test.skip(
    browserName === "webkit",
    "WebKit lacks the persistent OPFS substrate required by the supported desktop boot.",
  );

  const lines: string[] = [];
  page.on("console", (message) => lines.push(message.text()));
  page.on("pageerror", (error) => lines.push(`[pageerror] ${error.message}`));

  await bootDesktop(page, lines);
  await launchTerminal(page, lines);

  const steadyWorkers = Number(
    (await page.locator("body").getAttribute("data-pmos-live-workers")) ?? "0",
  );
  const evidenceStart = lines.length;
  await submitTerminalCommand(page, "/bin/mem_adversary > /dev/console");
  const status = await runTerminalCommand(
    page,
    lines,
    "echo mem-adversary-status-$? > /dev/console",
    (line) => line.includes("mem-adversary-status-"),
  );

  const evidence = lines.slice(evidenceStart);
  expect(status).toContain("mem-adversary-status-0");
  expect(
    evidence.some((line) => line.includes("mem-adversary OK")),
    `adversary did not report success:\n${evidence.join("\n")}`,
  ).toBe(true);
  expect(evidence.filter((line) => line.includes("PASS "))).toHaveLength(8);
  expect(evidence.some((line) => line.includes("BREACH"))).toBe(false);
  await expect
    .poll(
      async () =>
        Number(
          (await page
            .locator("body")
            .getAttribute("data-pmos-live-workers")) ?? "0",
        ),
      { timeout: 10_000 },
    )
    .toBe(steadyWorkers);
  expect(lines.some((line) => line.includes("real kernel panic"))).toBe(false);
  expect(lines.some((line) => line.includes("user worker crashed pid="))).toBe(
    false,
  );
  expect(lines.some((line) => line.startsWith("[pageerror]"))).toBe(false);
});
