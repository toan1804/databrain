import { useCallback, useEffect, useRef, useState, type ReactNode } from "react";
import { AlertCircle, Ban, CheckCircle2, Sparkles, History, Loader2, Play, Plug, SquarePen, Trash2, Workflow } from "lucide-react";
import { api, toError } from "../../lib/api";
import type { HistoryClear, HistoryEntry, RunStatus } from "../../lib/types";
import { formatCount, formatDuration, relativeTime, sqlPreview } from "../../lib/util";
import { openOutput } from "../../outputs";
import { useStore } from "../../store";
import { MenuItem, MenuSeparator, Modal, Popover } from "../ui";
import { PanelHeader, SearchBox } from "./primitives";

// ------------------------------------------------------------------ history

/** Output produced by a history entry: opens it if it is still available. */
function HistoryOutput({ handle }: { handle: string }) {
  const o = useStore((s) => s.outputs.find((x) => x.handle === handle));
  const available = !!o && o.state !== "evicted";
  return (
    <button
      className={`ml-auto shrink-0 rounded border px-1 font-mono text-[10px] ${available ? "border-accent/40 text-accent hover:bg-accent/10" : "border-line text-muted"}`}
      title={available ? `Open output ${handle} (no re-run)` : `Output ${handle} is no longer in memory`}
      disabled={!available}
      onClick={(e) => {
        e.stopPropagation();
        if (o) void openOutput(o);
      }}
    >
      {o?.name ?? handle}
    </button>
  );
}

/** Open `sql` in a new tab on `connectionId` and run it (all statements). */
function runAgain(sql: string, connectionId: string | null) {
  const st = useStore.getState();
  const id = st.newTab({ sql, connection_id: connectionId ?? undefined });
  void st.runTab(id, "all", { doc: sql, selFrom: 0, selTo: 0, cursor: 0 });
}

