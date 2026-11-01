import { defineConfig } from "@playwright/test";
import base from "./playwright.config";

// README screenshots (e2e/screenshots): `npm run screenshots` writes them
// to docs/screenshots/. Not part of `npm run e2e`.
export default defineConfig({
  ...base,
  testDir: "e2e/screenshots",
  testIgnore: [],
  retries: 0,
  fullyParallel: false,
});
