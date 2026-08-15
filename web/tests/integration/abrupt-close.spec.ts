// T141 — abrupt-close consistency gate. The test durably writes a baseline
// guest file, starts a large streaming guest write, observes its external `head`
// Worker still live, then terminates the actual kernel Worker before pagehide
// can synchronize it and closes without beforeunload. A fresh kernel instance must remount the same OPFS
// image, preserve the flushed baseline, and read unrelated directory state
// regardless of how much of the interrupted file transaction committed.

import { expect, test } from "@playwright/test";

import {
  bootDesktop,
  launchTerminal,
  runTerminalCommand,
  runTerminalCommandToPrompt,
  submitTerminalCommand,
} from "./guest-terminal";

test.use({ viewport: { width: 1280, height: 900 } });
test.setTimeout(60_000);

test("an abruptly terminated kernel remounts a consistent OPFS image", async ({
  context,
  page,
  browserName,
}) => {
  test.skip(
    browserName === "webkit",
    "WebKit lacks the persistent OPFS substrate required by the v1 crash-consistency gate.",
  );

  await page.addInitScript(() => {
    const NativeWorker = window.Worker;
    class TrackedWorker extends NativeWorker {
      constructor(scriptURL: string | URL, options?: WorkerOptions) {
        super(scriptURL, options);
        if (String(scriptURL).includes("kernel-worker")) {
          Object.defineProperty(window, "__pmosKernelWorker", {
            configurable: true,
            value: this,
          });
        }
      }
    }
    Object.defineProperty(window, "Worker", {
      configurable: true,
      value: TrackedWorker,
    });
  });

  const firstLines: string[] = [];
  page.on("console", (message) => firstLines.push(message.text()));
  page.on("pageerror", (error) =>
    firstLines.push(`[pageerror] ${error.message}`),
  );
  await bootDesktop(page, firstLines);
  await launchTerminal(page, firstLines);
  await runTerminalCommandToPrompt(
    page,
    firstLines,
    "echo durable-before-abrupt-stop > /home/user/abrupt-durable.txt",
  );
  await runTerminalCommandToPrompt(
    page,
    firstLines,
    "cp /home/user/abrupt-durable.txt /home/user/.abrupt-flush-barrier",
  );
  await runTerminalCommand(
    page,
    firstLines,
    "echo abrupt-baseline-durable > /dev/console",
    (line) => line.includes("abrupt-baseline-durable"),
  );

  const workersBeforeWrite = Number(
    (await page.locator("body").getAttribute("data-pmos-live-workers")) ?? "0",
  );
  await submitTerminalCommand(
    page,
    "head -c 1000000000 /dev/zero > /home/user/abrupt-inflight.bin",
  );
  await expect
    .poll(
      async () =>
        Number(
          (await page
            .locator("body")
            .getAttribute("data-pmos-live-workers")) ?? "0",
        ),
      {
        timeout: 5_000,
        message: "the continuous writer never entered its isolated Worker",
      },
    )
    .toBe(workersBeforeWrite + 1);

  const terminated = await page.evaluate(() => {
    const worker = (
      window as unknown as { __pmosKernelWorker?: Worker }
    ).__pmosKernelWorker;
    if (worker === undefined) return false;
    worker.terminate();
    return true;
  });
  expect(terminated).toBe(true);
  await page.close({ runBeforeUnload: false });

  const secondPage = await context.newPage();
  const secondLines: string[] = [];
  secondPage.on("console", (message) => secondLines.push(message.text()));
  secondPage.on("pageerror", (error) =>
    secondLines.push(`[pageerror] ${error.message}`),
  );
  await bootDesktop(secondPage, secondLines);
  await launchTerminal(secondPage, secondLines);
  const durableRead = await runTerminalCommand(
    secondPage,
    secondLines,
    "cat /home/user/abrupt-durable.txt > /dev/console",
    (line) => line.includes("durable-before-abrupt-stop"),
  );
  expect(durableRead).toContain("durable-before-abrupt-stop");
  await runTerminalCommand(
    secondPage,
    secondLines,
    "ls /home/user > /dev/console",
    (line) => line.includes("Documents"),
  );
  const allLines = [...firstLines, ...secondLines];
  expect(allLines.some((line) => line.includes("real kernel panic"))).toBe(false);
  expect(
    allLines.some((line) => line.includes("user worker crashed pid=")),
  ).toBe(false);
  expect(allLines.some((line) => line.startsWith("[pageerror]"))).toBe(false);
});
