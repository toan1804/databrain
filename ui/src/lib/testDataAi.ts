// "Generate with AI" in Generate test data: realistic values from an AI
// model for the columns random values can't make sense of. The choices are
// remembered (setting `test_data_ai`).
import type { ProviderView, TestDataAi, TestDataColumn } from "./types";

export interface TestDataAiSettings {
  /** Ask the AI (when the connection allows AI and a provider is set up). */
  enabled: boolean;
  /** Only text columns (names, emails, descriptions): cheaper. Unticked: every column the AI may write. */
  textOnly: boolean;
  /** Rows the AI writes; more rows reuse them, so the cost stays flat. */
  rows: number;
}

export const MAX_AI_ROWS = 500;
export const DEFAULT_TEST_DATA_AI: TestDataAiSettings = { enabled: true, textOnly: true, rows: 100 };
/** Rows per model call (the backend's batch size), for the cost hint. */
export const AI_BATCH_ROWS = 25;

/** The saved setting, with defaults for what is missing or wrong. */
export function parseTestDataAi(v: unknown): TestDataAiSettings {
  if (!v || typeof v !== "object") return DEFAULT_TEST_DATA_AI;
  const o = v as Record<string, unknown>;
  const rows = typeof o.rows === "number" && Number.isFinite(o.rows) ? clampAiRows(o.rows) : DEFAULT_TEST_DATA_AI.rows;
  return {
    enabled: typeof o.enabled === "boolean" ? o.enabled : DEFAULT_TEST_DATA_AI.enabled,
    textOnly: typeof o.textOnly === "boolean" ? o.textOnly : DEFAULT_TEST_DATA_AI.textOnly,
    rows,
  };
}

export function clampAiRows(n: number): number {
  return Math.max(1, Math.min(MAX_AI_ROWS, Math.floor(n) || 1));
}

/** Whether the AI writes this column (keys and foreign keys never). */
export function aiWrites(c: TestDataColumn, ticked: Set<string>, s: TestDataAiSettings): boolean {
  return s.enabled && ticked.has(c.name) && !c.parent && (s.textOnly ? c.ai_text : c.ai_ok);
}

/**
 * The provider asked: the AI panel's when the connection allows it
 * (`allowed`, empty: any), else the first allowed model provider, else Kiro
 * (each batch starts kiro-cli, so it is slower). Null when there is none.
 */
export function aiTarget(
  providers: ProviderView[],
  panel: { providerId: string | null; model: string | null },
  allowed: string[] = [],
): { provider_id: string; model: string | null } | null {
  const usable = providers.filter((p) => allowed.length === 0 || allowed.includes(p.id));
  const chosen = usable.find((p) => p.id === panel.providerId);
  if (chosen) return { provider_id: chosen.id, model: panel.model };
  const first = usable.find((p) => p.kind !== "kiro") ?? usable[0];
  return first ? { provider_id: first.id, model: null } : null;
}

/** What the backend gets, or null when the AI isn't used. */
export function aiRequest(s: TestDataAiSettings, target: { provider_id: string; model: string | null } | null, writes: number): TestDataAi | null {
  if (!s.enabled || !target || writes === 0) return null;
  return { provider_id: target.provider_id, model: target.model, all_columns: !s.textOnly, rows: clampAiRows(s.rows) };
}
