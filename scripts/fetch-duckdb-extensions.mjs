#!/usr/bin/env node
// Download DuckDB's signed extension binaries (Excel, Delta, Iceberg, …) for
// the build target into crates/app/resources/duckdb-extensions, so the app
// bundle needs no download at runtime. Parquet and JSON are compiled into
// DuckDB itself (Cargo features) and are not fetched.
//
// Usage: node scripts/fetch-duckdb-extensions.mjs [--target <rust triple>]
// The DuckDB version is read from Cargo.lock (libduckdb-sys 1.MMmmpp.x → v1.mm.pp).
// Already present, non-empty files are kept.

import { existsSync, mkdirSync, readFileSync, renameSync, statSync, writeFileSync } from "node:fs";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";
import { gunzipSync } from "node:zlib";

export const EXTENSIONS = ["excel", "delta", "iceberg", "avro", "httpfs", "icu"];

const root = join(dirname(fileURLToPath(import.meta.url)), "..");

/** libduckdb-sys "1.10506.0" → "v1.5.6" (crate minor = 1MMPP: minor MM, patch PP). */
export function duckdbVersion(cargoLock) {
  const m = /name = "libduckdb-sys"\s*\nversion = "(\d+)\.(\d+)\.\d+"/.exec(cargoLock);
  if (!m || m[2].length < 5) throw new Error("libduckdb-sys not found in Cargo.lock");
  const packed = m[2];
  return `v${m[1]}.${Number(packed.slice(-4, -2))}.${Number(packed.slice(-2))}`;
}

/** Rust target triple → DuckDB platform name. */
export function duckdbPlatform(triple) {
  const t = triple ?? `${process.arch}-${process.platform}`;
  if (/aarch64-apple|arm64-darwin/.test(t)) return "osx_arm64";
  if (/x86_64-apple|x64-darwin/.test(t)) return "osx_amd64";
  if (/x86_64-pc-windows|x64-win32/.test(t)) return "windows_amd64";
  if (/aarch64-pc-windows|arm64-win32/.test(t)) return "windows_arm64";
  if (/aarch64-unknown-linux|arm64-linux/.test(t)) return "linux_arm64";
  if (/x86_64-unknown-linux|x64-linux/.test(t)) return "linux_amd64";
  throw new Error(`no DuckDB extensions for target ${t}`);
}

async function main() {
  const i = process.argv.indexOf("--target");
  const triple = i > 0 ? process.argv[i + 1] : process.env.TAURI_ENV_TARGET_TRIPLE || undefined;
  const version = duckdbVersion(readFileSync(join(root, "Cargo.lock"), "utf8"));
  const platform = duckdbPlatform(triple);
  const dir = join(root, "crates/app/resources/duckdb-extensions", version, platform);
  mkdirSync(dir, { recursive: true });
  for (const ext of EXTENSIONS) {
    const file = join(dir, `${ext}.duckdb_extension`);
    if (existsSync(file) && statSync(file).size > 0) {
      console.log(`✓ ${ext} (cached)`);
      continue;
    }
    const url = `https://extensions.duckdb.org/${version}/${platform}/${ext}.duckdb_extension.gz`;
    const res = await fetch(url);
    if (!res.ok) throw new Error(`${url}: HTTP ${res.status}`);
    const data = gunzipSync(Buffer.from(await res.arrayBuffer()));
    writeFileSync(`${file}.tmp`, data);
    renameSync(`${file}.tmp`, file);
    console.log(`↓ ${ext} ${(data.length / 1e6).toFixed(1)} MB`);
  }
  console.log(`DuckDB ${version} extensions for ${platform} in ${dir}`);
}

if (process.argv[1] && fileURLToPath(import.meta.url) === process.argv[1]) {
  main().catch((e) => {
    console.error(`fetch-duckdb-extensions: ${e.message}`);
    process.exit(1);
  });
}
