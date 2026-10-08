// Appearance settings: background colours, interface and code fonts, and
// code font size. Applied as CSS variables on <html> (over index.css's
// defaults), so every component, the editor, the result grid and diagrams
// follow them.

export type ThemeMode = "dark" | "light";

export interface Appearance {
  /** Background preset id, or "custom" (uses `customBackground`). */
  background: string;
  /** `#rrggbb` used when `background` is "custom". */
  customBackground: string;
  /** Interface font preset id, or "custom" (uses `uiFontCustom`). */
  uiFont: string;
  uiFontCustom: string;
  /** Code font (editor, result grid, SQL) preset id, or "custom". */
  monoFont: string;
  monoFontCustom: string;
  /** Code font size in px (editor; the grid is half a pixel smaller). */
  editorFontSize: number;
}

export const DEFAULT_APPEARANCE: Appearance = {
  background: "default",
  customBackground: "#1e2230",
  uiFont: "inter",
  uiFontCustom: "",
  monoFont: "jetbrains",
  monoFontCustom: "",
  editorFontSize: 13.5,
};

export const EDITOR_FONT_SIZES = { min: 10, max: 22 };

/** Surface colours of a theme. */
export interface Surface {
  bg: string;
  panel: string;
  panel2: string;
  hover: string;
  border: string;
  /** Text colours, only when the background needs other ones than the theme's. */
  text?: string;
  muted?: string;
}

export interface BackgroundPreset {
  id: string;
  label: string;
  dark: Surface;
  light: Surface;
}

/** `default` is index.css's palette (no variables are set for it). */
export const BACKGROUNDS: BackgroundPreset[] = [
  {
    id: "default",
    label: "Default",
    dark: { bg: "#0e0f13", panel: "#15161b", panel2: "#1b1c22", hover: "#23242c", border: "#272830" },
    light: { bg: "#f7f7f9", panel: "#ffffff", panel2: "#f1f2f5", hover: "#e9eaef", border: "#e3e4e8" },
  },
  {
    id: "graphite",
    label: "Graphite",
    dark: { bg: "#18181b", panel: "#1f1f23", panel2: "#27272a", hover: "#2f2f34", border: "#34343a" },
    light: { bg: "#f4f4f5", panel: "#fafafa", panel2: "#ececee", hover: "#e4e4e7", border: "#dcdce0" },
  },
  {
    id: "slate",
    label: "Slate blue",
    dark: { bg: "#0f172a", panel: "#131c31", panel2: "#1a2540", hover: "#22304f", border: "#273554" },
    light: { bg: "#f1f5f9", panel: "#f8fafc", panel2: "#e8eef5", hover: "#dfe7f0", border: "#d5dee9" },
  },
  {
    id: "forest",
    label: "Forest",
    dark: { bg: "#0f1512", panel: "#141c18", panel2: "#1a241f", hover: "#212d27", border: "#26332c" },
    light: { bg: "#f2f6f3", panel: "#fbfdfb", panel2: "#e9f0eb", hover: "#e0e9e3", border: "#d6e1d9" },
  },
  {
    id: "warm",
    label: "Warm paper",
    dark: { bg: "#17140f", panel: "#1e1a14", panel2: "#26211a", hover: "#2e2820", border: "#352e24" },
    light: { bg: "#f7f3ea", panel: "#fdfaf3", panel2: "#f0eadd", hover: "#e8e0cf", border: "#e0d6c2" },
  },
  {
    id: "contrast",
    label: "High contrast",
    dark: { bg: "#000000", panel: "#0a0a0a", panel2: "#141414", hover: "#1f1f1f", border: "#3a3a3a", text: "#ffffff", muted: "#b4b4b4" },
    light: { bg: "#ffffff", panel: "#ffffff", panel2: "#f2f2f2", hover: "#e6e6e6", border: "#9a9a9a", text: "#000000", muted: "#4a4a4a" },
  },
];

export interface FontPreset {
  id: string;
  label: string;
  /** CSS font-family list. */
  stack: string;
  /** Font checked to tell whether it is installed ("" = always there). */
  probe: string;
}

const SANS_FALLBACK = 'ui-sans-serif, system-ui, -apple-system, "Segoe UI", sans-serif';
const MONO_FALLBACK = 'ui-monospace, "SF Mono", Menlo, Consolas, monospace';

export const UI_FONTS: FontPreset[] = [
  { id: "inter", label: "Inter", stack: `"Inter", ${SANS_FALLBACK}`, probe: "Inter" },
  { id: "system", label: "System font", stack: SANS_FALLBACK, probe: "" },
  { id: "helvetica", label: "Helvetica Neue / Arial", stack: `"Helvetica Neue", Helvetica, Arial, ${SANS_FALLBACK}`, probe: "" },
  { id: "segoe", label: "Segoe UI", stack: `"Segoe UI", ${SANS_FALLBACK}`, probe: "Segoe UI" },
  { id: "roboto", label: "Roboto", stack: `"Roboto", ${SANS_FALLBACK}`, probe: "Roboto" },
  { id: "plex", label: "IBM Plex Sans", stack: `"IBM Plex Sans", ${SANS_FALLBACK}`, probe: "IBM Plex Sans" },
  { id: "noto", label: "Noto Sans", stack: `"Noto Sans", ${SANS_FALLBACK}`, probe: "Noto Sans" },
];

