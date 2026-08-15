// T139 — canonical browser-level persistent-root gate. A real Terminal writes
// a user file through the guest VFS, an external child exit supplies the
// durability barrier, and a fresh kernel Worker/WASM instance reads the exact
// bytes back from the remounted OPFS root.

import { expect, test } from "@playwright/test";

import {
  bootDesktop,
  launchTerminal,
  runTerminalCommand,
  runTerminalCommandToPrompt,
} from "./guest-terminal";

test.use({ viewport: { width: 1280, height: 900 } });

test("a guest-created file survives a fresh page and kernel instance", async ({
  context,
  page,
  browserName,
}) => {
  test.skip(
    browserName === "webkit",
    "WebKit lacks the persistent OPFS substrate required by the v1 persistence gate.",
  );

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
    "mkdir -p /home/user/notes",
  );
  await runTerminalCommandToPrompt(
    page,
    firstLines,
    "echo hello-pmos-persistence > /home/user/notes/.hi.tmp",
  );
  await runTerminalCommandToPrompt(
    page,
    firstLines,
    "cp /home/user/notes/.hi.tmp /home/user/notes/hi.txt",
  );
  await runTerminalCommand(
    page,
    firstLines,
    "echo persistence-write-durable > /dev/console",
    (line) => line.includes("persistence-write-durable"),
  );

  await page.close();
  const secondLines: string[] = [];
  const secondPage = await context.newPage();
  secondPage.on("console", (message) => secondLines.push(message.text()));
  secondPage.on("pageerror", (error) =>
    secondLines.push(`[pageerror] ${error.message}`),
  );
  await bootDesktop(secondPage, secondLines);
  await launchTerminal(secondPage, secondLines);
  const read = await runTerminalCommand(
    secondPage,
    secondLines,
    "cat /home/user/notes/hi.txt > /dev/console",
    (line) => line.includes("hello-pmos-persistence"),
  );
  expect(read).toContain("hello-pmos-persistence");
  const allLines = [...firstLines, ...secondLines];
  expect(allLines.some((line) => line.includes("real kernel panic"))).toBe(false);
  expect(
    allLines.some((line) => line.includes("user worker crashed pid=")),
  ).toBe(false);
  expect(allLines.some((line) => line.startsWith("[pageerror]"))).toBe(false);
});
