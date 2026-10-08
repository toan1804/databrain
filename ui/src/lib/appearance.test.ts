import { describe, expect, it } from "vitest";
import { BACKGROUNDS, DEFAULT_APPEARANCE, appearanceVars, backgroundMatchesTheme, cleanFontName, contrast, deriveSurface, parseAppearance, parseHex } from "./appearance";

describe("appearance", () => {
  it("sets nothing for the defaults (index.css has them)", () => {
    expect(appearanceVars(DEFAULT_APPEARANCE, "dark")).toEqual({});
  });

  it("uses each preset's colours for the theme", () => {
    const v = appearanceVars({ ...DEFAULT_APPEARANCE, background: "slate" }, "light");
    expect(v["--bg"]).toBe(BACKGROUNDS.find((b) => b.id === "slate")!.light.bg);
    expect(v["--text"]).toBeUndefined();
    expect(appearanceVars({ ...DEFAULT_APPEARANCE, background: "contrast" }, "dark")["--text"]).toBe("#ffffff");
  });

  it("derives readable surfaces from a custom colour", () => {
    for (const hex of ["#1e2230", "#f5e6c8", "#336699", "#000000", "#ffffff"]) {
      const s = deriveSurface(hex)!;
      expect(contrast(parseHex(s.bg)!, parseHex(s.text!)!)).toBeGreaterThanOrEqual(4.5);
      expect(contrast(parseHex(s.panel)!, parseHex(s.muted!)!)).toBeGreaterThan(3);
    }
    expect(deriveSurface("nope")).toBeNull();
    expect(backgroundMatchesTheme("#101010", "dark")).toBe(true);
    expect(backgroundMatchesTheme("#f0f0f0", "dark")).toBe(false);
  });

  it("builds font stacks and cleans typed names", () => {
    const v = appearanceVars({ ...DEFAULT_APPEARANCE, uiFont: "custom", uiFontCustom: 'Open Sans"; color: red', monoFont: "fira", editorFontSize: 15 }, "dark");
    expect(v["--font-sans"]).toMatch(/^"Open Sans color: red", /);
    expect(v["--font-mono"]).toMatch(/^"Fira Code"/);
    expect(v["--editor-font-size"]).toBe("15px");
    expect(v["--grid-font-size"]).toBe("14px");
    expect(cleanFontName("  A{b}  c  ")).toBe("Ab c");
  });

  it("reads saved settings defensively", () => {
    expect(parseAppearance(undefined)).toEqual(DEFAULT_APPEARANCE);
    const a = parseAppearance({ background: "bogus", uiFont: "roboto", editorFontSize: 99, customBackground: "red" });
    expect(a).toMatchObject({ background: "default", uiFont: "roboto", editorFontSize: 22, customBackground: DEFAULT_APPEARANCE.customBackground });
  });
});
