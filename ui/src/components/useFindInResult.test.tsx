import { act, renderHook } from "@testing-library/react";
import { afterEach, describe, expect, it, vi } from "vitest";
import type { FindPlan } from "../lib/find";
import type { FindResult, ViewSpec } from "../lib/types";

const pending: { term: string; resolve: (r: FindResult) => void }[] = [];
vi.mock("../lib/api", () => ({
  api: {
    findInResult: (_id: string, _view: ViewSpec, term: string) => new Promise<FindResult>((resolve) => pending.push({ term, resolve })),
  },
}));

const { useFindInResult, FIND_DEBOUNCE_MS } = await import("./useFindInResult");

const plan = (term: string): FindPlan => ({ term, columns: null, nameMatches: [] }) as FindPlan;
const hit = (row: number): FindResult => ({ matches: [{ row, col: 0 }], truncated: false }) as FindResult;
const view = {} as ViewSpec;

afterEach(() => {
  pending.length = 0;
  vi.useRealTimers();
});

describe("find in result", () => {
  it("drops the answer to an older search that arrives last", async () => {
    vi.useFakeTimers();
    const onResult = vi.fn();
    const { rerender } = renderHook((p: { term: string }) => useFindInResult({ open: true, plan: plan(p.term), wholeWord: false, view, resultId: "r1", onResult, onError: () => {} }), {
      initialProps: { term: "ali" },
    });
    act(() => vi.advanceTimersByTime(FIND_DEBOUNCE_MS));
    rerender({ term: "alice" });
    act(() => vi.advanceTimersByTime(FIND_DEBOUNCE_MS));
    expect(pending.map((p) => p.term)).toEqual(["ali", "alice"]);
    // The new search answers first, the old one after it.
    await act(async () => pending[1].resolve(hit(2)));
    await act(async () => pending[0].resolve(hit(1)));
    expect(onResult).toHaveBeenCalledTimes(1);
    expect(onResult).toHaveBeenLastCalledWith(hit(2));
  });

  it("debounces typing and clears when closed", () => {
    vi.useFakeTimers();
    const onResult = vi.fn();
    const { rerender } = renderHook((p: { term: string; open: boolean }) => useFindInResult({ open: p.open, plan: plan(p.term), wholeWord: false, view, resultId: "r1", onResult, onError: () => {} }), {
      initialProps: { term: "a", open: true },
    });
    rerender({ term: "ab", open: true });
    rerender({ term: "abc", open: true });
    act(() => vi.advanceTimersByTime(FIND_DEBOUNCE_MS));
    expect(pending.map((p) => p.term)).toEqual(["abc"]);
    rerender({ term: "abc", open: false });
    expect(onResult).toHaveBeenLastCalledWith({ matches: [], truncated: false });
  });
});
