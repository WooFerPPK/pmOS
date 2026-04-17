// Vitest configuration for the PMos TS unit tests.
//
// Restricts the test discovery to `tests/unit/`. The
// `tests/integration/` tree contains Playwright specs that import
// `@playwright/test` — Vitest will happily walk into them on the
// default glob and then fail at module-load time because Playwright
// rejects `test()` calls outside its own runner.
//
// Justfile target `test-drivers` runs `npx vitest run`; the
// `test-integration` target runs `npx playwright test` against the
// integration tree.

import { defineConfig } from "vitest/config";

export default defineConfig({
  test: {
    include: ["tests/unit/**/*.test.ts"],
  },
});
