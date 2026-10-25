#!/usr/bin/env node
// DataBrain without the bundled DuckDB engine: it uses the DuckDB library
// installed on the computer (libduckdb.dylib / libduckdb.so / duckdb.dll),
// loaded at runtime, and doesn't ship DuckDB's extensions (DuckDB's own
// extension folder is used). The app starts without DuckDB and shows how
// to install it.
//
// The `duckdb` crate is built against libduckdb-sys without `bundled`; its
// C functions come from crates/duckdb-dynload. libduckdb-sys still asks the
// linker for a library, so it gets an empty static one.
//
// Usage:
//   node scripts/build-system-duckdb.mjs            # release bundle (tauri build)
//   node scripts/build-system-duckdb.mjs --dev      # tauri dev
//   node scripts/build-system-duckdb.mjs --cargo test -p databrain-app   # any cargo command
import { execFileSync, spawnSync } from "node:child_process";
import { mkdirSync, writeFileSync, existsSync, renameSync, readdirSync } from "node:fs";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";

const root = join(dirname(fileURLToPath(import.meta.url)), "..");
const stub = join(root, "target", "duckdb-stub");
const win = process.platform === "win32";

function makeStub() {
  mkdirSync(stub, { recursive: true });
  const lib = join(stub, win ? "duckdb_static.lib" : "libduckdb_static.a");
  if (existsSync(lib)) return;
  const c = join(stub, "empty.c");
  writeFileSync(c, "/* intentionally empty: DuckDB is loaded at runtime */\nint databrain_duckdb_stub;\n");
  if (win) {
    execFileSync("cl", ["/nologo", "/c", c, `/Fo${join(stub, "empty.obj")}`], { stdio: "inherit" });
    execFileSync("lib", ["/nologo", `/OUT:${lib}`, join(stub, "empty.obj")], { stdio: "inherit" });
  } else {
    execFileSync("cc", ["-c", c, "-o", join(stub, "empty.o")], { stdio: "inherit" });
    execFileSync("ar", ["rcs", lib, join(stub, "empty.o")], { stdio: "inherit" });
  }
}

makeStub();
const env = {
  ...process.env,
  DUCKDB_LIB_DIR: stub,
  DUCKDB_STATIC: "1",
  // Same as `npm run tauri` (ui/package.json).
  TAURI_APP_PATH: join(root, "crates", "app"),
  TAURI_FRONTEND_PATH: join(root, "ui"),
};
const features = ["--no-default-features", "--features", "system-duckdb"];
const args = process.argv.slice(2);
const run = (cmd, a, cwd = root) => {
  const r = spawnSync(cmd, a, { cwd, env, stdio: "inherit", shell: win });
  if (r.status !== 0) process.exit(r.status ?? 1);
};

if (args[0] === "--cargo") {
  // Package-scoped cargo commands for databrain-app get the features.
  const rest = args.slice(1);
  const forApp = rest.includes("databrain-app");
  run("cargo", forApp ? [...rest, ...features] : rest);
} else {
  const ui = join(root, "ui");
  const config = join(root, "crates", "app", "tauri.system-duckdb.conf.json");
  const mode = args.includes("--dev") ? "dev" : "build";
  run("npx", ["tauri", mode, "--config", config, ...args.filter((a) => a !== "--dev"), "--", ...features], ui);
  if (mode === "build") {
    // Name the installers so they don't overwrite the full build's
    // (target/release/bundle, or target/<triple>/release/bundle with --target).
    const t = args.indexOf("--target");
    const base = t >= 0 ? join(root, "target", args[t + 1], "release", "bundle") : join(root, "target", "release", "bundle");
    const dirs = ["dmg", "macos", "msi", "nsis", "deb", "appimage", "rpm"].map((d) => join(base, d));
    for (const d of dirs.filter(existsSync))
      for (const f of readdirSync(d))
        if (/^DataBrain_\d/.test(f) && !f.includes("no-duckdb") && !f.endsWith(".app")) renameSync(join(d, f), join(d, f.replace(/^DataBrain_/, "DataBrain-no-duckdb_")));
  }
}
