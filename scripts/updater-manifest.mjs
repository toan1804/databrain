#!/usr/bin/env node
// Write the Tauri updater manifest (latest.json) for a release from the
// .sig files of its updater bundles in a folder (downloaded from the draft
// release). The app reads it from
// https://github.com/<repo>/releases/latest/download/latest.json.
//
// Usage: node scripts/updater-manifest.mjs <dir> <tag> <repo> [notes-file] > latest.json
//
// Bundles are named by the release workflow:
//   DataBrain_<ver>_<arch>[-no-duckdb].app.tar.gz   macOS   → darwin-<arch>[-no-duckdb]
//   DataBrain_<ver>_x64-setup.exe                   Windows → windows-x86_64
//   DataBrain_<ver>_amd64.AppImage                  Linux   → linux-x86_64
import { readFileSync, readdirSync } from "node:fs";
import { join } from "node:path";
import { fileURLToPath } from "node:url";

const ARCH = { aarch64: "aarch64", arm64: "aarch64", x64: "x86_64", x86_64: "x86_64", amd64: "x86_64" };

/** Updater key of a bundle file name, or null for other files. */
export function platformOf(name) {
  let m = /_(aarch64|arm64|x64|x86_64)(-no-duckdb)?\.app\.tar\.gz$/.exec(name);
  if (m) return `darwin-${ARCH[m[1]]}${m[2] ?? ""}`;
  m = /_(x64|amd64|x86_64)-setup\.exe$/.exec(name);
  if (m) return `windows-${ARCH[m[1]]}`;
  m = /_(amd64|x86_64)\.AppImage$/.exec(name);
  if (m) return `linux-${ARCH[m[1]]}`;
  return null;
}

/** `files`: names in the folder; each `<bundle>.sig` adds `<bundle>` (uploaded next to it). */
export function manifest(files, readSig, tag, repo, notes = "", date = new Date().toISOString()) {
  const platforms = {};
  for (const sig of files.filter((f) => f.endsWith(".sig"))) {
    const f = sig.slice(0, -4);
    const p = platformOf(f);
    if (!p) continue;
    platforms[p] = { signature: readSig(sig).trim(), url: `https://github.com/${repo}/releases/download/${tag}/${encodeURIComponent(f)}` };
  }
  if (!Object.keys(platforms).length) throw new Error("no signed updater bundles found");
  return { version: tag.replace(/^v/, ""), notes, pub_date: date, platforms };
}

if (process.argv[1] && fileURLToPath(import.meta.url) === process.argv[1]) {
  const [dir, tag, repo, notesFile] = process.argv.slice(2);
  if (!dir || !tag || !repo) {
    console.error("usage: updater-manifest.mjs <dir> <tag> <repo> [notes-file]");
    process.exit(2);
  }
  const notes = notesFile ? readFileSync(notesFile, "utf8") : "";
  const m = manifest(readdirSync(dir), (f) => readFileSync(join(dir, f), "utf8"), tag, repo, notes);
  process.stdout.write(JSON.stringify(m, null, 2) + "\n");
  console.error(`latest.json: ${Object.keys(m.platforms).join(", ")}`);
}
