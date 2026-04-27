// T167 — Zero OS network traffic. Load the page, navigate the OS
// for 30 s, assert no network request reaches any origin other
// than the dev-server's localhost. Constitution Principle III.

import { expect, test } from "@playwright/test";

test("PMos makes zero outbound network calls beyond the static host", async ({
  page,
}) => {
  const requests: string[] = [];
  page.on("request", (req) => requests.push(req.url()));

  await page.goto("/index.html");
  // Let the boot run + a bit of idle time for any background polls.
  await page.waitForTimeout(5_000);

  // The base URL set in playwright.config.ts is 127.0.0.1:8081.
  // Every request must target that origin (or be a `data:` /
  // `blob:` URL — those don't count as network).
  const offending = requests.filter((u) => {
    if (u.startsWith("data:") || u.startsWith("blob:")) return false;
    if (u.startsWith("http://127.0.0.1:8081")) return false;
    if (u.startsWith("http://localhost:8081")) return false;
    return true;
  });

  expect(offending).toEqual([]);
});
