import { beforeEach, describe, expect, it, vi } from "vitest";

const store = new Map<string, string>();
vi.stubGlobal("localStorage", {
  getItem: (k: string) => store.get(k) ?? null,
  setItem: (k: string, v: string) => void store.set(k, v),
  removeItem: (k: string) => void store.delete(k),
  key: () => null,
  get length() {
    return store.size;
  },
});

const { readStored, isStringList, isListRecord } = await import("./storage");

beforeEach(() => store.clear());

describe("stored values", () => {
  it("returns a value that fits, and drops one that doesn't (not JSON, wrong shape)", () => {
    store.set("db.folders.connections", '["a","b"]');
    expect(readStored("db.folders.connections", [], isStringList)).toEqual(["a", "b"]);
    store.set("db.folders.connections", '{"a": 1}');
    expect(readStored("db.folders.connections", [], isStringList)).toEqual([]);
    expect(store.has("db.folders.connections")).toBe(false);
    store.set("db.ai.models", "{not json");
    expect(readStored("db.ai.models", {}, isListRecord)).toEqual({});
    store.set("db.ai.models", '{"p": "gpt"}');
    expect(readStored("db.ai.models", {}, isListRecord)).toEqual({});
    expect(readStored("missing", 7, (v): v is number => typeof v === "number")).toBe(7);
  });
});
