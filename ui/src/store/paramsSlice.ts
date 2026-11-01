import type { StateCreator } from "zustand";
import { api } from "../lib/api";
import type { ConnectorKind, ParamValue } from "../lib/types";
import { persistParams } from "./helpers";
import type { State } from "./types";

export interface ParamsSlice {
  /** Parameter values per query tab (`:name` → value). Saved in settings. */
  params: Record<string, Record<string, ParamValue>>;
  /** `:name` parameters found in each query tab's SQL (in order, unique). */
  paramNames: Record<string, string[]>;
  /** Per tab: names used as Oracle `&name` substitution variables. */
  paramTextual: Record<string, string[]>;
  /** Parameter box to focus (a run was missing its value). */
  paramFocus: { tabId: string; name: string; seq: number } | null;
  setParam: (tabId: string, name: string, patch: Partial<ParamValue>) => void;
  setParamNames: (tabId: string, names: string[], textual?: string[]) => void;
  /**
   * Values for the parameters `sql` uses, from tab/notebook `tabId`; `null`
   * (with a toast and the first empty box focused) when one has no value.
   */
  paramsFor: (tabId: string, kind: ConnectorKind, sql: string) => Promise<Record<string, ParamValue> | null>;
}

export const createParamsSlice: StateCreator<State, [], [], ParamsSlice> = (set, get) => ({
  params: {},
  paramNames: {},
  paramTextual: {},
  paramFocus: null,
  setParam: (tabId, name, patch) => {
    const cur = get().params[tabId]?.[name] ?? { value: "" };
    const params = { ...get().params, [tabId]: { ...get().params[tabId], [name]: { ...cur, ...patch } } };
    set({ params });
    persistParams(params);
  },
  setParamNames: (tabId, names, textual = []) => {
    const same = (a: string[] | undefined, b: string[]) => !!a && a.length === b.length && a.every((n, i) => n === b[i]);
    if (same(get().paramNames[tabId], names) && same(get().paramTextual[tabId] ?? [], textual)) return;
    set((s) => ({ paramNames: { ...s.paramNames, [tabId]: names }, paramTextual: { ...s.paramTextual, [tabId]: textual } }));
  },
  paramsFor: async (tabId, kind, sql) => {
    const values = get().params[tabId] ?? {};
    if (!sql.includes(":") && !sql.includes("&")) return values;
    const used = await api.sqlParameters(kind, sql).catch(() => []);
    const missing = [...new Set(used.map((p) => p.name))].filter((n) => !values[n]?.value.trim());
    if (!missing.length) return values;
    get().toast(`Enter a value for ${missing.map((n) => `:${n}`).join(", ")}`, "error");
    set((st) => ({ paramFocus: { tabId, name: missing[0], seq: (st.paramFocus?.seq ?? 0) + 1 } }));
    return null;
  },
});