export const MONO_FONTS: FontPreset[] = [
  { id: "jetbrains", label: "JetBrains Mono", stack: `"JetBrains Mono", ${MONO_FALLBACK}`, probe: "JetBrains Mono" },
  { id: "system", label: "System monospace", stack: MONO_FALLBACK, probe: "" },
  { id: "sfmono", label: "SF Mono / Menlo", stack: `"SF Mono", Menlo, ${MONO_FALLBACK}`, probe: "" },
  { id: "fira", label: "Fira Code", stack: `"Fira Code", ${MONO_FALLBACK}`, probe: "Fira Code" },
  { id: "cascadia", label: "Cascadia Code", stack: `"Cascadia Code", ${MONO_FALLBACK}`, probe: "Cascadia Code" },
  { id: "source", label: "Source Code Pro", stack: `"Source Code Pro", ${MONO_FALLBACK}`, probe: "Source Code Pro" },
  { id: "plexmono", label: "IBM Plex Mono", stack: `"IBM Plex Mono", ${MONO_FALLBACK}`, probe: "IBM Plex Mono" },
  { id: "consolas", label: "Consolas", stack: `Consolas, ${MONO_FALLBACK}`, probe: "Consolas" },
];

/** A font name typed by the user, safe to put in a CSS font-family (quotes and CSS syntax removed). */
export function cleanFontName(name: string): string {
  return name
    .replace(/["'`;{}()<>\\]/g, "")
    .replace(/\s+/g, " ")
    .trim()
    .slice(0, 64);
}

function fontStack(presets: FontPreset[], id: string, custom: string, fallback: string): string {
  if (id === "custom") {
    const name = cleanFontName(custom);
    return name ? `"${name}", ${fallback}` : fallback;
  }
  return (presets.find((p) => p.id === id) ?? presets[0]).stack;
}

/** `#rgb` / `#rrggbb` → [r, g, b], or null. */
export function parseHex(hex: string): [number, number, number] | null {
  const m = /^#?([0-9a-f]{3}|[0-9a-f]{6})$/i.exec(hex.trim());
  if (!m) return null;
  const h = m[1].length === 3 ? [...m[1]].map((c) => c + c).join("") : m[1];
  return [0, 2, 4].map((i) => parseInt(h.slice(i, i + 2), 16)) as [number, number, number];
}

const toHex = (c: number[]) => "#" + c.map((v) => Math.round(Math.max(0, Math.min(255, v))).toString(16).padStart(2, "0")).join("");

/** Mix `a` toward `b` by `t` (0–1). */
function mix(a: [number, number, number], b: [number, number, number], t: number): string {
  return toHex(a.map((v, i) => v + (b[i] - v) * t));
}

/** WCAG relative luminance (0 = black, 1 = white). */
export function luminance(rgb: [number, number, number]): number {
  const [r, g, b] = rgb.map((v) => {
    const c = v / 255;
    return c <= 0.03928 ? c / 12.92 : ((c + 0.055) / 1.055) ** 2.4;
  });
  return 0.2126 * r + 0.7152 * g + 0.0722 * b;
}

export function contrast(a: [number, number, number], b: [number, number, number]): number {
  const [x, y] = [luminance(a), luminance(b)].sort((p, q) => q - p);
  return (x + 0.05) / (y + 0.05);
}

/**
 * Surfaces derived from one background colour: panels a little lighter on
 * dark backgrounds (darker on light ones), and text that stays readable
 * whatever the theme (light text on dark colours, dark text on light ones).
 */
export function deriveSurface(hex: string): Surface | null {
  const bg = parseHex(hex);
  if (!bg) return null;
  const dark = luminance(bg) < 0.18;
  const toward: [number, number, number] = dark ? [255, 255, 255] : [0, 0, 0];
  const panel = dark ? mix(bg, toward, 0.035) : mix(bg, [255, 255, 255], 0.6);
  return {
    bg: toHex(bg),
    panel,
    panel2: mix(bg, toward, dark ? 0.07 : 0.035),
    hover: mix(bg, toward, dark ? 0.11 : 0.07),
    border: mix(bg, toward, dark ? 0.14 : 0.1),
    // Whichever of near-white and near-black reads better on it.
    text: contrast(bg, [244, 245, 247]) >= contrast(bg, [28, 29, 33]) ? "#f4f5f7" : "#1c1d21",
    muted: dark ? mix(bg, [255, 255, 255], 0.6) : mix(bg, [0, 0, 0], 0.6),
  };
}

/** Whether a background colour suits the theme (a dark colour with the dark theme…). */
export function backgroundMatchesTheme(hex: string, theme: ThemeMode): boolean {
  const bg = parseHex(hex);
  return !bg || luminance(bg) < 0.18 === (theme === "dark");
}

/** Every variable an appearance sets on <html>. */
export const APPEARANCE_VARS = ["--bg", "--panel", "--panel-2", "--hover", "--border", "--text", "--muted", "--font-sans", "--font-mono", "--editor-font-size", "--grid-font-size"];

/** CSS variables for `a` in `theme`; defaults are left out (index.css has them). */
export function appearanceVars(a: Appearance, theme: ThemeMode): Record<string, string> {
  const out: Record<string, string> = {};
  const surface = a.background === "custom" ? deriveSurface(a.customBackground) : a.background !== "default" ? (BACKGROUNDS.find((b) => b.id === a.background)?.[theme] ?? null) : null;
  if (surface) {
    out["--bg"] = surface.bg;
    out["--panel"] = surface.panel;
    out["--panel-2"] = surface.panel2;
    out["--hover"] = surface.hover;
    out["--border"] = surface.border;
    if (surface.text) out["--text"] = surface.text;
    if (surface.muted) out["--muted"] = surface.muted;
  }
  if (a.uiFont !== DEFAULT_APPEARANCE.uiFont) out["--font-sans"] = fontStack(UI_FONTS, a.uiFont, a.uiFontCustom, SANS_FALLBACK);
  if (a.monoFont !== DEFAULT_APPEARANCE.monoFont) out["--font-mono"] = fontStack(MONO_FONTS, a.monoFont, a.monoFontCustom, MONO_FALLBACK);
  const size = clampFontSize(a.editorFontSize);
  if (size !== DEFAULT_APPEARANCE.editorFontSize) {
    out["--editor-font-size"] = `${size}px`;
    out["--grid-font-size"] = `${size - 1}px`;
  }
  return out;
}

export function clampFontSize(n: number): number {
  if (!Number.isFinite(n)) return DEFAULT_APPEARANCE.editorFontSize;
  return Math.min(EDITOR_FONT_SIZES.max, Math.max(EDITOR_FONT_SIZES.min, Math.round(n * 2) / 2));
}

/** Saved setting → appearance (unknown or missing fields get defaults). */
export function parseAppearance(v: unknown): Appearance {
  const o = (v && typeof v === "object" ? v : {}) as Partial<Record<keyof Appearance, unknown>>;
  const str = (x: unknown, d: string) => (typeof x === "string" ? x : d);
  const known = (x: unknown, ids: string[], d: string) => (typeof x === "string" && (ids.includes(x) || x === "custom") ? x : d);
  return {
    background: known(o.background, BACKGROUNDS.map((b) => b.id), DEFAULT_APPEARANCE.background),
    customBackground: parseHex(str(o.customBackground, "")) ? str(o.customBackground, "") : DEFAULT_APPEARANCE.customBackground,
    uiFont: known(o.uiFont, UI_FONTS.map((f) => f.id), DEFAULT_APPEARANCE.uiFont),
    uiFontCustom: cleanFontName(str(o.uiFontCustom, "")),
    monoFont: known(o.monoFont, MONO_FONTS.map((f) => f.id), DEFAULT_APPEARANCE.monoFont),
    monoFontCustom: cleanFontName(str(o.monoFontCustom, "")),
    editorFontSize: clampFontSize(typeof o.editorFontSize === "number" ? o.editorFontSize : DEFAULT_APPEARANCE.editorFontSize),
  };
}

/** Put `a` on <html> (removing variables it doesn't set). */
export function applyAppearance(a: Appearance, theme: ThemeMode) {
  if (typeof document === "undefined") return;
  const style = document.documentElement.style;
  const vars = appearanceVars(a, theme);
  for (const k of APPEARANCE_VARS) {
    if (vars[k]) style.setProperty(k, vars[k]);
    else style.removeProperty(k);
  }
}

/**
 * Whether a font is installed: text in it measures differently from the
 * generic fallbacks. `null` when it can't be told (no canvas).
 */
export function fontInstalled(name: string): boolean | null {
  if (!name || typeof document === "undefined") return null;
  const ctx = document.createElement("canvas").getContext("2d");
  if (!ctx) return null;
  const sample = "mmmmmmmmmwwwwwwwlli1|0O";
  return ["monospace", "serif", "sans-serif"].some((generic) => {
    ctx.font = `32px ${generic}`;
    const base = ctx.measureText(sample).width;
    ctx.font = `32px "${cleanFontName(name)}", ${generic}`;
    return ctx.measureText(sample).width !== base;
  });
}
