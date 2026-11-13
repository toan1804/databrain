import { useEffect, useState } from "react";
import { api, toError } from "../lib/api";
import type { ResultsMemory } from "../lib/types";
import { formatBytes } from "../lib/util";
import { useStore } from "../store";

const MB = 1024 * 1024;

/** Settings → Queries: the memory query results may use (automatic or a chosen size). */
export function ResultsMemorySettings() {
  const toast = useStore((s) => s.toast);
  const [m, setM] = useState<ResultsMemory | null>(null);
  const [text, setText] = useState("");
  useEffect(() => {
    api
      .resultsMemory()
      .then((r) => {
        setM(r);
        setText(String(r.custom_mb ?? Math.round(r.auto / MB)));
      })
      .catch(() => {});
  }, []);
  if (!m) return null;
  const custom = m.custom_mb !== null;
  const maxMb = m.total ? Math.floor(m.total / MB) : null;
  const n = Number(text);
  const valid = Number.isInteger(n) && n >= 256 && (maxMb === null || n <= maxMb);
  const save = async (mb: number | null) => {
    try {
      const r = await api.setResultsMemory(mb);
      setM(r);
      if (mb === null) setText(String(Math.round(r.auto / MB)));
    } catch (e) {
      toast(toError(e).message, "error");
    }
  };
  return (
    <>
      <h3 className="pt-3 text-[13px] font-medium">Memory for results</h3>
      <div className="space-y-1.5 text-[12px]" role="radiogroup" aria-label="Memory for results">
        <label className="flex items-center gap-2">
          <input type="radio" name="results-memory" checked={!custom} onChange={() => void save(null)} />
          Automatic: {formatBytes(m.auto)}
          {m.total ? <span className="text-muted">(a quarter of this computer's {formatBytes(m.total)})</span> : null}
        </label>
        <label className="flex items-center gap-2">
          <input type="radio" name="results-memory" checked={custom} onChange={() => valid && void save(n)} />
          <input
            className={`field h-7 w-24 py-0 ${valid ? "" : "border-danger"}`}
            inputMode="numeric"
            aria-label="Memory for results in MB"
            aria-invalid={!valid}
            value={text}
            onChange={(e) => setText(e.target.value)}
            onBlur={() => custom && valid && n !== m.custom_mb && void save(n)}
            onKeyDown={(e) => e.key === "Enter" && valid && void save(n)}
          />
          MB
          {!valid && <span className="text-danger">256 – {maxMb ?? "…"}</span>}
        </label>
      </div>
      <p className="text-[11.5px] text-muted">
        Results are kept in memory so the grid, sorting and filters are fast; {formatBytes(m.used)} of {formatBytes(m.budget)} is in use now. One
        result keeps at most half, and stops early (keeping the rows it has) when the computer runs low on free memory. When results need more,
        the least recently used older ones free their rows (a tab's latest result and pinned ones stay). Export or "Load into…" get every row
        without keeping them in memory.
      </p>
    </>
  );
}
