import { applyAppearance, type Appearance } from "../lib/appearance";
import { api, isTauri } from "../lib/api";
import type { AuthEvent, OutputInfo, ParamValue, SchemaInfo } from "../lib/types";
import type { QuickFilterEntry } from "../lib/gridViews";
import type { SignInState, Tab, TabRun, Theme } from "./types";

/**
 * Next sign-in dialog state for an auth event. A dialog opened by an explicit
 * sign-in stays until that call returns ("Finishing sign-in…"); one the
 * backend opened on its own closes when the sign-in finishes, since nothing
 * else would close it (the connection then just carries on).
 */
export function nextSignIn(cur: SignInState | null, event: AuthEvent): SignInState | null {
  if (event.type === "finished") return cur && !cur.implicit ? { ...cur, event } : null;
  if (cur) return { ...cur, event };
  return { connectionId: null, label: "Sign in", event, implicit: true };
}

/**
 * Schema ids to list for a saved filter: the chosen schemas (two-level), or
 * every schema of the chosen catalogs. Names that match nothing are skipped.
 */
export function chosenSchemaIds(schemas: SchemaInfo[], chosen: string[]): string[] {
  const set = new Set(chosen);
  return schemas.filter((s) => set.has(s.name) || (!!s.catalog && set.has(s.catalog))).map((s) => s.name);
}

/**
 * Put the theme on <html> right away. Components that read CSS variables
 * while rendering (the result grid's canvas colours) must see the new values
 * in the same render that sees the new `theme`, so this runs before the
 * store changes (an effect would run after that render).
 */
export function applyTheme(theme: Theme, appearance?: Appearance) {
  if (typeof document === "undefined") return;
  document.documentElement.classList.toggle("dark", theme === "dark");
  document.documentElement.style.colorScheme = theme;
  // Background presets have a colour per theme.
  if (appearance) applyAppearance(appearance, theme);
}

const TAB_SAVE_DELAY = 400;

/** Notice shown on a result restored from the previous session. */
export const RESTORED_NOTICE = "Restored from your last session. Run again for fresh data.";

/**
 * Run state that shows a tab's saved outputs (its last results, saved when
 * the app quit) without re-running anything. `null` when the tab has none.
 */
export function restoredRun(tabId: string, outputs: OutputInfo[]): TabRun | null {
  const mine = outputs.filter((o) => o.tab_id === tabId && o.active && o.state !== "evicted").sort((a, b) => a.statement_index - b.statement_index);
  if (!mine.length) return null;
  return {
    jobId: null,
    running: false,
    startedAt: Math.min(...mine.map((o) => o.created_at)),
    finishedStatus: "success",
    statements: mine.map((o) => ({
      plan: { index: o.statement_index, sql: o.sql, start: 0, end: 0, classification: { kind: "read", missing_where: false, keyword: "" } },
      status: "done",
      result: { id: o.result_id, columns: o.columns, total_rows: o.rows, complete: true, truncated: o.truncated, bytes: o.bytes },
      output: o,
      notices: [RESTORED_NOTICE],
    })),
    activeIndex: mine.length - 1,
  };
}

let tabSaveTimer: ReturnType<typeof setTimeout> | undefined;

export function persistTabs(tabs: Tab[]) {
  if (!isTauri()) return;
  clearTimeout(tabSaveTimer);
  tabSaveTimer = setTimeout(() => {
    api
      .saveTabs(
        // DDL views are reloaded from the server on demand, not restored.
        tabs.filter((t) => !t.ddl && !t.lineage && !t.job_id && !t.plan && !t.compare && !t.er && !t.dashboard_id).map((t) => ({
          id: t.id,
          title: t.title,
          sql: t.sql,
          connection_id: t.connection_id ?? null,
          saved_query_id: t.saved_query_id ?? null,
          notebook_id: t.notebook_id ?? null,
          output_ref: t.output_ref ?? null,
          auto_commit: t.auto_commit ?? null,
          file_path: t.file_path ?? null,
        })),
      )
      .catch(() => {});
  }, TAB_SAVE_DELAY);
}

/** File name of a path (either slash). */
export const baseName = (p: string) => p.split(/[\\/]/).pop() || p;

/** `editor_params` setting: { tabId: { name: { value, raw } } }, for tabs that still exist. */
export function parseParams(v: unknown, tabIds: string[]): Record<string, Record<string, ParamValue>> {
  const out: Record<string, Record<string, ParamValue>> = {};
  if (!v || typeof v !== "object") return out;
  for (const id of tabIds) {
    const m = (v as Record<string, unknown>)[id];
    if (!m || typeof m !== "object") continue;
    const vals: Record<string, ParamValue> = {};
    for (const [k, x] of Object.entries(m as Record<string, unknown>)) {
      if (x && typeof x === "object" && typeof (x as ParamValue).value === "string") vals[k] = { value: (x as ParamValue).value, raw: !!(x as ParamValue).raw };
    }
    out[id] = vals;
  }
  return out;
}

let paramsSaveTimer: ReturnType<typeof setTimeout> | undefined;
export function persistParams(params: Record<string, Record<string, ParamValue>>) {
  if (!isTauri()) return;
  clearTimeout(paramsSaveTimer);
  paramsSaveTimer = setTimeout(() => void api.setSetting("editor_params", params).catch(() => {}), TAB_SAVE_DELAY);
}

let quickSaveTimer: ReturnType<typeof setTimeout> | undefined;
export function persistQuickFilters(v: Record<string, QuickFilterEntry>) {
  if (!isTauri()) return;
  clearTimeout(quickSaveTimer);
  quickSaveTimer = setTimeout(() => void api.setSetting("grid_quick_filters", v).catch(() => {}), TAB_SAVE_DELAY);
}

/** `explorer_schemas` setting: { connectionId: [schema ids] }. */
export function parseSchemaFilter(v: unknown): Record<string, string[]> {
  if (!v || typeof v !== "object") return {};
  const out: Record<string, string[]> = {};
  for (const [k, list] of Object.entries(v as Record<string, unknown>)) {
    if (Array.isArray(list)) {
      const names = list.filter((x): x is string => typeof x === "string");
      if (names.length) out[k] = names;
    }
  }
  return out;
}
