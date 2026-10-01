import { useEffect, useRef, useState } from "react";
import { ChevronsDownUp, FileText, Table2, Lightbulb, Loader2, Play, PlayCircle, Plus, Save, Sparkles, Square, X } from "lucide-react";
import { useActiveTab, useStore, type Tab } from "../store";
import { editorBridge } from "../editorBridge";
import { SqlEditor } from "./SqlEditor";
import { ResultsPanel } from "./ResultsPanel";
import { ConnDot, EnvBadge } from "./ui";
import { NotebookView } from "./Notebook";
import { OutputTab } from "./OutputTab";
import { useAi } from "../aiStore";

const ROW_LIMITS = [100, 500, 1000, 5000, 10000, 50000, 100000, 0];

export function EditorPane() {
  const tabs = useStore((s) => s.tabs);
  const activeId = useStore((s) => s.activeTabId);
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
            <NotebookView tabId={t.id} notebookId={t.notebook_id!} visible={t.id === activeId} />
          </div>
        ))}
      {tabs
        .filter((t) => t.output_ref)
        .map((t) => (
          <div key={t.id} className={t.id === activeId ? "min-h-0 flex-1" : "hidden"}>
            <OutputTab tabId={t.id} reference={t.output_ref!} visible={t.id === activeId} />
          </div>
        ))}
      <div className={active?.notebook_id || active?.output_ref ? "hidden" : "contents"}>
      <Toolbar />
      <div ref={container} className="flex min-h-0 flex-1 flex-col">
        <div className="min-h-0" style={{ flex: collapsed ? "1 1 auto" : `0 0 ${split * 100}%` }}>
          {tabs
            .filter((t) => !t.notebook_id && !t.output_ref)
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
        {!collapsed && <div className="min-h-0 flex-1 bg-panel">{activeId && !active?.notebook_id && !active?.output_ref && <ResultsPanel tabId={activeId} />}</div>}
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

  const close = (t: Tab) => {
    if (t.dirty) {
      useStore.getState().askConfirm({
        title: `Close "${t.title}"?`,
        reasons: ["This tab has unsaved changes to its saved query."],
        confirmLabel: "Close without saving",
        onConfirm: () => closeTab(t.id),
      });
    } else closeTab(t.id);
  };

  return (
    <div className="flex h-10 shrink-0 items-end gap-0.5 overflow-x-auto border-b border-line bg-bg px-1.5" role="tablist" data-tauri-drag-region>
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
            className={`group flex h-8 min-w-[110px] max-w-[220px] cursor-pointer items-center gap-1.5 rounded-t-lg border border-b-0 px-2.5 text-[12.5px] ${
              active ? "border-line bg-panel text-fg" : "border-transparent text-muted hover:bg-panel/60 hover:text-fg"
            }`}
          >
            {running ? (
              <Loader2 size={11} className="shrink-0 animate-spin text-accent" />
            ) : t.notebook_id ? (
              <FileText size={12} className="shrink-0 text-muted" />
            ) : t.output_ref ? (
              <Table2 size={12} className="shrink-0 text-muted" />
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

  if (!tab) return null;

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

  return (
    <div
      className={`flex h-10 shrink-0 items-center gap-2 border-b px-2 ${
        conn?.env === "prod" ? "border-danger/50 bg-danger/5" : "border-line"
      }`}
    >
      <div className="flex items-center gap-1.5 rounded-md border border-line bg-panel-2 pl-2">
        <ConnDot color={conn?.color} connected={conn?.connected} />
        <select
          className="h-7 max-w-[220px] bg-transparent pr-1 text-[12.5px] outline-none"
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
      {conn && <EnvBadge env={conn.env} />}

      <div className="mx-1 h-5 w-px bg-line" />

      {run?.running ? (
        <button className="btn-danger py-1" onClick={() => cancelTab(tab.id)} title="Cancel (⌘.)">
          <Square size={12} fill="currentColor" /> Stop
        </button>
      ) : (
        <>
          <button className="btn-primary py-1" onClick={() => trigger("statement")} disabled={!conn} title="Run statement or selection (⌘↵)">
            <Play size={13} fill="currentColor" /> Run
          </button>
          <button className="btn-ghost py-1" onClick={() => trigger("all")} disabled={!conn} title="Run all statements (⇧⌘↵)">
            <PlayCircle size={14} /> Run all
          </button>
        </>
      )}

      <label className="ml-1 flex items-center gap-1.5 text-[12px] text-muted">
        Limit
        <select
          className="h-7 rounded-md border border-line bg-panel-2 px-1.5 text-[12px] text-fg outline-none"
          value={rowLimit}
          aria-label="Row limit"
          onChange={(e) => setRowLimit(Number(e.target.value))}
        >
          {ROW_LIMITS.map((n) => (
            <option key={n} value={n}>
              {n === 0 ? "No limit" : n.toLocaleString()}
            </option>
          ))}
        </select>
      </label>

      <div className="ml-auto flex items-center gap-1">
        {conn?.config.read_only && <span className="rounded bg-panel-2 px-1.5 py-0.5 text-[11px] text-muted">read-only</span>}
        <button
          className="btn-ghost py-1"
          disabled={!conn}
          title="Explain this query with AI"
          onClick={() => void send({ message: "Explain what the query in my editor does.", mode: "explain", targetKey: tab.id })}
        >
          <Lightbulb size={13} /> Explain
        </button>
        <button
          className="btn-ghost py-1"
          title="Edit with AI (⌘I) · open assistant (⌘L)"
          onClick={() => {
            if (!editorBridge.get(tab.id)) return setAiOpen(true);
            window.dispatchEvent(new CustomEvent("db:inline-ai", { detail: { key: tab.id } }));
          }}
        >
          <Sparkles size={13} /> AI
        </button>
        <button className="btn-ghost py-1" onClick={() => saveTabQuery(tab.id)} title="Save query (⌘S)">
          <Save size={13} /> {tab.saved_query_id ? "Save" : "Save as…"}
        </button>
      </div>
    </div>
  );
}
