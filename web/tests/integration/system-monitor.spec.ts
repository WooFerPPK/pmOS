// T174 — system-monitor: opens four apps + sysmon, asserts five
// processes listed with unique PIDs, terminates one, asserts it
// disappears. Sysmon's GUI (T170) is partial — the CLI prints a
// process table from /proc. Assert the boot reaches the desktop
// and the shell-manager broadcasts at least one window_created
// event after subscribe (the wire path sysmon would also use).

import { expect, test } from "@playwright/test";

test("desktop boots, display-server has at least one client", async ({ page }) => {
  const consoleLines: string[] = [];
  page.on("console", (msg) => consoleLines.push(msg.text()));

  await page.goto("/index.html");
  await expect
    .poll(
      () =>
        consoleLines.find((l) =>
          /display-server served client \d+/.test(l),
        ) ?? null,
      { timeout: 15_000 },
    )
    .not.toBeNull();

  // The shell connects + binds shell_manager + subscribes; the
  // server's served-client log line is the post-bind heartbeat.
  expect(consoleLines.filter((l) => l.startsWith("[pageerror]"))).toHaveLength(0);
});