export function HistoryPanel() {
  const version = useStore((s) => s.historyVersion);
  const newTab = useStore((s) => s.newTab);
  const toast = useStore((s) => s.toast);
  const connections = useStore((s) => s.connections);
  const [search, setSearch] = useState("");
  // Filters: one connection, how the run ended. Remembered for the session.
  const [connFilter, setConnFilter] = useState("");
  const [statusFilter, setStatusFilter] = useState<RunStatus | "">("");
  const [menu, setMenu] = useState<{ x: number; y: number; entry: HistoryEntry } | null>(null);
  const [items, setItems] = useState<HistoryEntry[]>([]);
  const [loading, setLoading] = useState(false);
  // AI queries (assistant, MCP) are hidden unless asked for; remembered.
  const [showAi, setShowAi] = useState(() => localStorage.getItem("db.history.showAi") === "1");
  const toggleAi = () =>
    setShowAi((v) => {
      localStorage.setItem("db.history.showAi", v ? "0" : "1");
      return !v;
    });

  // 50 at a time; scrolling near the end loads the next 50.
  const [more, setMore] = useState(true);
  const [clearing, setClearing] = useState(false);
  const busy = useRef(false);
  const query = useCallback(
    (before: number | null) =>
      api.listHistory({ search: search || null, limit: HISTORY_PAGE, before, exclude_ai: !showAi, connection_id: connFilter || null, status: statusFilter || undefined }),
    [search, showAi, connFilter, statusFilter],
  );

  const load = useCallback(async () => {
    setLoading(true);
    busy.current = true;
    try {
      const page = await query(null);
      setItems(page);
      setMore(page.length === HISTORY_PAGE);
    } catch (e) {
      toast(toError(e).message, "error");
    } finally {
      busy.current = false;
      setLoading(false);
    }
  }, [query, toast]);

  const loadMore = useCallback(async () => {
    if (busy.current || !more || items.length === 0) return;
    busy.current = true;
    setLoading(true);
    try {
      const page = await query(items[items.length - 1].id);
      setItems((cur) => [...cur, ...page.filter((p) => !cur.some((c) => c.id === p.id))]);
      setMore(page.length === HISTORY_PAGE);
    } catch (e) {
      toast(toError(e).message, "error");
    } finally {
      busy.current = false;
      setLoading(false);
    }
  }, [items, more, query, toast]);

  useEffect(() => {
    const t = setTimeout(() => void load(), 150);
    return () => clearTimeout(t);
  }, [load, version]);

  return (
    <>
      <PanelHeader title="History">
        {loading && <Loader2 size={13} className="mr-1 animate-spin text-muted" />}
        <button
          className={`icon-btn ${showAi ? "text-accent" : ""}`}
          title={showAi ? "Showing queries run by the AI assistant: click to hide them" : "Show queries run by the AI assistant"}
          aria-label="Include AI queries"
          aria-pressed={showAi}
          onClick={toggleAi}
        >
          <Sparkles size={14} />
        </button>
        <button className="icon-btn" title="Clear history…" aria-label="Clear history" onClick={() => setClearing(true)}>
          <Trash2 size={14} />
        </button>
      </PanelHeader>
      <SearchBox value={search} onChange={setSearch} placeholder="Search SQL, errors, connections" />
      <div className="flex gap-1 px-2 pb-1.5">
        <select className="field h-7 min-w-0 flex-1 py-0 text-[11.5px]" aria-label="Connection" value={connFilter} onChange={(e) => setConnFilter(e.target.value)}>
          <option value="">All connections</option>
          {connections.map((c) => (
            <option key={c.id} value={c.id}>
              {c.name}
            </option>
          ))}
        </select>
        <select className="field h-7 w-[92px] py-0 text-[11.5px]" aria-label="Status" value={statusFilter} onChange={(e) => setStatusFilter(e.target.value as RunStatus | "")}>
          <option value="">Any status</option>
          <option value="success">Succeeded</option>
          <option value="error">Failed</option>
          <option value="cancelled">Stopped</option>
        </select>
      </div>
      {menu && (
        <Popover x={menu.x} y={menu.y} onClose={() => setMenu(null)} className="max-h-[60vh] overflow-auto">
          <MenuItem
            icon={<SquarePen size={13} />}
            label="Open in a new tab"
            onClick={() => {
              setMenu(null);
              newTab({ sql: menu.entry.sql, connection_id: menu.entry.connection_id ?? undefined });
            }}
          />
          <MenuItem
            icon={<Play size={13} />}
            label="Run again"
            disabled={!menu.entry.connection_id || !connections.some((c) => c.id === menu.entry.connection_id)}
            onClick={() => {
              setMenu(null);
              runAgain(menu.entry.sql, menu.entry.connection_id);
            }}
          />
          <MenuSeparator />
          <div className="px-2 pb-0.5 pt-1 text-[11px] text-muted">Run on another connection</div>
          {connections
            .filter((c) => c.id !== menu.entry.connection_id)
            .map((c) => (
              <MenuItem
                key={c.id}
                icon={<Plug size={12} />}
                label={c.name}
                onClick={() => {
                  setMenu(null);
                  runAgain(menu.entry.sql, c.id);
                }}
              />
            ))}
        </Popover>
      )}
      {clearing && (
        <ClearHistoryDialog
          onClose={() => setClearing(false)}
          onDone={(n) => {
            setClearing(false);
            toast(n === 0 ? "Nothing to delete" : `Deleted ${n.toLocaleString()} history entr${n === 1 ? "y" : "ies"} and their results`, "success");
            void load();
            void useStore.getState().refreshOutputs();
          }}
        />
      )}
      <div
        className="min-h-0 flex-1 overflow-auto px-1.5 pb-3"
        onScroll={(e) => {
          const el = e.currentTarget;
          if (el.scrollHeight - el.scrollTop - el.clientHeight < 200) void loadMore();
        }}
      >
        {items.length === 0 && !loading && (
          <div className="px-3 py-8 text-center text-[12.5px] text-muted">
            <History size={26} className="mx-auto mb-3 opacity-50" />
            {search || connFilter || statusFilter ? "No matches" : "Queries you run appear here."}
            {!showAi && (
              <div className="mt-1 text-[11px]">
                AI queries are hidden ·{" "}
                <button className="text-accent underline" onClick={toggleAi}>
                  show them
                </button>
              </div>
            )}
          </div>
        )}
        {items.map((h) => (
          <div
            key={h.id}
            role="button"
            tabIndex={0}
            title={h.error ?? h.sql}
            onClick={() => newTab({ sql: h.sql, connection_id: h.connection_id ?? undefined })}
            onKeyDown={(e) => e.key === "Enter" && newTab({ sql: h.sql, connection_id: h.connection_id ?? undefined })}
            onContextMenu={(e) => {
              e.preventDefault();
              setMenu({ x: e.clientX, y: e.clientY, entry: h });
            }}
            className="group mb-0.5 cursor-pointer rounded-md px-2 py-1.5 hover:bg-hover"
          >
            <div className="flex items-start gap-1.5">
              {h.status === "success" ? (
                <CheckCircle2 size={12} className="mt-0.5 shrink-0 text-success" />
              ) : h.status === "error" ? (
                <AlertCircle size={12} className="mt-0.5 shrink-0 text-danger" />
              ) : (
                <Ban size={12} className="mt-0.5 shrink-0 text-muted" />
              )}
              <span className="line-clamp-2 min-w-0 flex-1 break-all font-mono text-[11.5px]">{sqlPreview(h.sql, 140)}</span>
              {h.connection_id && connections.some((c) => c.id === h.connection_id) && (
                <button
                  type="button"
                  className="icon-btn -my-0.5 h-5 w-5 shrink-0 opacity-0 hover:text-accent focus:opacity-100 group-hover:opacity-100"
                  aria-label="Run again"
                  title="Run again in a new tab (right-click: run on another connection)"
                  onClick={(e) => {
                    e.stopPropagation();
                    runAgain(h.sql, h.connection_id);
                  }}
                  onKeyDown={(e) => e.stopPropagation()}
                >
                  <Play size={11} />
                </button>
              )}
              <button
                type="button"
                className="icon-btn -my-0.5 h-5 w-5 shrink-0 opacity-0 hover:text-danger focus:opacity-100 group-hover:opacity-100"
                aria-label="Delete this query from history"
                title="Delete from history (and its result)"
                onClick={(e) => {
                  e.stopPropagation();
                  void api
                    .deleteHistoryEntry(h.id)
                    .then(() => {
                      setItems((cur) => cur.filter((x) => x.id !== h.id));
                      void useStore.getState().refreshOutputs();
                    })
                    .catch((err) => toast(toError(err).message, "error"));
                }}
                onKeyDown={(e) => e.stopPropagation()}
              >
                <Trash2 size={11} />
              </button>
            </div>
            <div className="mt-1 flex items-center gap-2 pl-[18px] text-[10.5px] text-muted">
              <span>{relativeTime(h.started_at)}</span>
              <span>{formatDuration(h.duration_ms)}</span>
              {h.rows !== null && <span>{formatCount(h.rows)} rows</span>}
              {h.connection_name && (
                <span className="flex min-w-0 items-center gap-1 truncate">
                  <Plug size={10} />
                  {h.connection_name}
                </span>
              )}
              {h.origin === "job" && (
                <span className="flex shrink-0 items-center gap-0.5 rounded bg-hover px-1 text-fg" title="Run by a job step">
                  <Workflow size={9} /> Job
                </span>
              )}
              {(h.origin === "ai" || h.origin === "mcp") && (
                <span className="flex shrink-0 items-center gap-0.5 rounded bg-accent/10 px-1 text-accent" title={h.origin === "mcp" ? "Run by an MCP client" : "Run by the AI assistant"}>
                  <Sparkles size={9} /> {h.origin === "mcp" ? "MCP" : "AI"}
                </span>
              )}
              {h.output_handle && <HistoryOutput handle={h.output_handle} />}
            </div>
          </div>
        ))}
        {items.length > 0 && (
          <div className="py-2 text-center text-[11px] text-muted">
            {more ? (loading ? "Loading…" : <button className="text-accent hover:underline" onClick={() => void loadMore()}>Show more</button>) : `${items.length.toLocaleString()} entries`}
          </div>
        )}
      </div>
    </>
  );
}

