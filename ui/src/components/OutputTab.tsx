import { useEffect, useState } from "react";
import { Loader2, RefreshCw } from "lucide-react";
import { api, toError } from "../lib/api";
import type { OutputInfo, ResultInfo } from "../lib/types";
import { formatCount, sqlPreview } from "../lib/util";
import { useStore } from "../store";
import { outputRef } from "../outputs";
import { ResultView } from "./ResultsPanel";

/** Shows a stored output (by handle) without re-running its query. */
export function OutputTab({ tabId, reference, visible }: { tabId: string; reference: string; visible: boolean }) {
  const live = useStore((s) => s.outputs.find((o) => o.handle === reference));
  const [state, setState] = useState<{ output: OutputInfo; info: ResultInfo } | { error: string } | null>(null);
  const newTab = useStore((s) => s.newTab);
  const runTab = useStore((s) => s.runTab);

  useEffect(() => {
    let alive = true;
    (async () => {
      try {
        const output = await api.loadOutput(reference);
        const info = await api.resultInfo(output.result_id);
        if (alive) setState({ output, info });
      } catch (e) {
        if (alive) setState({ error: toError(e).message });
      }
    })();
    return () => {
      alive = false;
    };
  }, [reference]);

  const rerun = (o: OutputInfo) => {
    const id = newTab({ title: o.name ?? o.handle, sql: o.sql, connection_id: o.connection_id });
    void runTab(id, "all", { doc: o.sql, selFrom: 0, selTo: 0, cursor: 0 });
  };

  return (
    <div className="h-full min-h-0 flex-col" style={{ display: visible ? "flex" : "none" }}>
      {!state ? (
        <div className="flex h-full items-center justify-center text-muted">
          <Loader2 size={18} className="animate-spin" />
        </div>
      ) : "error" in state ? (
        <div className="p-6 text-[13px]">
          <p className="text-danger">{state.error}</p>
          {live && (
            <button className="btn-ghost mt-3 border border-line" onClick={() => rerun(live)}>
              <RefreshCw size={13} /> Re-run its query on {live.connection_name}
            </button>
          )}
        </div>
      ) : (
        <>
          <div className="flex h-8 shrink-0 items-center gap-2 border-b border-line bg-panel-2 px-3 text-[11.5px] text-muted">
            <span className="font-mono text-fg">{outputRef(live ?? state.output)}</span>
            <span>from {state.output.connection_name}</span>
            <span>· {formatCount(state.output.rows)} rows</span>
            <span className="min-w-0 flex-1 truncate font-mono" title={state.output.sql}>
              · {sqlPreview(state.output.sql, 140)}
            </span>
            <button className="btn-ghost h-6 py-0 text-[11.5px]" onClick={() => rerun(state.output)}>
              <RefreshCw size={11} /> Re-run
            </button>
          </div>
          <div className="min-h-0 flex-1">
            <ResultView stmt={{ output: live ?? state.output }} info={state.info} tabId={tabId} connectionId={state.output.connection_id} title={state.output.name ?? state.output.handle} />
          </div>
        </>
      )}
    </div>
  );
}
