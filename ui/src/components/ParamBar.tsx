import { useEffect, useRef, useState } from "react";
import { Braces, Variable } from "lucide-react";
import { api, isTauri } from "../lib/api";
import type { ConnectorKind } from "../lib/types";
import { useStore } from "../store";

/**
 * One box per `:name` parameter of a query tab or notebook (`tabId`).
 * Values are typed as plain text: the engine quotes text (`2026-09-09` →
 * `'2026-09-09'`), keeps numbers (`2000`) and literals already quoted. The
 * `{}` toggle inside a box inserts the value as an SQL expression instead
 * (`current_date - 1`). Hovering a box shows what goes into the query.
 * At most three rows tall; more parameters scroll.
 */
export function ParamBar({ tabId, kind }: { tabId: string; kind: ConnectorKind | undefined }) {
  const names = useStore((s) => s.paramNames[tabId]);
  const values = useStore((s) => s.params[tabId]);
  const focus = useStore((s) => (s.paramFocus?.tabId === tabId ? s.paramFocus : null));
  const setParam = useStore((s) => s.setParam);
  const [preview, setPreview] = useState<Record<string, string | null>>({});
  const inputs = useRef<Record<string, HTMLInputElement | null>>({});

  useEffect(() => {
    if (!names?.length || !isTauri()) return;
    const v = Object.fromEntries(names.map((n) => [n, values?.[n] ?? { value: "" }]));
    let alive = true;
    const t = setTimeout(() => {
      api
        .previewParameters(kind ?? "postgres", v)
        .then((p) => alive && setPreview(p))
        .catch(() => {});
    }, 150);
    return () => {
      alive = false;
      clearTimeout(t);
    };
  }, [names, values, kind]);

  useEffect(() => {
    if (!focus) return;
    const el = inputs.current[focus.name];
    el?.scrollIntoView({ block: "nearest" });
    el?.focus();
  }, [focus]);

  if (!names?.length) return null;
  return (
    <div className="flex shrink-0 items-start gap-2 border-b border-line bg-panel-2/50 px-2 py-1" role="group" aria-label="Query parameters">
      <span className="flex h-6 shrink-0 items-center text-muted" title={`Parameters (:name) in this ${tabId.startsWith("nb") ? "notebook" : "query"}`}>
        <Variable size={13} aria-hidden />
        <span className="sr-only">Parameters</span>
      </span>
      {/* Three rows of boxes (24px + 4px gaps), then scroll. */}
      <div className="flex max-h-[80px] min-w-0 flex-1 flex-wrap content-start gap-1 overflow-y-auto overscroll-contain">
        {names.map((n) => {
          const v = values?.[n] ?? { value: "" };
          const empty = !v.value.trim();
          const shown = preview[n];
          const id = `param-${tabId}-${n}`;
          const tip = empty ? `:${n} needs a value` : `:${n} → ${shown ?? v.value}${v.raw ? " (SQL expression)" : ""}`;
          return (
            <div key={n} className="group flex h-6 items-center gap-1" title={tip}>
              <label htmlFor={id} className="font-mono text-[12px] text-accent">
                :{n}
              </label>
              <div className="relative">
                <input
                  id={id}
                  ref={(el) => {
                    inputs.current[n] = el;
                  }}
                  className={`field h-6 w-32 py-0 pr-5 font-mono text-[12px] ${empty ? "border-dashed border-warning" : ""} ${v.raw ? "text-accent" : ""}`}
                  placeholder={v.raw ? "SQL" : "value"}
                  value={v.value}
                  aria-invalid={empty}
                  aria-label={`Value of :${n}`}
                  onChange={(e) => setParam(tabId, n, { value: e.target.value })}
                  spellCheck={false}
                />
                <button
                  type="button"
                  className={`absolute right-0.5 top-1/2 -translate-y-1/2 rounded p-0.5 ${
                    v.raw ? "text-accent" : "text-muted opacity-0 hover:text-fg focus:opacity-100 group-hover:opacity-100"
                  }`}
                  aria-pressed={!!v.raw}
                  aria-label={`Insert :${n} as an SQL expression`}
                  title={v.raw ? "Inserted as an SQL expression (no quotes). Click to use it as a value." : "Insert as an SQL expression (no quotes added)"}
                  onClick={() => setParam(tabId, n, { raw: !v.raw })}
                >
                  <Braces size={11} />
                </button>
              </div>
            </div>
          );
        })}
      </div>
    </div>
  );
}
