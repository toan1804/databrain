import { describe, expect, it } from "vitest";
// @ts-expect-error: a plain .mjs script outside the UI's sources.
import { releaseNotes } from "../../../scripts/release-notes.mjs";

const CHANGELOG = `# Changelog

Intro.

## Unreleased

### Added

- Coming soon.

## 0.3.4 - 2026-11-12

### Fixed

- Thing one.
- Thing two.

## 0.3.3 - 2026-11-11

### Added

- Older.
`;

describe("release notes", () => {
  it("are the version's CHANGELOG section, without its heading", () => {
    expect(releaseNotes(CHANGELOG, "v0.3.4")).toBe("### Fixed\n\n- Thing one.\n- Thing two.");
    expect(releaseNotes(CHANGELOG, "0.3.3")).toBe("### Added\n\n- Older.");
  });

  it("are empty for a version without a section (no prefix matches)", () => {
    expect(releaseNotes(CHANGELOG, "0.3.5")).toBe("");
    expect(releaseNotes(CHANGELOG, "0.3")).toBe("");
  });
});
