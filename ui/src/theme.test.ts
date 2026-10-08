import { describe, expect, it, vi } from "vitest";

// Minimal <html> stand-in (tests run in node).
const classes = new Set<string>();
const html = { classList: { toggle: (c: string, on: boolean) => (on ? classes.add(c) : classes.delete(c)) }, style: { colorScheme: "", setProperty: () => {}, removeProperty: () => {} } };
(globalThis as { document?: unknown }).document = { documentElement: html };

vi.mock("./lib/api", () => ({
  isTauri: () => false,
  toError: (e: unknown) => ({ kind: "internal", message: String(e) }),
  onJobEvent: async () => () => {},
  onAuthEvent: async () => () => {},
  onOracleAgent: async () => () => {},
  api: {},
}));

const { useStore } = await import("./store");

describe("theme switch", () => {
  it("puts the theme on <html> before anything renders with it", () => {
    // What the result grid does while rendering: read CSS (the class) for the new theme.
    const seen: [string, boolean][] = [];
    const unsub = useStore.subscribe((s) => seen.push([s.theme, classes.has("dark")]));
    useStore.getState().setTheme("light");
    useStore.getState().setTheme("dark");
    useStore.getState().setTheme("light");
    unsub();
    expect(seen).toEqual([
      ["light", false],
      ["dark", true],
      ["light", false],
    ]);
    expect(html.style.colorScheme).toBe("light");
  });
});
