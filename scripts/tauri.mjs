#!/usr/bin/env node
// `npm run tauri -- <args>` on every OS: runs the Tauri CLI with the app in
// crates/app and the frontend in ui (the `VAR=value cmd` form of npm
// scripts doesn't work in Windows shells).
import { spawnSync } from "node:child_process";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";

const root = join(dirname(fileURLToPath(import.meta.url)), "..");
const env = { ...process.env, TAURI_APP_PATH: join(root, "crates", "app"), TAURI_FRONTEND_PATH: join(root, "ui") };
const r = spawnSync("npx", ["tauri", ...process.argv.slice(2)], { cwd: join(root, "ui"), env, stdio: "inherit", shell: process.platform === "win32" });
process.exit(r.status ?? 1);