const HISTORY_PAGE = 50;

/** Clear history: AI queries (default), entries older than N days, or all.
 *  The results those entries produced are dropped too. */
function ClearHistoryDialog({ onClose, onDone }: { onClose: () => void; onDone: (deleted: number) => void }) {
  const [kind, setKind] = useState<"ai" | "older_than_days" | "all">("ai");
  const [days, setDays] = useState("7");
  const [forget, setForget] = useState(false);
  const [busy, setBusy] = useState(false);
  const toast = useStore((s) => s.toast);
  const n = Number(days);
  const daysOk = Number.isInteger(n) && n >= 1 && n <= 3650;
  const go = async () => {
    setBusy(true);
    try {
      const what: HistoryClear = kind === "older_than_days" ? { kind, days: n } : { kind };
      onDone(await api.clearHistory(what, forget));
    } catch (e) {
      toast(toError(e).message, "error");
      setBusy(false);
    }
  };
  const opt = (k: typeof kind, label: string, hint: string, extra?: ReactNode) => (
    <label className={`flex cursor-pointer gap-2 rounded-md border px-2.5 py-2 ${kind === k ? "border-accent bg-accent/5" : "border-line"}`}>
      <input type="radio" name="clear-history" className="mt-0.5" checked={kind === k} onChange={() => setKind(k)} />
      <span className="min-w-0 flex-1">
        <span className="block text-[12.5px]">{label}</span>
        <span className="block text-[11px] text-muted">{hint}</span>
        {extra}
      </span>
    </label>
  );
  return (
    <Modal
      title="Clear query history"
      onClose={onClose}
      width={420}
      footer={
        <>
          <button className="btn-ghost" onClick={onClose}>
            Cancel
          </button>
          <button className={kind === "all" ? "btn-danger" : "btn-primary"} disabled={busy || (kind === "older_than_days" && !daysOk)} onClick={() => void go()}>
            {busy ? <Loader2 size={13} className="animate-spin" /> : <Trash2 size={13} />} Delete
          </button>
        </>
      }
    >
      <div className="space-y-2" role="radiogroup" aria-label="What to delete">
        {opt("ai", "Queries run by the AI assistant", "From the AI panel and MCP clients; yours and jobs' stay.")}
        {opt(
          "older_than_days",
          "Keep only recent queries",
          "Delete everything older than:",
          <span className="mt-1 flex items-center gap-1.5 text-[12px]">
            <input
              className={`field h-7 w-20 py-0 ${daysOk ? "" : "border-danger"}`}
              inputMode="numeric"
              aria-label="Days to keep"
              value={days}
              onFocus={() => setKind("older_than_days")}
              onChange={(e) => setDays(e.target.value)}
            />
            days
          </span>,
        )}
        {opt("all", "All queries", "Every history entry.")}
        <p className="pt-1 text-[11px] text-muted">The results these queries produced are deleted too, except pinned outputs and results still shown in a tab.</p>
        <label className="flex cursor-pointer items-start gap-2 pt-1 text-[12px]">
          <input type="checkbox" className="mt-0.5" checked={forget} onChange={(e) => setForget(e.target.checked)} />
          <span>
            Also forget what the AI learned from history
            <span className="block text-[11px] text-muted">Learned joins and table usage on every connection. Otherwise they stay, without the deleted queries as examples.</span>
          </span>
        </label>
      </div>
    </Modal>
  );
}
