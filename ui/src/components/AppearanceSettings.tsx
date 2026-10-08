// Settings → Appearance: theme, background, interface and code fonts.
import { useEffect, useMemo, useState } from "react";
import { Check, Moon, RotateCcw, Sun } from "lucide-react";
import {
  BACKGROUNDS,
  DEFAULT_APPEARANCE,
  EDITOR_FONT_SIZES,
  MONO_FONTS,
  UI_FONTS,
  backgroundMatchesTheme,
  cleanFontName,
  deriveSurface,
  fontInstalled,
  parseHex,
  type FontPreset,
} from "../lib/appearance";
import { useStore } from "../store";

const heading = "text-[13px] font-medium";
const hint = "text-[11.5px] text-muted";

export function AppearanceSettings() {
  const theme = useStore((s) => s.theme);
  const setTheme = useStore((s) => s.setTheme);
  const a = useStore((s) => s.appearance);
  const set = useStore((s) => s.setAppearance);
  const [hex, setHex] = useState(a.customBackground);
  useEffect(() => setHex(a.customBackground), [a.customBackground]);
  const custom = a.background === "custom";
  const mismatch = custom && !backgroundMatchesTheme(a.customBackground, theme);

  return (
    <div className="max-w-xl space-y-5 text-[12.5px]">
      <section className="space-y-2">
        <h3 className={heading}>Theme</h3>
        <div className="flex gap-2" role="radiogroup" aria-label="Theme">
          {(["dark", "light"] as const).map((t) => (
            <button
              key={t}
              role="radio"
              aria-checked={theme === t}
              className={`flex items-center gap-1.5 rounded-md border px-3 py-1.5 ${theme === t ? "border-accent bg-accent/10 text-accent" : "border-line text-muted hover:text-fg"}`}
              onClick={() => setTheme(t)}
            >
              {t === "dark" ? <Moon size={13} /> : <Sun size={13} />} {t === "dark" ? "Dark" : "Light"}
            </button>
          ))}
        </div>
      </section>

      <section className="space-y-2">
        <h3 className={heading}>Background</h3>
        <div className="grid grid-cols-4 gap-2" role="radiogroup" aria-label="Background">
          {BACKGROUNDS.map((b) => {
            const s = b[theme];
            const on = a.background === b.id;
            return (
              <button
                key={b.id}
                role="radio"
                aria-checked={on}
                title={b.label}
                className={`overflow-hidden rounded-md border text-left ${on ? "border-accent ring-2 ring-accent/40" : "border-line hover:border-muted"}`}
                onClick={() => set({ background: b.id })}
              >
                <Swatch bg={s.bg} panel={s.panel} panel2={s.panel2} border={s.border} text={s.text} />
                <div className="flex items-center gap-1 px-1.5 py-1 text-[11.5px]">
                  {on && <Check size={11} className="shrink-0 text-accent" />}
                  <span className="truncate">{b.label}</span>
                </div>
              </button>
            );
          })}
          <button
            role="radio"
            aria-checked={custom}
            className={`overflow-hidden rounded-md border text-left ${custom ? "border-accent ring-2 ring-accent/40" : "border-line hover:border-muted"}`}
            onClick={() => set({ background: "custom" })}
          >
            {(() => {
              const s = deriveSurface(a.customBackground);
              return s ? <Swatch bg={s.bg} panel={s.panel} panel2={s.panel2} border={s.border} text={s.text} /> : <div className="h-12" />;
            })()}
            <div className="flex items-center gap-1 px-1.5 py-1 text-[11.5px]">
              {custom && <Check size={11} className="shrink-0 text-accent" />}
              <span className="truncate">Custom colour</span>
            </div>
          </button>
        </div>
        {custom && (
          <div className="flex items-center gap-2">
            <label htmlFor="custom-bg" className="text-muted">
              Colour
            </label>
            <input
              id="custom-bg"
              type="color"
              className="h-7 w-10 cursor-pointer rounded border border-line bg-transparent"
              value={parseHex(a.customBackground) ? a.customBackground : "#000000"}
              onChange={(e) => set({ customBackground: e.target.value })}
            />
            <input
              aria-label="Colour as hex"
              className={`field w-28 font-mono ${parseHex(hex) ? "" : "border-danger"}`}
              value={hex}
              onChange={(e) => {
                setHex(e.target.value);
                const v = e.target.value.trim();
                if (parseHex(v)) set({ customBackground: v.startsWith("#") ? v : `#${v}` });
              }}
            />
            <span className={hint}>Panels, borders and text are derived from it so text stays readable.</span>
          </div>
        )}
        {mismatch && (
          <div className="text-[11.5px] text-warning">
            This colour is {theme === "dark" ? "light" : "dark"} while the theme is {theme}: text and panels follow the colour, but highlights and syntax colours are made for the {theme} theme.{" "}
            <button className="underline" onClick={() => setTheme(theme === "dark" ? "light" : "dark")}>
              Switch to the {theme === "dark" ? "light" : "dark"} theme
            </button>
          </div>
        )}
        <p className={hint}>Each preset has a dark and a light version; it follows the theme.</p>
      </section>

      <section className="space-y-2">
        <h3 className={heading}>Fonts</h3>
        <FontPicker
          id="ui-font"
          label="Interface"
          presets={UI_FONTS}
          value={a.uiFont}
          custom={a.uiFontCustom}
          onPick={(uiFont) => set({ uiFont })}
          onCustom={(uiFontCustom) => set({ uiFontCustom })}
          preview="Connections · Run query · 1,204 rows"
          mono={false}
        />
        <FontPicker
          id="mono-font"
          label="Code (editor, results, SQL)"
          presets={MONO_FONTS}
          value={a.monoFont}
          custom={a.monoFontCustom}
          onPick={(monoFont) => set({ monoFont })}
          onCustom={(monoFontCustom) => set({ monoFontCustom })}
          preview="select o.id, sum(o.amount) from sales.orders o -- 0O 1lI"
          mono
        />
        <div className="flex items-center gap-2">
          <label htmlFor="editor-size" className="w-44 shrink-0 text-muted">
            Code font size
          </label>
          <input
            id="editor-size"
            type="range"
            min={EDITOR_FONT_SIZES.min}
            max={EDITOR_FONT_SIZES.max}
            step={0.5}
            value={a.editorFontSize}
            onChange={(e) => set({ editorFontSize: Number(e.target.value) })}
            className="flex-1 accent-[var(--accent)]"
          />
          <span className="w-12 text-right font-mono">{a.editorFontSize}px</span>
        </div>
        <p className={hint}>Fonts must be installed on this computer; a font that isn't falls back to the system font. The result grid uses the code font, 1px smaller.</p>
      </section>

      <button
        className="btn-ghost border border-line"
        disabled={JSON.stringify(a) === JSON.stringify(DEFAULT_APPEARANCE)}
        onClick={() => set({ ...DEFAULT_APPEARANCE })}
      >
        <RotateCcw size={13} /> Reset appearance
      </button>
    </div>
  );
}

