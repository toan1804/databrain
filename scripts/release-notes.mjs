// The release notes of a version: its section of CHANGELOG.md, as Markdown.
// The release workflow puts them on the GitHub release (which the app's
// "What's new" opens) and in latest.json for the updater.
//
// Usage: node scripts/release-notes.mjs <version or vX.Y.Z> [CHANGELOG.md] > notes.md

import { readFileSync } from "node:fs";
import { fileURLToPath } from "node:url";

/** The body of `## <version>` (any date after it), without the heading; "" when there is none. */
export function releaseNotes(changelog, version) {
  const v = version.replace(/^v/, "");
  const lines = changelog.split(/\r?\n/);
  const start = lines.findIndex((l) => l.startsWith("## ") && l.slice(3).trim().split(/\s/)[0] === v);
  if (start < 0) return "";
  let end = lines.findIndex((l, i) => i > start && l.startsWith("## "));
  if (end < 0) end = lines.length;
  return lines.slice(start + 1, end).join("\n").trim();
}

if (process.argv[1] === fileURLToPath(import.meta.url)) {
  const [version, file = "CHANGELOG.md"] = process.argv.slice(2);
  if (!version) {
    console.error("usage: release-notes.mjs <version> [CHANGELOG.md]");
    process.exit(2);
  }
  const notes = releaseNotes(readFileSync(file, "utf8"), version);
  if (!notes) {
    console.error(`release-notes: no "## ${version.replace(/^v/, "")}" section in ${file}`);
    process.exit(1);
  }
  process.stdout.write(`${notes}\n`);
}
