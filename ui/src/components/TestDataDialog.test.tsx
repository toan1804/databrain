import { act, fireEvent, render, screen, waitFor } from "@testing-library/react";
import { beforeEach, describe, expect, it, vi } from "vitest";
import { useAi } from "../aiStore";
import { api } from "../lib/api";
import type { ConnectionView, ProviderView, TestDataColumn, TestDataPlan } from "../lib/types";
import { useStore } from "../store";
import { suggestTable, testDataTable } from "./LoadIntoDialog";
import { defaultColumns, MAX_SEED, seedProblem, specFor, TestDataDialogHost } from "./TestDataDialog";

// No Tauri events in tests.
vi.mock("../lib/api", async (importOriginal) => ({ ...(await importOriginal<typeof import("../lib/api")>()), onLoadProgress: () => Promise.resolve(() => {}) }));

const col = (name: string, p: Partial<TestDataColumn> = {}): TestDataColumn => ({
  name,
  data_type: "int",
  nullable: true,
  primary_key: false,
  supported: true,
  database_fills: false,
  computed: false,
  generator: "",
  ai_text: false,
  ai_ok: false,
  ...p,
});

describe("test data dialog", () => {
  const cols = [col("id", { database_fills: true, primary_key: true }), col("name"), col("note"), col("shape", { supported: false })];

  it("ticks what DataBrain fills and the database doesn't", () => {
    expect([...defaultColumns(cols)]).toEqual(["name", "note"]);
  });

  it("sends unticked columns as skip and ticked database-filled ones as fill", () => {
    const s = specFor(cols, new Set(["id", "name"]), 250_000, 140, " 42 ", false);
    expect(s).toEqual({ rows: 100_000, null_percent: 100, seed: 42, skip: ["note"], fill: ["id"], parent_keys: false, patterns: {} });
    expect(specFor(cols, new Set(), 0, -5, "abc", true)).toMatchObject({ rows: 1, null_percent: 0, seed: null });
  });

  it("never sends a computed column to fill", () => {
    const c = [col("total", { database_fills: true, computed: true }), col("name")];
    expect(specFor(c, new Set(["total", "name"]), 10, 0, "", true)).toMatchObject({ skip: [], fill: [] });
  });

  it("sends every column's pattern: the user's, else its default, empty for none", () => {
    const pcols = [
      col("id", { primary_key: true }),
      col("age", { pattern: "18..90", pattern_hint: "18..90" }),
      col("email", { pattern_hint: "%@gmail.com" }),
      col("born", { pattern: "1950..2005", pattern_hint: "1950-01-01..2005-12-31" }),
    ];
    const s = specFor(pcols, new Set(["age", "email", "born"]), 10, 0, "", true, { email: " %@corp.vn ", born: "" });
    expect(s.patterns).toEqual({ age: "18..90", email: "%@corp.vn", born: "" });
  });

  it("sends patterns of ticked columns only, so one kept for a column left out doesn't block generating", () => {
    const pcols = [col("age", { pattern: "18..90", pattern_hint: "18..90" }), col("code", { pattern_hint: "%@gmail.com" })];
    const s = specFor(pcols, new Set(["age"]), 10, 0, "", true, { code: "INV-##### far too long {" });
    expect(s.patterns).toEqual({ age: "18..90" });
  });

  it("sends only seeds that a JavaScript number keeps exactly", () => {
    expect(specFor(cols, new Set(), 1, 0, String(MAX_SEED), true).seed).toBe(9007199254740991);
    expect(seedProblem("9007199254740991")).toBeNull();
    expect(seedProblem("9007199254740993")).toMatch(/up to 9007199254740991/);
    expect(specFor(cols, new Set(), 1, 0, "9007199254740993", true).seed).toBeNull();
    expect(seedProblem("")).toBeNull();
  });

  it("loads generated rows back into their table by default", () => {
    const sql = '-- Test data for "public"."orders" (50 rows, seed 3)';
    expect(testDataTable(sql)).toBe('"public"."orders"');
    expect(suggestTable(sql, null)).toBe('"public"."orders"');
    expect(testDataTable("select * from orders")).toBeNull();
    expect(suggestTable("select * from sales.orders")).toBe("orders");
  });
});

describe("suggest patterns with AI", () => {
  beforeEach(() => vi.restoreAllMocks());

  it("keeps pattern edits typed while the AI was suggesting", async () => {
    const conn = {
      id: "c1",
      name: "Dev",
      connected: true,
      env: "dev",
      has_secret: false,
      created_at: 0,
      updated_at: 0,
      config: { kind: "postgres", auth: { kind: "none" }, ssl_mode: "prefer", read_only: false },
      ai_policy: { ai_enabled: true, share_metadata: true, allowed_providers: [] },
    } as unknown as ConnectionView;
    const plan: TestDataPlan = {
      table: '"public"."people"',
      columns: [col("age", { pattern: "18..90", pattern_hint: "18..90" }), col("email", { data_type: "text", pattern_hint: "%@gmail.com" })],
      foreign_keys: [],
      warnings: [],
    };
    useStore.setState({ connections: [conn], testDataPatterns: {}, testDataDialog: { connection_id: "c1", schema: "public", table: "people" } });
    useAi.setState({ providers: [{ id: "p1", name: "AI", kind: "anthropic" } as unknown as ProviderView], providerId: "p1", model: null });
    vi.spyOn(api, "testDataPlan").mockResolvedValue(plan);
    let answer: (v: Record<string, string>) => void = () => {};
    vi.spyOn(api, "suggestTestDataPatterns").mockImplementation(() => new Promise((r) => (answer = r)));
    render(<TestDataDialogHost />);
    const email = await screen.findByLabelText("Pattern for email");
    fireEvent.click(screen.getByText("Suggest patterns with AI"));
    fireEvent.change(email, { target: { value: "%@corp.vn" } });
    await act(async () => answer({ age: "20..60" }));
    await waitFor(() => expect(screen.getByLabelText("Pattern for age")).toHaveValue("20..60"));
    expect(screen.getByLabelText("Pattern for email")).toHaveValue("%@corp.vn");
    expect(useStore.getState().testDataPatterns["c1|public.people"]).toEqual({ age: "20..60", email: "%@corp.vn" });
  });
});
