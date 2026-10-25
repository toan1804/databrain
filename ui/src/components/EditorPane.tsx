import { useEffect, useRef, useState } from "react";
import { ChevronsDownUp, FileCode2, FileText, GitCompare, GitFork, LayoutDashboard, Network, Table2, Lightbulb, Loader2, MoreHorizontal, Play, PlayCircle, Plus, Save, Sparkles, Square, X, Workflow } from "lucide-react";
import { isQueryTab, useActiveTab, useStore, type Tab } from "../store";
import { editorBridge } from "../editorBridge";
import { SqlEditor } from "./SqlEditor";
import { ResultsPanel } from "./ResultsPanel";
import { ConnDot, EnvBadge, MenuItem, Popover } from "./ui";
import { NotebookView } from "./Notebook";
import { OutputTab } from "./OutputTab";
import { DdlTab } from "./DdlTab";
import { ParamBar } from "./ParamBar";
import { JobTab } from "./JobTab";
import { TabErrorBoundary } from "./TabErrorBoundary";
import { LineageTab, openLineage } from "./LineageTab";
import { PlanTab, openPlan } from "./PlanTab";
import { CompareTab } from "./CompareTab";
import { DashboardTab } from "./DashboardTab";
import { useAi } from "../aiStore";
import type { ConnectionView } from "../lib/types";

const ROW_LIMITS = [100, 500, 1000, 5000, 10000, 50000, 100000, 0];

/** Databases whose open transactions the app can tell (manual commit works there). */
const TX_KINDS = new Set(["postgres", "mysql", "mssql", "sqlite"]);

/**
 * Auto/manual commit toggle and, while the tab's session has a transaction
 * open, a badge with Commit and Rollback.
 */
function TxControls({ tab, conn, running, compact }: { tab: Tab; conn?: ConnectionView; running: boolean; compact: boolean }) {
  const txConn = useStore((s) => s.txOpen[tab.id]);
  const endTransaction = useStore((s) => s.endTransaction);
  const setAutoCommit = useStore((s) => s.setAutoCommit);
  const manual = tab.auto_commit === false;
  const supported = !!conn && TX_KINDS.has(conn.config.kind) && !conn.config.read_only;
  return (
    <>
      {(supported || manual) && (
        <button
          className={`btn-ghost shrink-0 border py-0.5 text-[11.5px] ${manual ? "border-warning/60 text-warning" : "border-line text-muted"}`}
          aria-pressed={manual}
          title={
            manual
              ? "Manual commit: changes stay in a transaction until you Commit or Roll back. Click for auto-commit."
              : "Auto-commit: each statement is saved at once. Click for manual commit (changes wait for Commit)."
          }
          onClick={() => setAutoCommit(tab.id, manual)}
        >
          {manual ? "Manual commit" : compact ? "Auto" : "Auto-commit"}
        </button>
      )}
      {txConn && (
        <span className="flex shrink-0 items-center gap-1 rounded-md bg-warning/15 py-0.5 pl-2 pr-0.5 text-[11.5px] text-warning" role="status">
          {!compact && "Transaction open"}
          <button className="btn-ghost h-5 px-1.5 py-0 text-[11.5px] text-fg" disabled={running} title="COMMIT the open transaction" onClick={() => void endTransaction(tab.id, true)}>
            Commit
          </button>
          <button className="btn-ghost h-5 px-1.5 py-0 text-[11.5px] text-fg" disabled={running} title="ROLLBACK: undo the uncommitted changes" onClick={() => void endTransaction(tab.id, false)}>
            Rollback
          </button>
        </span>
      )}
    </>
  );
}

