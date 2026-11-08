// Settings → Editor: which provider completes SQL inline. Kiro can't (it
// runs as an agent, not a model), so inline completion may use another
// provider than the AI panel.
import type { ProviderView } from "./types";

export interface InlineAiProvider {
  provider_id: string;
  /** Empty: the provider's fast model, else its default model. */
  model: string;
}

/** The saved setting, or null when it isn't a valid choice. */
export function parseInlineAiProvider(v: unknown): InlineAiProvider | null {
  if (!v || typeof v !== "object") return null;
  const o = v as Record<string, unknown>;
  if (typeof o.provider_id !== "string" || !o.provider_id) return null;
  return { provider_id: o.provider_id, model: typeof o.model === "string" ? o.model : "" };
}

/** Providers that can complete inline (model providers, not Kiro). */
export function inlineCapable(providers: ProviderView[]): ProviderView[] {
  return providers.filter((p) => p.kind !== "kiro");
}

/**
 * Who a suggestion is asked of: the provider chosen for inline completion
 * (its model, when one is set, wins over the fast model), or the AI panel's
 * when none is chosen or the chosen one was removed.
 */
export function inlineTarget(
  choice: InlineAiProvider | null,
  providers: ProviderView[],
  panel: { providerId: string | null; model: string | null },
): { provider_id: string | null; model: string | null; exact_model: boolean } {
  if (choice && inlineCapable(providers).some((p) => p.id === choice.provider_id)) {
    const model = choice.model.trim();
    return { provider_id: choice.provider_id, model: model || null, exact_model: !!model };
  }
  return { provider_id: panel.providerId, model: panel.model, exact_model: false };
}