function Swatch({ bg, panel, panel2, border, text }: { bg: string; panel: string; panel2: string; border: string; text?: string }) {
  const fg = text ?? (parseHex(bg) && deriveSurface(bg)?.text) ?? "#888";
  return (
    <div className="flex h-12 gap-1 p-1.5" style={{ background: bg }} aria-hidden>
      <div className="w-1/4 rounded-sm" style={{ background: panel2, border: `1px solid ${border}` }} />
      <div className="flex-1 space-y-1 rounded-sm p-1" style={{ background: panel, border: `1px solid ${border}` }}>
        <div className="h-1 w-3/4 rounded-full" style={{ background: fg, opacity: 0.85 }} />
        <div className="h-1 w-1/2 rounded-full" style={{ background: fg, opacity: 0.45 }} />
      </div>
    </div>
  );
}

function FontPicker({
  id,
  label,
  presets,
  value,
  custom,
  onPick,
  onCustom,
  preview,
  mono,
}: {
  id: string;
  label: string;
  presets: FontPreset[];
  value: string;
  custom: string;
  onPick: (id: string) => void;
  onCustom: (name: string) => void;
  preview: string;
  mono: boolean;
}) {
  const [name, setName] = useState(custom);
  useEffect(() => setName(custom), [custom]);
  // Which presets are installed (checked once per open).
  const installed = useMemo(() => new Map(presets.map((p) => [p.id, p.probe ? fontInstalled(p.probe) : true])), [presets]);
  const isCustom = value === "custom";
  const customOk = isCustom && cleanFontName(name) ? fontInstalled(cleanFontName(name)) : null;
  const stack = isCustom ? `"${cleanFontName(name)}", ${mono ? "monospace" : "sans-serif"}` : (presets.find((p) => p.id === value) ?? presets[0]).stack;
  return (
    <div className="space-y-1">
      <div className="flex items-center gap-2">
        <label htmlFor={id} className="w-44 shrink-0 text-muted">
          {label}
        </label>
        <select id={id} className="field flex-1" value={value} onChange={(e) => onPick(e.target.value)}>
          {presets.map((p) => (
            <option key={p.id} value={p.id}>
              {p.label}
              {installed.get(p.id) === false ? " (not installed)" : ""}
            </option>
          ))}
          <option value="custom">Other font…</option>
        </select>
      </div>
      {isCustom && (
        <div className="flex items-center gap-2 pl-[11.5rem]">
          <input
            aria-label={`${label} font name`}
            className="field flex-1"
            placeholder={mono ? "e.g. Iosevka" : "e.g. Open Sans"}
            value={name}
            onChange={(e) => setName(e.target.value)}
            onBlur={() => onCustom(cleanFontName(name))}
            onKeyDown={(e) => e.key === "Enter" && onCustom(cleanFontName(name))}
          />
          {customOk === false && <span className="text-[11.5px] text-warning">not found on this computer</span>}
        </div>
      )}
      <div className="ml-[11.5rem] truncate rounded-md border border-line bg-panel-2 px-2 py-1" style={{ fontFamily: stack, fontSize: 13 }}>
        {preview}
      </div>
    </div>
  );
}
