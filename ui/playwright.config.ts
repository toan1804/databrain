import { defineConfig, devices } from "@playwright/test";

// Smoke tests of the UI in Chromium against a mocked Tauri backend
// (e2e/tauri-mock.js): `npm run e2e`. The Vite dev server is started on a
// port of its own so it doesn't clash with `tauri dev` (1420).
const PORT = 1431;

export default defineConfig({
  testDir: "e2e",
  // README screenshots have their own config (playwright.screenshots.config.ts).
  testIgnore: "screenshots/**",
  timeout: 30_000,
  expect: { timeout: 7_000 },
  fullyParallel: true,
  forbidOnly: !!process.env.CI,
  retries: process.env.CI ? 1 : 0,
  reporter: process.env.CI ? [["list"], ["html", { open: "never" }]] : "list",
  use: {
    baseURL: `http://localhost:${PORT}`,
    viewport: { width: 1400, height: 880 },
    trace: "retain-on-failure",
  },
  projects: [{ name: "chromium", use: { ...devices["Desktop Chrome"], viewport: { width: 1400, height: 880 } } }],
  webServer: {
    command: `npx vite --port ${PORT} --strictPort`,
    url: `http://localhost:${PORT}`,
    reuseExistingServer: !process.env.CI,
    timeout: 60_000,
  },
});