export function EditorPane() {
  const tabs = useStore((s) => s.tabs);
  const activeId = useStore((s) => s.activeTabId);
  const connections = useStore((s) => s.connections);
  const [split, setSplit] = useState(() => Number(localStorage.getItem("db.split")) || 0.45);
  const [collapsed, setCollapsed] = useState(false);
  const container = useRef<HTMLDivElement>(null);
  const active = tabs.find((t) => t.id === activeId);

  const startDrag = (e: React.MouseEvent) => {
    e.preventDefault();
    const rect = container.current!.getBoundingClientRect();
    const move = (ev: MouseEvent) => {
      const r = Math.min(0.85, Math.max(0.12, (ev.clientY - rect.top) / rect.height));
      setSplit(r);
    };
    const up = () => {
      window.removeEventListener("mousemove", move);
      window.removeEventListener("mouseup", up);
      document.body.style.cursor = "";
      setSplit((r) => {
        localStorage.setItem("db.split", String(r));
        return r;
      });
    };
    document.body.style.cursor = "row-resize";
    window.addEventListener("mousemove", move);
    window.addEventListener("mouseup", up);
  };

  return (
    <div className="flex h-full min-w-0 flex-col bg-panel">
      <TabBar />
      {tabs
        .filter((t) => t.notebook_id)
        .map((t) => (
          <div key={t.id} className={t.id === activeId ? "min-h-0 flex-1" : "hidden"}>
            <TabErrorBoundary label="notebook">
              <NotebookView tabId={t.id} notebookId={t.notebook_id!} visible={t.id === activeId} />
            </TabErrorBoundary>
          </div>
        ))}
      {tabs
        .filter((t) => t.output_ref)
        .map((t) => (
          <div key={t.id} className={t.id === activeId ? "min-h-0 flex-1" : "hidden"}>
            <TabErrorBoundary label="output">
              <OutputTab tabId={t.id} reference={t.output_ref!} visible={t.id === activeId} />
            </TabErrorBoundary>
          </div>
        ))}
      {tabs
        .filter((t) => t.ddl)
        .map((t) => (
          <div key={t.id} className={t.id === activeId ? "min-h-0 flex-1" : "hidden"}>
            <TabErrorBoundary label="DDL view">
              <DdlTab tabId={t.id} reference={t.ddl!} visible={t.id === activeId} />
            </TabErrorBoundary>
          </div>
        ))}
      {tabs
        .filter((t) => t.job_id)
        .map((t) => (
          <div key={t.id} className={t.id === activeId ? "min-h-0 flex-1" : "hidden"}>
            <TabErrorBoundary label="job">
              <JobTab tabId={t.id} jobId={t.job_id!} visible={t.id === activeId} />
            </TabErrorBoundary>
          </div>
        ))}
      {tabs
        .filter((t) => t.lineage)
        .map((t) => (
          <div key={t.id} className={t.id === activeId ? "min-h-0 flex-1" : "hidden"}>
            <TabErrorBoundary label="lineage view">
              <LineageTab tabId={t.id} visible={t.id === activeId} />
            </TabErrorBoundary>
          </div>
        ))}
      {tabs
        .filter((t) => t.dashboard_id)
        .map((t) => (
          <div key={t.id} className={t.id === activeId ? "min-h-0 flex-1" : "hidden"}>
            <TabErrorBoundary label="dashboard">
              <DashboardTab tabId={t.id} visible={t.id === activeId} />
            </TabErrorBoundary>
          </div>
        ))}
      {tabs
        .filter((t) => t.compare)
        .map((t) => (
          <div key={t.id} className={t.id === activeId ? "min-h-0 flex-1" : "hidden"}>
            <TabErrorBoundary label="schema compare">
              <CompareTab tabId={t.id} visible={t.id === activeId} />
            </TabErrorBoundary>
          </div>
        ))}
      {tabs
        .filter((t) => t.plan)
        .map((t) => (
          <div key={t.id} className={t.id === activeId ? "min-h-0 flex-1" : "hidden"}>
            <TabErrorBoundary label="query plan">
              <PlanTab tabId={t.id} visible={t.id === activeId} />
            </TabErrorBoundary>
          </div>
        ))}
      <div className={active && !isQueryTab(active) ? "hidden" : "contents"}>
      <Toolbar />
      {isQueryTab(active) && <ParamBar tabId={active.id} kind={connections.find((c) => c.id === active.connection_id)?.config.kind} />}
      <div ref={container} className="flex min-h-0 flex-1 flex-col">
        <div className="min-h-0" style={{ flex: collapsed ? "1 1 auto" : `0 0 ${split * 100}%` }}>
          {tabs
            .filter(isQueryTab)
            .map((t) => (
              <SqlEditor key={t.id} tabId={t.id} visible={t.id === activeId} />
            ))}
        </div>
        <div
          role="separator"
          aria-orientation="horizontal"
          aria-label="Resize results"
          onMouseDown={startDrag}
          onDoubleClick={() => setCollapsed(!collapsed)}
          className="group relative h-[5px] shrink-0 cursor-row-resize border-t border-line bg-panel-2 hover:bg-accent/30"
        >
          <button
            className="absolute right-2 top-1/2 hidden -translate-y-1/2 rounded bg-panel px-1 text-muted group-hover:block"
            aria-label={collapsed ? "Show results" : "Hide results"}
            onMouseDown={(e) => e.stopPropagation()}
            onClick={() => setCollapsed(!collapsed)}
          >
            <ChevronsDownUp size={11} />
          </button>
        </div>
        {!collapsed && <div className="min-h-0 flex-1 bg-panel">{activeId && isQueryTab(active) && <ResultsPanel tabId={activeId} />}</div>}
      </div>
      </div>
    </div>
  );
}

