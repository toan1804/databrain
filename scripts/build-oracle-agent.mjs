#!/usr/bin/env node
// Build the Oracle helper (crates/connectors/oracle/agent, Go + go-ora).
//
//   node scripts/build-oracle-agent.mjs          all release targets → target/oracle-agent/,
//                                                gzip copies in target/oracle-agent/dist/ (upload
//                                                these to the release), and agent/SHA256SUMS
//   node scripts/build-oracle-agent.mjs --host   this computer only (development; SHA256SUMS kept)
//
// Builds are reproducible (CGO off, -trimpath, no build id), so the same Go
// version produces the binaries whose sums DataBrain checks after download.
import { execFileSync } from "node:child_process";
import { createHash } from "node:crypto";
import { mkdirSync, readFileSync, writeFileSync, existsSync } from "node:fs";
import { gzipSync } from "node:zlib";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";

const root = join(dirname(fileURLToPath(import.meta.url)), "..");
const src = join(root, "crates/connectors/oracle/agent");
const out = join(root, "target/oracle-agent");
const host = process.argv.includes("--host");
const optional = process.argv.includes("--if-go");

function hasGo() {
  try {
    execFileSync("go", ["version"], { stdio: "ignore" });
    return true;
  } catch {
    return false;
  }
}
if (!hasGo()) {
  if (optional) {
    console.log("oracle-agent: Go not installed, skipped (Oracle connections download the driver on first use)");
    process.exit(0);
  }
  console.error("oracle-agent: Go is required (https://go.dev/dl/)");
  process.exit(1);
}

const goos = { darwin: "darwin", linux: "linux", win32: "windows" }[process.platform];
const goarch = { arm64: "arm64", x64: "amd64" }[process.arch];
const targets = host
  ? [[goos, goarch]]
  : [["darwin", "arm64"], ["darwin", "amd64"], ["linux", "amd64"], ["linux", "arm64"], ["windows", "amd64"]];

mkdirSync(join(out, "dist"), { recursive: true });
const sums = [];
for (const [os, arch] of targets) {
  const file = `databrain-oracle-agent-${os}-${arch}${os === "windows" ? ".exe" : ""}`;
  const dest = join(out, file);
  execFileSync("go", ["build", "-trimpath", "-ldflags", "-s -w -buildid=", "-o", dest, "."], {
    cwd: src,
    stdio: "inherit",
    env: { ...process.env, CGO_ENABLED: "0", GOOS: os, GOARCH: arch, GOFLAGS: "-mod=readonly" },
  });
  const bin = readFileSync(dest);
  sums.push(`${createHash("sha256").update(bin).digest("hex")}  ${file}`);
  if (!host) writeFileSync(join(out, "dist", `${file}.gz`), gzipSync(bin, { level: 9 }));
  console.log(`oracle-agent: ${file} (${(bin.length / 1e6).toFixed(1)} MB)`);
}
if (!host) {
  const goVersion = execFileSync("go", ["env", "GOVERSION"]).toString().trim();
  const sumsFile = join(src, "SHA256SUMS");
  writeFileSync(sumsFile, `# ${goVersion}; upload target/oracle-agent/dist/*.gz to the release\n${sums.join("\n")}\n`);
  console.log(`oracle-agent: wrote ${sumsFile}`);
} else if (!existsSync(join(src, "SHA256SUMS"))) {
  console.warn("oracle-agent: agent/SHA256SUMS missing; run without --host to create it");
}
