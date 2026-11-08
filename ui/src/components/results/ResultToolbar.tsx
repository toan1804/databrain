import type { Dispatch, SetStateAction } from "react";
import { BarChart3, Braces, Download, Filter, GitCompare, ListFilter, Loader2, Pencil, Search, Sparkles, Table2, Upload, X } from "lucide-react";
import type { SendInput } from "../../aiStore";
import { OutputChip } from "../OutputChip";
import { GridViewsMenu } from "../GridViewsMenu";
import { tableLabel } from "../../lib/gridViews";
import { mentionInAi, outputLabel } from "../../outputs";
import type { EditTarget, ResultInfo, ViewSpec } from "../../lib/types";
import { filterLabel } from "../../lib/util";
import { useStore, type StatementRun } from "../../store";
import { kbd } from "../../lib/platform";

/** The result's toolbar: view mode, row filters, sort, find, value panel, AI, edit, load and export. */
export function ResultToolbar({
  stmt,
  info,
  tabId,
  connId,
  mode,
  setMode,
  quick,
  setQuick,
  sourceKey,
  keptQuick,
  view,
  setView,
  openFind,
  valueOpen,
  setValueOpen,
  send,
  target,
  checkingEdit,
  startEditing,
  stopEditing,
  setLoadOpen,
  setExportOpen,
}: {
  stmt: Pick<StatementRun, "durationMs" | "output">;
  info: ResultInfo;
  tabId: string;
  connId: string | null | undefined;
  mode: "grid" | "chart";
  setMode: Dispatch<SetStateAction<"grid" | "chart">>;
  quick: string;
  setQuick: Dispatch<SetStateAction<string>>;
  sourceKey: string | null;
  keptQuick: string | undefined;
  view: ViewSpec;
  setView: Dispatch<SetStateAction<ViewSpec>>;
  openFind: () => void;
  valueOpen: boolean;
  setValueOpen: Dispatch<SetStateAction<boolean>>;
  send: (input: SendInput) => Promise<void>;
  target: EditTarget | null;
  checkingEdit: boolean;
  startEditing: () => Promise<void>;
  stopEditing: () => void;
  setLoadOpen: Dispatch<SetStateAction<boolean>>;
  setExportOpen: Dispatch<SetStateAction<boolean>>;
}) {
  return (
    <div className="flex h-10 shrink-0 items-center gap-2 border-b border-line px-2">
      {stmt.output && <OutputChip output={stmt.output} />}
      <div className="flex shrink-0 rounded-md border border-line p-0.5" role="radiogroup" aria-label="View">
        {(["grid", "chart"] as const).map((m) => (
          <button
            key={m}
            role="radio"
            aria-checked={mode === m}
            title={m === "grid" ? "Table" : "Chart"}
            onClick={() => setMode(m)}
            className={`flex h-5 w-6 items-center justify-center rounded ${mode === m ? "bg-hover text-fg" : "text-muted hover:text-fg"}`}
          >
            {m === "grid" ? <Table2 size={12} /> : <BarChart3 size={12} />}
          </button>
        ))}
      </div>
      <div className="relative w-56 min-w-28 shrink">
        <ListFilter size={13} className="absolute left-2 top-1/2 -translate-y-1/2 text-muted" />
        <input
          className="field py-1 pl-7 pr-6"
          placeholder="Filter rows…"
          aria-label="Filter rows"
          title={
            sourceKey && quick.trim() && keptQuick === quick.trim()
              ? `Kept for ${tableLabel(sourceKey)}: shown again the next time its rows are. Clear it to forget it.`
              : sourceKey
                ? `Rows containing this text in any column. Kept for ${tableLabel(sourceKey)}.`
                : "Rows containing this text in any column"
          }
          value={quick}
          onChange={(e) => setQuick(e.target.value)}
        />
        {quick && (
          <button className="absolute right-1.5 top-1/2 -translate-y-1/2 text-muted hover:text-fg" aria-label="Clear filter" onClick={() => setQuick("")}>
            <X size={12} />
          </button>
        )}
      </div>
      <div className="flex min-w-0 flex-1 items-center gap-1 overflow-x-auto">
        {view.filters.map((f, i) => (
          <span key={i} className="flex shrink-0 items-center gap-1 rounded-md bg-accent/12 px-2 py-0.5 text-[11.5px] text-accent">
            <Filter size={10} />
            {filterLabel(f, info.columns[f.column]?.name ?? `#${f.column}`)}
            <button
              aria-label="Remove filter"
              className="opacity-70 hover:opacity-100"
              onClick={() => setView((v) => ({ ...v, filters: v.filters.filter((_, j) => j !== i) }))}
            >
              <X size={11} />
            </button>
          </span>
        ))}
        {view.sort.length > 0 && (
          <button className="shrink-0 text-[11.5px] text-muted hover:text-fg" onClick={() => setView((v) => ({ ...v, sort: [] }))}>
            Clear sort
          </button>
        )}
      </div>
      <GridViewsMenu
        info={info}
        view={view}
        sourceKey={sourceKey}
        onApply={(v) => {
          setView(v);
          setQuick(v.quick_filter ?? "");
        }}
      />
      <button className="icon-btn shrink-0" title={kbd("Find (⌘F)")} aria-label="Find in results" onClick={openFind}>
        <Search size={14} />
      </button>
      <button
        className={`icon-btn shrink-0 ${valueOpen ? "text-accent" : ""}`}
        title={kbd("Value panel: the selected cell's whole value (JSON, XML, binary, long text) (⇧Space)")}
        aria-label="Value panel"
        aria-pressed={valueOpen}
        onClick={() => setValueOpen(!valueOpen)}
      >
        <Braces size={14} />
      </button>
      <button
        className="btn-ghost shrink-0 whitespace-nowrap border border-line py-1"
        aria-label="Ask AI"
        title={stmt.output ? `Open the AI assistant with @${outputLabel(stmt.output)} mentioned, then ask your own question` : "Ask the AI to analyze this result"}
        onClick={() => {
          if (stmt.output) {
            mentionInAi(stmt.output);
            return;
          }
          void send({
            message: "Analyze this result and summarize the key findings.",
            mode: "analyze_result",
            targetKey: tabId,
            connectionId: connId ?? undefined,
            context: { result_id: info.id, mentions: [] },
          });
        }}
      >
        <Sparkles size={13} /> <span className="hidden @3xl:inline">Ask AI</span>
      </button>
      <button
        className={`btn-ghost shrink-0 whitespace-nowrap border py-1 ${target ? "border-accent text-accent" : "border-line"}`}
        aria-pressed={!!target}
        aria-label="Edit"
        disabled={checkingEdit}
        aria-busy={checkingEdit}
        onClick={() => (target ? stopEditing() : void startEditing())}
        title="Edit cells, add and delete rows of the table this result comes from (needs a primary key)"
      >
        {checkingEdit ? <Loader2 size={13} className="animate-spin" /> : <Pencil size={13} />} <span className="hidden @3xl:inline">Edit</span>
      </button>
      <button
        className="btn-ghost shrink-0 whitespace-nowrap border border-line py-1"
        aria-label="Load into…"
        onClick={() => setLoadOpen(true)}
        title="Copy these rows into a table of any connection (insert, replace, update or merge by key)"
      >
        <Upload size={13} /> <span className="hidden @3xl:inline">Load into…</span>
      </button>
      {stmt.output && (
        <button
          className="btn-ghost shrink-0 whitespace-nowrap border border-line py-1"
          aria-label="Compare with…"
          title="Compare this result with another result, a table or a file"
          onClick={() => stmt.output && useStore.getState().openCompareDialog({ initial: stmt.output })}
        >
          <GitCompare size={13} /> <span className="hidden @3xl:inline">Compare with…</span>
        </button>
      )}
      <button
        className="btn-ghost shrink-0 whitespace-nowrap border border-line py-1"
        aria-label="Export"
        title="Export"
        onClick={() => setExportOpen(true)}
      >
        <Download size={13} /> <span className="hidden @3xl:inline">Export</span>
      </button>
    </div>
  );
}
