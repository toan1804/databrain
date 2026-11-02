import { describe, expect, it } from "vitest";
import { loadModesFor } from "./LoadFields";

describe("load modes per target", () => {
  it("offers Update and Merge only where rows can be updated by key", () => {
    expect(loadModesFor("postgres").map((m) => m.mode)).toEqual(["append", "truncate", "replace", "update", "merge"]);
    expect(loadModesFor("clickhouse").map((m) => m.mode)).toEqual(["append", "truncate", "replace"]);
  });
  it("keeps a step's saved mode visible, marked", () => {
    const modes = loadModesFor("clickhouse", "merge");
    expect(modes.map((m) => m.mode)).toEqual(["append", "truncate", "replace", "merge"]);
    expect(modes[3].text).toContain("not on ClickHouse");
  });
});
