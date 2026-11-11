import { act, render, screen, waitFor, within } from "@testing-library/react";
import { beforeEach, describe, expect, it, vi } from "vitest";
import { api } from "../lib/api";
import type { ConnectionView, PlanNode, QueryPlan } from "../lib/types";
import { useStore, type Tab } from "../store";
import { canComparePlans, PlanCompareTab } from "./PlanCompareTab";

const n = (op: string, p: Partial<PlanNode> = {}): PlanNode => ({ op, extra: [], children: [], ...p });
const plan = (roots: PlanNode[]): QueryPlan => ({ analyzed: false, roots, raw: "" });

const conn = (kind: "postgres" | "bigquery"): ConnectionView =>
  ({ id: "c1", name: "Warehouse", connected: true, env: "dev", has_secret: false, created_at: 0, updated_at: 0, config: { kind, auth: { kind: "none" }, ssl_mode: "prefer", read_only: false }, ai_policy: {} }) as unknown as ConnectionView;

const BEFORE = "select * from orders where cast(created as date) = '2026-01-01'";
const AFTER = "select * from orders where created >= '2026-01-01' and created < '2026-01-02'";

function setup(kind: "postgres" | "bigquery") {
  const tab = { id: "p1", title: "Plans · Q", sql: AFTER, connection_id: "c1", plan: { source: "q1", analyze: false, compare: { before: BEFORE, after: AFTER } } } as Tab;
  useStore.setState({ connections: [conn(kind)], tabs: [tab] });
}

beforeEach(() => vi.restoreAllMocks());

describe("plan comparison tab", () => {
  it("is offered where there is a plan or a cost estimate", () => {
    expect(canComparePlans("postgres")).toBe(true);
    expect(canComparePlans("bigquery")).toBe(true);
    expect(canComparePlans("clickhouse")).toBe(false);
    expect(canComparePlans(undefined)).toBe(false);
  });

  it("shows what the rewrite changed, on the source tab's session", async () => {
    setup("postgres");
    const explain = vi.spyOn(api, "explainPlan").mockImplementation(async (_c, _s, sql) =>
      sql === BEFORE ? plan([n("Seq Scan", { object: "on orders", rows: 90_000, cost: 1000 })]) : plan([n("Index Scan", { object: "using orders_created on orders", rows: 40, cost: 25 })]),
    );
    render(<PlanCompareTab tabId="p1" visible />);
    const table = await screen.findByRole("table", { name: "What changed" });
    const cost = within(table).getByText("Estimated cost").closest("tr")!;
    expect(cost).toHaveTextContent("1000");
    expect(cost).toHaveTextContent("25");
    expect(cost).toHaveTextContent("−98%");
    expect(screen.getByText("No longer: Seq Scan on orders")).toBeInTheDocument();
    expect(explain).toHaveBeenCalledWith("c1", "q1", BEFORE, false, undefined);
    expect(explain).toHaveBeenCalledWith("c1", "q1", AFTER, false, undefined);
  });

  it("compares bytes scanned on BigQuery, which has no plan", async () => {
    setup("bigquery");
    const explain = vi.spyOn(api, "explainPlan");
    vi.spyOn(api, "estimateCost").mockImplementation(async (_c, _s, sql) => ({ method: "dry run", bytes: sql === BEFORE ? 4 * 1024 ** 3 : 1024 ** 3, cache_hit: false, notes: [] }));
    render(<PlanCompareTab tabId="p1" visible />);
    const row = (await screen.findByText("Bytes scanned")).closest("tr")!;
    await waitFor(() => expect(row).toHaveTextContent("−75%"));
    expect(row).toHaveTextContent("4.0 GiB");
    expect(explain).not.toHaveBeenCalled();
  });

  it("explains again with the new parameters when compared again", async () => {
    setup("postgres");
    const explain = vi.spyOn(api, "explainPlan").mockResolvedValue(plan([n("Seq Scan", { object: "on orders", rows: 90_000 })]));
    render(<PlanCompareTab tabId="p1" visible />);
    await screen.findByRole("table", { name: "What changed" });
    expect(explain).toHaveBeenCalledTimes(2);
    const ref = useStore.getState().tabs[0].plan!;
    act(() => useStore.getState().updateTab("p1", { plan: { ...ref, params: { day: { value: "2026-02-01" } } } }));
    await waitFor(() => expect(explain).toHaveBeenCalledTimes(4));
    expect(explain).toHaveBeenLastCalledWith("c1", "q1", AFTER, false, { day: { value: "2026-02-01" } });
  });

  it("says so when the tab's connection is gone instead of spinning", async () => {
    setup("postgres");
    useStore.setState({ connections: [] });
    const explain = vi.spyOn(api, "explainPlan");
    render(<PlanCompareTab tabId="p1" visible />);
    expect(await screen.findByRole("alert")).toHaveTextContent("The connection of this tab no longer exists.");
    expect(explain).not.toHaveBeenCalled();
  });
});