function TabBar() {
  const tabs = useStore((s) => s.tabs);
  const activeId = useStore((s) => s.activeTabId);
  const runs = useStore((s) => s.runs);
  const connections = useStore((s) => s.connections);
  const setActive = useStore((s) => s.setActiveTab);
  const closeTab = useStore((s) => s.closeTab);
  const newTab = useStore((s) => s.newTab);
  const moveTab = useStore((s) => s.moveTab);
  const updateTab = useStore((s) => s.updateTab);
  const [editing, setEditing] = useState<string | null>(null);
  const drag = useRef<number | null>(null);
  const bar = useRef<HTMLDivElement>(null);

  // Keep the active tab visible: when it changes, and when the bar gets
  // narrower (AI panel opened or resized, sidebar resized, window resized).
  useEffect(() => {
    const el = bar.current;
    if (!el) return;
    const reveal = () => {
      const tab = el.querySelector<HTMLElement>('[role="tab"][aria-selected="true"]');
      if (!tab) return;
      const b = el.getBoundingClientRect();
      const r = tab.getBoundingClientRect();
      if (r.left < b.left) el.scrollLeft -= b.left - r.left + 8;
      else if (r.right > b.right) el.scrollLeft += r.right - b.right + 8;
    };
    reveal();
    const ro = new ResizeObserver(reveal);
    ro.observe(el);
    return () => ro.disconnect();
  }, [activeId, tabs.length]);

  // A mouse wheel scrolls the tabs sideways (trackpads already do).
  useEffect(() => {
    const el = bar.current;
    if (!el) return;
    const onWheel = (e: WheelEvent) => {
      if (Math.abs(e.deltaY) <= Math.abs(e.deltaX) || el.scrollWidth <= el.clientWidth) return;
      e.preventDefault();
      el.scrollLeft += e.deltaY;
    };
    el.addEventListener("wheel", onWheel, { passive: false });
    return () => el.removeEventListener("wheel", onWheel);
  }, []);

  const close = (t: Tab) => {
    if (t.dirty) {
      useStore.getState().askConfirm({
        title: `Close "${t.title}"?`,
        reasons: [t.file_path ? `This tab has changes not saved to ${t.file_path}.` : "This tab has unsaved changes to its saved query."],
        confirmLabel: "Close without saving",
        onConfirm: () => closeTab(t.id),
      });
    } else closeTab(t.id);
  };

  return (
    <div
      ref={bar}
      className="flex h-10 shrink-0 items-end gap-0.5 overflow-x-auto border-b border-line bg-bg px-1.5"
      role="tablist"
      data-tauri-drag-region
    >
      {tabs.map((t, i) => {
        const conn = connections.find((c) => c.id === t.connection_id);
        const running = runs[t.id]?.running;
        const active = t.id === activeId;
        return (
          <div
            key={t.id}
            role="tab"
            aria-selected={active}
            tabIndex={0}
            draggable={editing !== t.id}
            onDragStart={() => (drag.current = i)}
            onDragOver={(e) => e.preventDefault()}
            onDrop={() => {
              if (drag.current !== null && drag.current !== i) moveTab(drag.current, i);
              drag.current = null;
            }}
            onClick={() => setActive(t.id)}
            onDoubleClick={() => setEditing(t.id)}
            onAuxClick={(e) => e.button === 1 && close(t)}
            onKeyDown={(e) => e.key === "Enter" && setActive(t.id)}
            className={`group flex h-8 min-w-[84px] max-w-[220px] shrink cursor-pointer items-center gap-1.5 rounded-t-lg border border-b-0 px-2.5 text-[12.5px] ${
              active ? "border-line bg-panel text-fg" : "border-transparent text-muted hover:bg-panel/60 hover:text-fg"
            }`}
          >
            {running ? (
              <Loader2 size={11} className="shrink-0 animate-spin text-accent" />
            ) : t.notebook_id ? (
              <FileText size={12} className="shrink-0 text-muted" />
            ) : t.output_ref ? (
              <Table2 size={12} className="shrink-0 text-muted" />
            ) : t.ddl ? (
              <FileCode2 size={12} className="shrink-0 text-muted" />
            ) : t.lineage ? (
              <Network size={12} className="shrink-0 text-muted" />
            ) : t.plan ? (
              <GitFork size={12} className="shrink-0 rotate-90 text-muted" />
            ) : t.compare ? (
              <GitCompare size={12} className="shrink-0 text-muted" />
            ) : t.dashboard_id ? (
              <LayoutDashboard size={12} className="shrink-0 text-muted" />
            ) : t.job_id ? (
              <Workflow size={12} className="shrink-0 text-muted" />
            ) : (
              <ConnDot color={conn?.color} />
            )}
            {editing === t.id ? (
              <input
                autoFocus
                className="w-full min-w-0 bg-transparent outline-none"
                defaultValue={t.title}
                aria-label="Tab name"
                onBlur={(e) => {
                  updateTab(t.id, { title: e.target.value.trim() || t.title });
                  setEditing(null);
                }}
                onKeyDown={(e) => {
                  if (e.key === "Enter") (e.target as HTMLInputElement).blur();
                  if (e.key === "Escape") setEditing(null);
                }}
              />
            ) : (
              <span className="min-w-0 flex-1 truncate">{t.title}</span>
            )}
            {t.dirty && <span className="h-1.5 w-1.5 shrink-0 rounded-full bg-fg/60 group-hover:hidden" />}
            <button
              aria-label={`Close ${t.title}`}
              onClick={(e) => {
                e.stopPropagation();
                close(t);
              }}
              className={`shrink-0 rounded p-0.5 text-muted hover:bg-hover hover:text-fg ${
                active ? "" : "opacity-0 group-hover:opacity-100"
              } ${t.dirty ? "hidden group-hover:block" : ""}`}
            >
              <X size={12} />
            </button>
          </div>
        );
      })}
      <button className="icon-btn mb-0.5 ml-0.5" title="New tab (⌘T)" aria-label="New tab" onClick={() => newTab()}>
        <Plus size={15} />
      </button>
      <div className="flex-1 self-stretch" data-tauri-drag-region />
    </div>
  );
}

