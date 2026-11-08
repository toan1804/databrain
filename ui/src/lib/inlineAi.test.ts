import { describe, expect, it } from "vitest";
import type { ProviderView } from "./types";
import { inlineCapable, inlineTarget, parseInlineAiProvider } from "./inlineAi";

const prov = (id: string, kind: string) => ({ id, kind, name: id, has_key: true, config: { auth: { type: "api_key" } }, created_at: 0, updated_at: 0 }) as unknown as ProviderView;
const PROVIDERS = [prov("kiro", "kiro"), prov("ollama", "ollama"), prov("openai", "openai")];
const PANEL = { providerId: "kiro", model: "auto" };

describe("inline completion provider", () => {
  it("reads the saved setting", () => {
    expect(parseInlineAiProvider({ provider_id: "ollama", model: "qwen2.5-coder" })).toEqual({ provider_id: "ollama", model: "qwen2.5-coder" });
    expect(parseInlineAiProvider({ provider_id: "ollama" })).toEqual({ provider_id: "ollama", model: "" });
    expect(parseInlineAiProvider(null)).toBeNull();
    expect(parseInlineAiProvider({ provider_id: "" })).toBeNull();
  });

  it("never offers Kiro", () => {
    expect(inlineCapable(PROVIDERS).map((p) => p.id)).toEqual(["ollama", "openai"]);
  });

  it("asks the chosen provider, its model winning over the fast model; else the AI panel's", () => {
    expect(inlineTarget({ provider_id: "ollama", model: " qwen2.5-coder " }, PROVIDERS, PANEL)).toEqual({ provider_id: "ollama", model: "qwen2.5-coder", exact_model: true });
    expect(inlineTarget({ provider_id: "openai", model: "" }, PROVIDERS, PANEL)).toEqual({ provider_id: "openai", model: null, exact_model: false });
    // None chosen, a removed one, or Kiro: the panel's (the backend says Kiro can't).
    expect(inlineTarget(null, PROVIDERS, PANEL)).toEqual({ provider_id: "kiro", model: "auto", exact_model: false });
    expect(inlineTarget({ provider_id: "gone", model: "x" }, PROVIDERS, PANEL)).toEqual({ provider_id: "kiro", model: "auto", exact_model: false });
    expect(inlineTarget({ provider_id: "kiro", model: "" }, PROVIDERS, PANEL).exact_model).toBe(false);
  });
});
