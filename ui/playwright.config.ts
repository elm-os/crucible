import { defineConfig } from "@playwright/test";

const PORT = 4173;
const ci = Boolean(process.env.CI);

// Runs against the static export (`pnpm build` first). Every /api/v1 request is answered by the test.
export default defineConfig({
  testDir: "./e2e",
  fullyParallel: true,
  forbidOnly: ci,
  workers: ci ? 2 : undefined,
  reporter: ci ? [["list"], ["html", { open: "never" }]] : "list",
  use: {
    baseURL: `http://127.0.0.1:${PORT}`,
    trace: "retain-on-failure",
  },
  projects: [
    {
      name: "desktop",
      testIgnore: /phone\.spec\.ts/,
      use: { browserName: "chromium", viewport: { width: 1280, height: 900 } },
    },
    {
      name: "phone",
      testMatch: /phone\.spec\.ts/,
      use: {
        browserName: "chromium",
        viewport: { width: 390, height: 844 },
        deviceScaleFactor: 3,
        isMobile: true,
        hasTouch: true,
      },
    },
  ],
  webServer: {
    command: `node e2e/serve.mjs --port ${PORT}`,
    url: `http://127.0.0.1:${PORT}/home/`,
    reuseExistingServer: !ci,
  },
});
