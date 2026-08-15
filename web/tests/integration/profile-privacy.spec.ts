// T140 — profile privacy gate. Context A creates and durably flushes a real
// guest file. Context B lists its own `/home/user` and must not see it, while a
// fresh page in A's original storage partition must read the exact contents.

import { expect, test } from "@playwright/test";

import {
  bootDesktop,
  launchTerminal,
  runTerminalCommand,
  runTerminalCommandToPrompt,
} from "./guest-terminal";

test.use({ viewport: { width: 1280, height: 900 } });
test.setTimeout(60_000);

test("browser storage partitions cannot observe each other's guest files", async ({
  browser,
  browserName,
}) => {
  test.skip(
    browserName === "webkit",
    "WebKit lacks the persistent OPFS substrate required by the v1 privacy gate.",
  );

  const contextA = await browser.newContext();
  const pageA = await contextA.newPage();
  const linesA: string[] = [];
  pageA.on("console", (message) => linesA.push(message.text()));
  pageA.on("pageerror", (error) =>
    linesA.push(`[pageerror] ${error.message}`),
  );
  await bootDesktop(pageA, linesA);
  await launchTerminal(pageA, linesA);
  await runTerminalCommandToPrompt(
    pageA,
    linesA,
    "echo profile-a-secret-content > /home/user/profile-a-secret.txt",
  );
  await runTerminalCommandToPrompt(
    pageA,
    linesA,
    "cp /home/user/profile-a-secret.txt /home/user/.profile-a-durable",
  );
  await runTerminalCommand(
    pageA,
    linesA,
    "echo profile-a-write-durable > /dev/console",
    (line) => line.includes("profile-a-write-durable"),
  );

  const contextB = await browser.newContext();
  const pageB = await contextB.newPage();
  const linesB: string[] = [];
  pageB.on("console", (message) => linesB.push(message.text()));
  pageB.on("pageerror", (error) =>
    linesB.push(`[pageerror] ${error.message}`),
  );
  await bootDesktop(pageB, linesB);
  await launchTerminal(pageB, linesB);
  const listingStart = linesB.length;
  await runTerminalCommand(
    pageB,
    linesB,
    "ls /home/user > /dev/console",
    (line) => line.includes("Documents"),
  );
  const listingEnd = linesB.length;
  expect(
    linesB
      .slice(listingStart, listingEnd)
      .some((line) => line.includes("profile-a-secret.txt")),
  ).toBe(false);

  await pageA.close();
  const pageA2 = await contextA.newPage();
  const linesA2: string[] = [];
  pageA2.on("console", (message) => linesA2.push(message.text()));
  pageA2.on("pageerror", (error) =>
    linesA2.push(`[pageerror] ${error.message}`),
  );
  await bootDesktop(pageA2, linesA2);
  await launchTerminal(pageA2, linesA2);
  const content = await runTerminalCommand(
    pageA2,
    linesA2,
    "cat /home/user/profile-a-secret.txt > /dev/console",
    (line) => line.includes("profile-a-secret-content"),
  );
  expect(content).toContain("profile-a-secret-content");
  const allLines = [...linesA, ...linesB, ...linesA2];
  expect(
    allLines.some((line) => line.includes("user worker crashed pid=")),
    `unexpected user Worker crash:\n${allLines
      .filter((line) => line.includes("user worker crashed pid="))
      .join("\n")}`,
  ).toBe(false);
  expect(allLines.some((line) => line.startsWith("[pageerror]"))).toBe(false);

  await contextA.close();
  await contextB.close();
});