function Toolbar() {
  const tab = useActiveTab();
  const connections = useStore((s) => s.connections);
  const run = useStore((s) => (tab ? s.runs[tab.id] : undefined));
  const rowLimit = useStore((s) => s.rowLimit);
  const setRowLimit = useStore((s) => s.setRowLimit);
  const updateTab = useStore((s) => s.updateTab);
  const cancelTab = useStore((s) => s.cancelTab);
  const saveTabQuery = useStore((s) => s.saveTabQuery);
  const openConnectionDialog = useStore((s) => s.openConnectionDialog);
  const conn = connections.find((c) => c.id === tab?.connection_id);

  useEffect(() => {
    // Keep the tab pointing at a real connection.
    if (tab && tab.connection_id && connections.length > 0 && !conn) {
      updateTab(tab.id, { connection_id: null });
    }
  }, [tab, conn, connections.length, updateTab]);

  const send = useAi((s) => s.send);
  const setAiOpen = useAi((s) => s.setOpen);

  // Fit narrow editors (AI panel open, wide sidebar): first the right-hand
  // buttons lose their labels, then they move into a "More" menu.
  const bar = useRef<HTMLDivElement>(null);
  const [width, setWidth] = useState(Infinity);
  const hasTab = !!tab;
  useEffect(() => {
    const el = bar.current;
    if (!el) return;
    const ro = new ResizeObserver(([e]) => setWidth(e.contentRect.width));
    ro.observe(el);
    return () => ro.disconnect();
  }, [hasTab]);
  const [more, setMore] = useState<{ x: number; y: number } | null>(null);

  if (!tab) return null;

  const iconOnly = width < 860;
  const overflow = width < 640;

  const trigger = (mode: "statement" | "all") => {
    const v = editorBridge.get(tab.id);
    if (!v) return;
    const sel = v.state.selection.main;
    void useStore.getState().runTab(tab.id, mode, {
      doc: v.state.doc.toString(),
      selFrom: sel.from,
      selTo: sel.to,
      cursor: sel.head,
    });
  };

  const explain = () => void send({ message: "Explain what the query in my editor does.", mode: "explain", targetKey: tab.id });
  const editWithAi = () => {
    if (!editorBridge.get(tab.id)) return setAiOpen(true);
    window.dispatchEvent(new CustomEvent("db:inline-ai", { detail: { key: tab.id } }));
  };
  const saveLabel = tab.saved_query_id || tab.file_path ? "Save" : "Save as…";
  const save = () => (tab.file_path ? void useStore.getState().saveTabFile(tab.id) : void saveTabQuery(tab.id));
  const saveTitle = tab.file_path ? `Save to ${tab.file_path} (⌘S)` : `${saveLabel} (⌘S)`;

  return (
    <div
      ref={bar}
      className={`flex h-10 shrink-0 items-center gap-2 border-b px-2 ${
        conn?.env === "prod" ? "border-danger/50 bg-danger/5" : "border-line"
      }`}
    >
      <div className="flex min-w-0 shrink items-center gap-1.5 rounded-md border border-line bg-panel-2 pl-2">
        <ConnDot color={conn?.color} connected={conn?.connected} />
        <select
          className={`h-7 min-w-0 bg-transparent pr-1 text-[12.5px] outline-none ${overflow ? "max-w-[140px]" : "max-w-[220px]"}`}
          aria-label="Connection"
          value={tab.connection_id ?? ""}
          onChange={(e) => {
            if (e.target.value === "__new__") openConnectionDialog(null);
            else updateTab(tab.id, { connection_id: e.target.value || null });
          }}
        >
          <option value="">No connection</option>
          {connections.map((c) => (
            <option key={c.id} value={c.id}>
              {c.name}
            </option>
          ))}
          <option value="__new__">+ New connection…</option>
        </select>
      </div>
      {conn && !overflow && <EnvBadge env={conn.env} />}

      <div className="mx-1 h-5 w-px shrink-0 bg-line" />

      {run?.running ? (
        <button className="btn-danger shrink-0 py-1" onClick={() => cancelTab(tab.id)} title="Cancel (⌘.)">
          <Square size={12} fill="currentColor" /> Stop
        </button>
      ) : (
        <>
          <button className="btn-primary shrink-0 py-1" onClick={() => trigger("statement")} disabled={!conn} title="Run statement or selection (⌘↵)">
            <Play size={13} fill="currentColor" /> Run
          </button>
          <button
            className="btn-ghost shrink-0 py-1"
            onClick={() => trigger("all")}
            disabled={!conn}
            title="Run all statements (⇧⌘↵)"
            aria-label="Run all"
          >
            <PlayCircle size={14} /> {!overflow && "Run all"}
          </button>
        </>
      )}

      <label className="ml-1 flex shrink-0 items-center gap-1.5 text-[12px] text-muted">
        {!iconOnly && "Limit"}
        <select
          className="h-7 rounded-md border border-line bg-panel-2 px-1.5 text-[12px] text-fg outline-none"
          value={rowLimit}
          aria-label="Row limit"
          title="Row limit"
          onChange={(e) => setRowLimit(Number(e.target.value))}
        >
          {ROW_LIMITS.map((n) => (
            <option key={n} value={n}>
              {n === 0 ? "No limit" : n.toLocaleString()}
            </option>
          ))}
        </select>
      </label>

      <TxControls tab={tab} conn={conn} running={!!run?.running} compact={iconOnly} />

      <div className="ml-auto flex shrink-0 items-center gap-1">
        {conn?.config.read_only && !overflow && (
          <span className="rounded bg-panel-2 px-1.5 py-0.5 text-[11px] text-muted">{iconOnly ? "RO" : "read-only"}</span>
        )}
        {overflow ? (
          <button
            className="btn-ghost py-1"
            title="More: Lineage, Plan, Explain, AI, Save"
            aria-label="More actions"
            aria-haspopup="menu"
            aria-expanded={!!more}
            onClick={(e) => {
              const r = e.currentTarget.getBoundingClientRect();
              setMore({ x: r.right - 200, y: r.bottom + 4 });
            }}
          >
            <MoreHorizontal size={14} />
          </button>
        ) : (
          <>
            <button
              className="btn-ghost py-1"
              title="Column lineage of the statement or selection, in a new tab"
              aria-label="Lineage"
              onClick={() => openLineage(tab.id)}
            >
              <Network size={13} /> {!iconOnly && "Lineage"}
            </button>
            <button
              className="btn-ghost py-1"
              disabled={!conn}
              title="Query plan of the statement or selection (EXPLAIN), in a new tab"
              aria-label="Plan"
              onClick={() => void openPlan(tab.id)}
            >
              <GitFork size={13} className="rotate-90" /> {!iconOnly && "Plan"}
            </button>
            <button className="btn-ghost py-1" disabled={!conn} title="Explain this query with AI" aria-label="Explain" onClick={explain}>
              <Lightbulb size={13} /> {!iconOnly && "Explain"}
            </button>
            <button className="btn-ghost py-1" title="Edit with AI (⌘I) · open assistant (⌘L)" aria-label="AI" onClick={editWithAi}>
              <Sparkles size={13} /> {!iconOnly && "AI"}
            </button>
            <button className="btn-ghost py-1" onClick={save} title={saveTitle} aria-label={saveLabel}>
              <Save size={13} /> {!iconOnly && saveLabel}
            </button>
          </>
        )}
      </div>
      {more && (
        <Popover x={more.x} y={more.y} onClose={() => setMore(null)} className="w-[200px]">
          {conn?.env && conn.env !== "none" && (
            <div className="px-2 pb-1 pt-0.5">
              <EnvBadge env={conn.env} />
            </div>
          )}
          {conn?.config.read_only && <div className="px-2 pb-1 text-[11px] text-muted">Read-only connection</div>}
          <MenuItem icon={<Network size={13} />} label="Lineage" onClick={() => (setMore(null), openLineage(tab.id))} />
          <MenuItem icon={<GitFork size={13} className="rotate-90" />} label="Query plan" disabled={!conn} onClick={() => (setMore(null), void openPlan(tab.id))} />
          <MenuItem icon={<Lightbulb size={13} />} label="Explain" disabled={!conn} onClick={() => (setMore(null), explain())} />
          <MenuItem icon={<Sparkles size={13} />} label="Edit with AI" hint="⌘I" onClick={() => (setMore(null), editWithAi())} />
          <MenuItem icon={<Save size={13} />} label={saveLabel} hint="⌘S" onClick={() => (setMore(null), save())} />
          <MenuItem icon={<FileCode2 size={13} />} label="Save to file…" hint="⇧⌘S" onClick={() => (setMore(null), void useStore.getState().saveTabFile(tab.id, true))} />
          {tab.file_path && tab.saved_query_id == null && (
            <MenuItem icon={<Save size={13} />} label="Save as saved query…" onClick={() => (setMore(null), void saveTabQuery(tab.id))} />
          )}
        </Popover>
      )}
    </div>
  );
}
