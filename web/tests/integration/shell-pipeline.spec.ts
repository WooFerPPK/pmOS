import { expect, test } from "@playwright/test";

import {
  bootDesktop,
  launchTerminal,
  runTerminalCommand,
} from "./guest-terminal";

test.use({ viewport: { width: 1280, height: 900 } });

test("Terminal runs an isolated external pipeline and reaps every stage", async ({
  page,
}) => {
  const consoleLines: string[] = [];
  page.on("console", (message) => consoleLines.push(message.text()));
  page.on("pageerror", (error) =>
    consoleLines.push(`[pageerror] ${error.message}`),
  );

  await bootDesktop(page, consoleLines, 10_000);
  await launchTerminal(page, consoleLines, { timeout: 5_000 });

  const steadyWorkers = Number(
    (await page.locator("body").getAttribute("data-pmos-live-workers")) ?? "0",
  );
  expect(steadyWorkers).toBeGreaterThan(0);

  try {
    await runTerminalCommand(
      page,
      consoleLines,
      "echo m3-pipeline-ok | grep m3-pipeline > /dev/console",
      (line) => line === "[real-kernel] m3-pipeline-ok",
      5_000,
    );
  } catch (error) {
    throw new Error(
      `pipeline output missing; console follows:\n${consoleLines.join("\n")}`,
      { cause: error },
    );
  }
  await expect
    .poll(
      async () =>
        Number(
          (await page.locator("body").getAttribute("data-pmos-live-workers")) ??
            "0",
        ),
      { timeout: 5_000 },
    )
    .toBe(steadyWorkers);
  expect(consoleLines.filter((line) => line.startsWith("[pageerror]"))).toEqual(
    [],
  );
});
