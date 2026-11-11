import { describe, expect, it } from "vitest";
import type { ProviderView, TestDataColumn } from "./types";
import { DEFAULT_TEST_DATA_AI, aiRequest, aiTarget, aiWrites, clampAiRows, parseTestDataAi } from "./testDataAi";

const col = (name: string, p: Partial<TestDataColumn> = {}): TestDataColumn => ({
  name,
  data_type: "text",
  nullable: true,
  primary_key: false,
  supported: true,
  database_fills: false,
  generator: "",
  ai_text: false,
  ai_ok: false,
  ...p,
});

const provider = (id: string, kind = "anthropic") => ({ id, kind, name: id }) as unknown as ProviderView;

describe("generate test data with AI", () => {
  it("defaults to on, text columns only, 100 rows; a bad saved value falls back", () => {
    expect(DEFAULT_TEST_DATA_AI).toEqual({ enabled: true, textOnly: true, rows: 100 });
    expect(parseTestDataAi(null)).toEqual(DEFAULT_TEST_DATA_AI);
    expect(parseTestDataAi({ enabled: false, textOnly: false, rows: 9999 })).toEqual({ enabled: false, textOnly: false, rows: 500 });
    expect(parseTestDataAi({ enabled: "yes", rows: "x" })).toEqual(DEFAULT_TEST_DATA_AI);
    expect(clampAiRows(0)).toBe(1);
  });

  it("writes only ticked text columns by default, never foreign keys", () => {
    const name = col("name", { ai_text: true, ai_ok: true });
    const age = col("age", { ai_ok: true });
    const fk = col("team_id", { parent: "teams.id", ai_ok: false });
    const ticked = new Set(["name", "age", "team_id"]);
    const on = DEFAULT_TEST_DATA_AI;
    expect([name, age, fk].filter((c) => aiWrites(c, ticked, on)).map((c) => c.name)).toEqual(["name"]);
    const all = { ...on, textOnly: false };
    expect([name, age, fk].filter((c) => aiWrites(c, ticked, all)).map((c) => c.name)).toEqual(["name", "age"]);
    expect(aiWrites(name, new Set(), on)).toBe(false);
    expect(aiWrites(name, ticked, { ...on, enabled: false })).toBe(false);
  });

  it("asks the AI panel's provider, not Kiro", () => {
    const ps = [provider("kiro", "kiro"), provider("claude"), provider("gpt")];
    expect(aiTarget(ps, { providerId: "gpt", model: "m" })).toEqual({ provider_id: "gpt", model: "m" });
    expect(aiTarget(ps, { providerId: "kiro", model: "k" })).toEqual({ provider_id: "claude", model: null });
    expect(aiTarget([provider("kiro", "kiro")], { providerId: "kiro", model: null })).toBeNull();
    // The connection allows only some providers.
    expect(aiTarget(ps, { providerId: "claude", model: "m" }, ["gpt"])).toEqual({ provider_id: "gpt", model: null });
    expect(aiTarget(ps, { providerId: "claude", model: "m" }, ["other"])).toBeNull();
  });

  it("sends the AI request only when there is something for it to write", () => {
    const t = { provider_id: "claude", model: null };
    expect(aiRequest(DEFAULT_TEST_DATA_AI, t, 2)).toEqual({ provider_id: "claude", model: null, all_columns: false, rows: 100 });
    expect(aiRequest({ ...DEFAULT_TEST_DATA_AI, textOnly: false }, t, 2)?.all_columns).toBe(true);
    expect(aiRequest(DEFAULT_TEST_DATA_AI, t, 0)).toBeNull();
    expect(aiRequest(DEFAULT_TEST_DATA_AI, null, 2)).toBeNull();
    expect(aiRequest({ ...DEFAULT_TEST_DATA_AI, enabled: false }, t, 2)).toBeNull();
  });
});
