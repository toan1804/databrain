import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import { save as saveDialog } from "@tauri-apps/plugin-dialog";
import { AlertCircle, AlertTriangle, ArrowDown, ArrowRight, CheckCircle2, Columns3, ExternalLink, Rows3, ChevronDown, ClipboardCopy, Download, Loader2, Maximize2, Network, RefreshCw, TextCursorInput, WifiOff, X } from "lucide-react";
import { api, isTauri, toError } from "../lib/api";
import type { LineageCheck, LineageView, ParamValue } from "../lib/types";
import { layout, normalize, trace, withPositions, type Direction, type Positions } from "../lib/lineageLayout";
import { columnFlow, conditionText, explainColumn, type FlowNode } from "../lib/lineageExplain";
import { EXPORTS, exportText, kindLabel, toMermaid, toSvg, type DiagramColors, type ExportFormat } from "../lib/lineageExport";
import { useStore, type Tab } from "../store";
import { editorBridge } from "../editorBridge";
import { copyText } from "./CatalogMenus";
import { MenuItem, MenuSeparator, Popover } from "./ui";
import { SplitHandle } from "./SplitHandle";

/**
 * Open (or refresh) the lineage tab of an editor: its selection, else the
 * whole script. `editorKey` is a query tab id or a notebook cell key.
 */
export function openLineage(editorKey: string, connectionId?: string | null) {
  const st = useStore.getState();
  const view = editorBridge.get(editorKey);
  const srcTab = st.tabs.find((t) => t.id === editorKey);
  const nbTab = editorKey.startsWith("nb:") ? st.tabs.find((t) => t.notebook_id && editorKey.startsWith(`nb:${t.notebook_id}:`)) : undefined;
  const doc = view?.state.doc.toString() ?? srcTab?.sql ?? "";
  const sel = view?.state.selection.main;
  const whole = !sel || sel.empty;
  const sql = whole ? doc : doc.slice(sel.from, sel.to);
  if (!sql.trim()) return st.toast("Nothing to trace: write a query first", "info");
  const conn = connectionId !== undefined ? connectionId : (srcTab?.connection_id ?? nbTab?.connection_id ?? null);
  const lineage = { source: editorKey, base: whole ? 0 : sel.from, whole };
  const existing = st.tabs.find((t) => t.lineage?.source === editorKey && !t.lineage.column);
  if (existing) {
    st.updateTab(existing.id, { sql, lineage, connection_id: conn });
    return st.setActiveTab(existing.id);
  }
  const title = `Lineage · ${(srcTab ?? nbTab)?.title ?? "SQL"}`;
  st.newTab({ title, sql, connection_id: conn, lineage });
}

/** Open one column's flow in a tab of its own (from a lineage tab). */
export function openColumnLineage(from: Tab, node: string, column?: string) {
  const st = useStore.getState();
  if (!from.lineage) return;
  const lineage = { ...from.lineage, column: { node, column } };
  const same = st.tabs.find((t) => t.lineage?.column && t.lineage.source === from.lineage!.source && t.lineage.column.node === node && t.lineage.column.column === column);
  if (same) {
    st.updateTab(same.id, { sql: from.sql });
    return st.setActiveTab(same.id);
  }
  st.newTab({ title: `Lineage · ${column ?? node}`, sql: from.sql, connection_id: from.connection_id ?? null, lineage });
}

const PANEL_KEY = "db.lineage.panel";
const PANEL_DEFAULT = 416;

function cssVar(name: string): string {
  return getComputedStyle(document.documentElement).getPropertyValue(name).trim();
}

function themeColors(): DiagramColors {
  return {
    bg: cssVar("--bg") || "#0b0f17",
    panel: cssVar("--panel") || "#111827",
    header: cssVar("--panel-2") || "#1f2937",
    border: cssVar("--border") || "#334155",
    text: cssVar("--text") || "#e5e7eb",
    muted: cssVar("--muted") || "#94a3b8",
    accent: cssVar("--accent") || "#60a5fa",
    warning: cssVar("--warning") || "#fbbf24",
    font: cssVar("--font-mono") || "ui-monospace, Menlo, monospace",
  };
}

/** Rasterize an SVG string (2× for sharp images); base64 PNG without the data: prefix. */
async function svgToPng(svg: string, width: number, height: number): Promise<string> {
  const url = URL.createObjectURL(new Blob([svg], { type: "image/svg+xml" }));
  try {
    const img = new Image();
    await new Promise<void>((ok, fail) => {
      img.onload = () => ok();
      img.onerror = () => fail(new Error("Could not render the diagram"));
      img.src = url;
    });
    const scale = Math.min(2, 16000 / Math.max(width, height));
    const canvas = document.createElement("canvas");
    canvas.width = Math.ceil(width * scale);
    canvas.height = Math.ceil(height * scale);
    const ctx = canvas.getContext("2d")!;
    ctx.scale(scale, scale);
    ctx.drawImage(img, 0, 0);
    return canvas.toDataURL("image/png").split(",")[1];
  } finally {
    URL.revokeObjectURL(url);
  }
}

type Load = { status: "loading" } | { status: "ready"; view: LineageView } | { status: "error"; message: string };

function checkSummary(checks: LineageCheck[], connName: string | undefined): { tone: "ok" | "error" | "muted"; text: string; first?: LineageCheck } {
  const failed = checks.filter((c) => c.status === "error");
  if (failed.length) return { tone: "error", text: `${failed.length} statement${failed.length === 1 ? "" : "s"} fail in the database`, first: failed[0] };
  if (checks.some((c) => c.status === "offline")) return { tone: "muted", text: "Not checked: can't reach the server" };
  if (checks.some((c) => c.status === "timeout")) return { tone: "muted", text: "Not checked: the database took too long" };
  if (checks.some((c) => c.status === "ok")) return { tone: "ok", text: `Checked by ${connName ?? "the database"} (EXPLAIN, not run)` };
  if (checks.some((c) => c.status === "unsupported")) return { tone: "muted", text: "This engine has no EXPLAIN check: lineage from the SQL only" };
  return { tone: "muted", text: "Not checked by the database" };
}

/** Lineage diagram of a script (read-only tab): pan/zoom, click a column to trace it, export. */
export function LineageTab({ tabId, visible }: { tabId: string; visible: boolean }) {
  const tab = useStore((s) => s.tabs.find((t) => t.id === tabId)) as Tab | undefined;
  const conn = useStore((s) => s.connections.find((c) => c.id === tab?.connection_id));
  const theme = useStore((s) => s.theme);
  const [load, setLoad] = useState<Load>({ status: "loading" });
  const [check, setCheck] = useState(true);
  const [focus, setFocus] = useState<{ node: string; column?: string } | null>(null);
  const [view, setView] = useState({ x: 0, y: 0, k: 1 });
  const [menu, setMenu] = useState<{ x: number; y: number } | null>(null);
  const [showWarnings, setShowWarnings] = useState(false);
  const [dir, setDirState] = useState<Direction>(() => (localStorage.getItem("db.lineage.dir") === "tb" ? "tb" : "lr"));
  /** Nodes the user dragged (view only: the SQL is not changed). */
  const [pos, setPos] = useState<Positions>({});
  /** Tables whose columns are listed; starts with none (table-level lineage). */
  const [expanded, setExpanded] = useState<Set<string> | "all">(new Set<string>());
  const toggleNode = (id: string) =>
    setExpanded((e) => {
      const all = graph?.nodes.map((n) => n.id) ?? [];
      const next = new Set(e === "all" ? all : e);
      if (next.has(id)) next.delete(id);
      else next.add(id);
      return next.size === all.length && all.length > 0 ? "all" : next;
    });
  const setDir = (d: Direction) => {
    localStorage.setItem("db.lineage.dir", d);
    setDirState(d);
    setPos({});
  };
  const box = useRef<HTMLDivElement>(null);
  const area = useRef<HTMLDivElement>(null);
  const [panelW, setPanelW] = useState(() => Number(localStorage.getItem(PANEL_KEY)) || PANEL_DEFAULT);
  const seq = useRef(0);
  const fitted = useRef(false);
  const ref = tab?.lineage;
  /** This tab shows one column's flow (no diagram). */
  const columnTab = !!ref?.column;

  const paramsOf = useCallback((): Record<string, ParamValue> => {
    if (!ref) return {};
    const st = useStore.getState();
    const owner = ref.source.startsWith("nb:") ? st.tabs.find((t) => t.notebook_id && ref.source.startsWith(`nb:${t.notebook_id}:`))?.id : ref.source;
    return (owner && st.params[owner]) || {};
  }, [ref]);

  const run = useCallback(async () => {
    if (!tab) return;
    const my = ++seq.current;
    setLoad({ status: "loading" });
    if (!isTauri()) return setLoad({ status: "error", message: "Lineage needs the DataBrain desktop app" });
    try {
      const v = await api.sqlLineage(conn?.id ?? null, conn?.config.kind ?? "postgres", tab.sql, paramsOf(), check && !!conn);
      if (my === seq.current) {
        setLoad({ status: "ready", view: v });
        // A column tab opens on its column.
        const want = tab.lineage?.column;
        const n = want ? v.graph.nodes.find((x) => x.name === want.node) : undefined;
        setFocus(n ? { node: n.id, column: want!.column } : null);
        setPos({});
        setExpanded(new Set());
      }
    } catch (e) {
      if (my === seq.current) setLoad({ status: "error", message: toError(e).message });
    }
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [tab?.sql, conn?.id, conn?.config.kind, check, paramsOf]);

  useEffect(() => {
    void run();
  }, [run]);

  const graph = load.status === "ready" ? load.view.graph : null;
  const traced = useMemo(() => (graph && focus ? trace(graph, focus.node, focus.column) : null), [graph, focus]);
  // Collapsed tables still list the traced column's path.
  const base = useMemo(() => (graph ? layout(graph, dir, { expanded, show: traced?.cols }) : null), [graph, dir, expanded, traced]);
  const lay = useMemo(() => (base ? withPositions(base, pos) : null), [base, pos]);
  // eslint-disable-next-line react-hooks/exhaustive-deps
  const colors = useMemo(themeColors, [theme]);
  const inner = useMemo(() => (graph && lay ? toSvg(graph, lay, { colors, focus: traced, inner: true }) : ""), [graph, lay, colors, traced]);

  const fit = useCallback(() => {
    const el = box.current;
    if (!el || !lay) return;
    const r = el.getBoundingClientRect();
    if (!r.width || !r.height) return;
    const n = normalize(lay);
    const shiftX = lay.nodes.length ? Math.min(...lay.nodes.map((x) => x.x)) - Math.min(...n.nodes.map((x) => x.x)) : 0;
    const shiftY = lay.nodes.length ? Math.min(...lay.nodes.map((x) => x.y)) - Math.min(...n.nodes.map((x) => x.y)) : 0;
    const k = Math.min(1, (r.width - 24) / n.width, (r.height - 24) / n.height);
    setView({ k, x: (r.width - n.width * k) / 2 - shiftX * k, y: Math.max(12, (r.height - n.height * k) / 2) - shiftY * k });
  }, [lay]);
  // Refit for a new graph or direction (not when tables are expanded or dragged).
  useEffect(() => {
    fitted.current = false;
  }, [graph, dir]);
  useEffect(() => {
    if (visible && lay && !fitted.current) {
      fitted.current = true;
      requestAnimationFrame(fit);
    }
  }, [visible, lay, fit]);

  // Drag a node to move it, drag the background to pan; trackpad scrolls, ⌘/Ctrl + wheel or pinch zooms.
  const drag = useRef<{ x: number; y: number; vx: number; vy: number; moved: boolean; node?: string } | null>(null);
  useEffect(() => {
    const el = box.current;
    if (!el) return;
    const onWheel = (e: WheelEvent) => {
      e.preventDefault();
      const r = el.getBoundingClientRect();
      if (e.ctrlKey || e.metaKey) {
        const px = e.clientX - r.left;
        const py = e.clientY - r.top;
        setView((v) => {
          const k = Math.min(3, Math.max(0.15, v.k * Math.exp(-e.deltaY * 0.01)));
          return { k, x: px - ((px - v.x) * k) / v.k, y: py - ((py - v.y) * k) / v.k };
        });
      } else setView((v) => ({ ...v, x: v.x - e.deltaX, y: v.y - e.deltaY }));
    };
    el.addEventListener("wheel", onWheel, { passive: false });
    return () => el.removeEventListener("wheel", onWheel);
  }, []);

  const revealSql = (range?: { start: number; end: number }) => {
    if (!ref || !range || !tab) return;
    const st = useStore.getState();
    const v = editorBridge.get(ref.source);
    if (!v) return st.toast("The editor this lineage came from is closed", "info");
    const doc = v.state.doc.toString();
    const from = ref.base + range.start;
    const to = ref.base + range.end;
    if (doc.slice(ref.base, ref.base + tab.sql.length) !== tab.sql) return st.toast("The SQL changed since: press Refresh first", "info");
    const owner = ref.source.startsWith("nb:") ? st.tabs.find((t) => t.notebook_id && ref.source.startsWith(`nb:${t.notebook_id}:`)) : st.tabs.find((t) => t.id === ref.source);
    if (owner) st.setActiveTab(owner.id);
    requestAnimationFrame(() => {
      v.dispatch({ selection: { anchor: from, head: to }, scrollIntoView: true });
      v.focus();
    });
  };

  const refresh = () => {
    if (!tab || !ref) return;
    const v = editorBridge.get(ref.source);
    if (ref.whole && v) {
      const doc = v.state.doc.toString();
      if (doc !== tab.sql) return useStore.getState().updateTab(tab.id, { sql: doc });
    }
    void run();
  };

  const doExport = async (format: ExportFormat) => {
    setMenu(null);
    if (!graph || !lay) return;
    const ext = EXPORTS.find((e) => e.format === format)!.ext;
    const base = (tab?.title ?? "lineage").replace(/^Lineage · /, "").replace(/[^\w.-]+/g, "_") || "lineage";
    try {
      const path = await saveDialog({ defaultPath: `${base}-lineage.${ext}`, filters: [{ name: EXPORTS.find((e) => e.format === format)!.label, extensions: [ext] }] });
      if (!path) return;
      // What's on screen: direction and moved tables.
      const n = normalize(lay);
      if (format === "png") await api.saveBase64File(path, await svgToPng(toSvg(graph, n), n.width, n.height));
      else await api.saveTextFile(path, exportText(format, graph, lay));
      useStore.getState().toast(`Saved ${path.split(/[\\/]/).pop()}`, "success");
    } catch (e) {
      useStore.getState().toast(toError(e).message, "error");
    }
  };

  if (!tab || !ref) return null;
  const checks = load.status === "ready" ? load.view.checks : [];
  const summary = checkSummary(checks, conn?.name);
  const warnings = graph?.warnings ?? [];
  const focusNode = focus && graph?.nodes.find((n) => n.id === focus.node);
  const focusCol = focusNode && focus?.column !== undefined ? focusNode.columns.find((c) => c.name === focus.column) : undefined;
  const flow = useMemo(() => (graph && focus ? columnFlow(graph, focus.node, focus.column) : null), [graph, focus]);
  const formula = useMemo(() => (graph && focus && tab ? explainColumn(graph, tab.sql, focus.node, focus.column)?.formula : undefined), [graph, focus, tab]);
  return (
    <div className="h-full min-h-0 flex-col" style={{ display: visible ? "flex" : "none" }}>
      <div className="flex h-10 shrink-0 items-center gap-2 border-b border-line px-3 text-[12.5px]">
        <Network size={14} className="shrink-0 text-muted" />
        <span className="min-w-0 truncate font-medium">{tab.title}</span>
        {graph && (
          <span className="shrink-0 text-muted">
            · {graph.statements.length} statement{graph.statements.length === 1 ? "" : "s"} · {graph.nodes.filter((n) => n.name).length} nodes
          </span>
        )}
        {load.status === "ready" && (
          <button
            className={`flex min-w-0 items-center gap-1 truncate rounded px-1.5 py-0.5 text-[11.5px] ${
              summary.tone === "ok" ? "text-success" : summary.tone === "error" ? "bg-danger/10 text-danger hover:bg-danger/20" : "text-muted"
            }`}
            title={summary.first?.message ?? summary.text}
            onClick={() => summary.first?.position !== undefined && revealSql({ start: summary.first.position, end: summary.first.position + 1 })}
          >
            {summary.tone === "ok" ? <CheckCircle2 size={12} /> : summary.tone === "error" ? <AlertCircle size={12} /> : checks.some((c) => c.status === "offline") ? <WifiOff size={12} /> : null}
            <span className="truncate">
              {summary.text}
              {summary.first?.message ? `: ${summary.first.message}` : ""}
            </span>
          </button>
        )}
        <div className="ml-auto flex shrink-0 items-center gap-1">
          <label className="flex items-center gap-1 text-[11.5px] text-muted" title="Describe uncached tables and check each query with EXPLAIN (never runs it)">
            <input type="checkbox" checked={check} disabled={!conn} onChange={(e) => setCheck(e.target.checked)} /> Check with database
          </label>
          {!columnTab && (
            <>
          <div className="flex items-center rounded-md border border-line" role="group" aria-label="Diagram direction">
            {(
              [
                ["lr", <ArrowRight key="r" size={13} />, "Left to right: sources on the left"],
                ["tb", <ArrowDown key="d" size={13} />, "Top to bottom: sources on top"],
              ] as const
            ).map(([d, icon, title]) => (
              <button
                key={d}
                className={`flex h-6 w-7 items-center justify-center ${dir === d ? "bg-hover text-fg" : "text-muted hover:text-fg"}`}
                aria-pressed={dir === d}
                aria-label={title}
                title={title}
                onClick={() => setDir(d)}
              >
                {icon}
              </button>
            ))}
          </div>
          <button
            className="btn-ghost py-1"
            disabled={!graph}
            onClick={() => setExpanded((e) => (e === "all" ? new Set() : "all"))}
            title={expanded === "all" ? "Show tables only (click a table to list its columns)" : "List the columns of every table"}
          >
            {expanded === "all" ? <Rows3 size={13} /> : <Columns3 size={13} />} {expanded === "all" ? "Tables only" : "All columns"}
          </button>
          {Object.keys(pos).length > 0 && (
            <button className="btn-ghost py-1" onClick={() => setPos({})} title="Put moved tables back where the layout puts them">
              Reset layout
            </button>
          )}
          <button className="btn-ghost py-1" onClick={fit} disabled={!lay} title="Fit the diagram to the window">
            <Maximize2 size={13} /> Fit
          </button>
            </>
          )}
          <button className="btn-ghost py-1" onClick={refresh} disabled={load.status === "loading"} title="Trace the editor's current SQL again">
            <RefreshCw size={13} className={load.status === "loading" ? "animate-spin" : ""} /> Refresh
          </button>
          {!columnTab && (
          <button className="btn-ghost py-1" disabled={!graph} onClick={(e) => setMenu({ x: e.clientX, y: e.clientY + 8 })} aria-haspopup="menu">
            <Download size={13} /> Export <ChevronDown size={11} />
          </button>
          )}
        </div>
      </div>
      {menu && (
        <Popover x={menu.x - 200} y={menu.y} onClose={() => setMenu(null)} className="w-56">
          {EXPORTS.map((e) => (
            <MenuItem key={e.format} label={e.label} hint={`.${e.ext}`} onClick={() => void doExport(e.format)} />
          ))}
          <MenuSeparator />
          <MenuItem
            icon={<ClipboardCopy size={13} />}
            label="Copy Mermaid"
            onClick={() => {
              setMenu(null);
              if (graph && lay) void copyText(toMermaid(graph, lay), "Mermaid diagram");
            }}
          />
        </Popover>
      )}
      {columnTab && (
        <div className="min-h-0 flex-1 overflow-auto bg-bg p-4 text-[12px]">
          {load.status === "loading" && (
            <div className="flex items-center justify-center gap-2 text-muted">
              <Loader2 size={15} className="animate-spin text-accent" /> Tracing lineage…
            </div>
          )}
          {load.status === "error" && <div className="text-danger">{load.message}</div>}
          {load.status === "ready" && !flow && <div className="text-muted">Column {ref.column?.node}.{ref.column?.column} is no longer in this SQL. Refresh the source lineage tab.</div>}
          {flow && (
            <div className="mx-auto w-max min-w-full space-y-4">
              <Flow f={flow} onShow={revealSql} onTrace={(node, column) => setFocus({ node, column })} top />
              {formula && flow.children.length > 0 && (
                <pre className="mx-auto max-w-3xl whitespace-pre-wrap break-words rounded bg-panel-2 px-2 py-1.5 font-mono text-[11.5px]">
                  {focus?.column} = {formula}
                </pre>
              )}
            </div>
          )}
        </div>
      )}
      <div ref={area} className={`relative min-h-0 flex-1 ${columnTab ? "hidden" : "flex"}`}>
        <div
          ref={box}
          className="relative min-h-0 min-w-0 flex-1 cursor-grab overflow-hidden bg-bg active:cursor-grabbing"
          onMouseDown={(e) => {
            if (e.button !== 0) return;
            const id = (e.target as Element).closest("[data-node]")?.getAttribute("data-node") ?? undefined;
            const n = id ? lay?.byId.get(id) : undefined;
            drag.current = n ? { x: e.clientX, y: e.clientY, vx: n.x, vy: n.y, moved: false, node: id } : { x: e.clientX, y: e.clientY, vx: view.x, vy: view.y, moved: false };
          }}
          onMouseMove={(e) => {
            const d = drag.current;
            if (!d) return;
            if (Math.abs(e.clientX - d.x) + Math.abs(e.clientY - d.y) > 3) d.moved = true;
            if (!d.moved) return;
            const node = d.node;
            if (node) {
              const nx = d.vx + (e.clientX - d.x) / view.k;
              const ny = d.vy + (e.clientY - d.y) / view.k;
              setPos((p) => ({ ...p, [node]: { x: Math.round(nx), y: Math.round(ny) } }));
            } else setView((v) => ({ ...v, x: d.vx + e.clientX - d.x, y: d.vy + e.clientY - d.y }));
          }}
          onMouseUp={(e) => {
            const d = drag.current;
            drag.current = null;
            if (d?.moved) return;
            const target = e.target as Element;
            const hit = target.closest("[data-node]");
            if (!hit) return setFocus(null);
            const node = hit.getAttribute("data-node")!;
            const column = target.closest("[data-col]")?.getAttribute("data-col") ?? undefined;
            // A column: trace it. The header (or "N columns"): show/hide the table's columns.
            if (column !== undefined) return setFocus((f) => (f && f.node === node && f.column === column ? null : { node, column }));
            if (target.closest("[data-head]") || !hit.getAttribute("data-col")) toggleNode(node);
          }}
          onMouseLeave={() => (drag.current = null)}
        >
          {load.status === "loading" && (
            <div className="absolute inset-0 flex items-center justify-center gap-2 text-[12.5px] text-muted" role="status">
              <Loader2 size={15} className="animate-spin text-accent" /> Tracing lineage{check && conn ? ` and checking with ${conn.name}` : ""}…
            </div>
          )}
          {load.status === "error" && (
            <div className="absolute inset-0 flex items-center justify-center px-6 text-center text-[12.5px] text-danger" role="alert">
              {load.message}
            </div>
          )}
          {graph && lay && graph.nodes.length === 0 && (
            <div className="absolute inset-0 flex items-center justify-center text-[12.5px] text-muted">No tables or columns found in this SQL.</div>
          )}
          {graph && lay && (
            <svg className="absolute inset-0 h-full w-full select-none" role="img" aria-label="Lineage diagram">
              <g transform={`translate(${view.x},${view.y}) scale(${view.k})`} dangerouslySetInnerHTML={{ __html: inner }} />
            </svg>
          )}
          {graph && (
            <div className="pointer-events-none absolute bottom-2 left-2 flex gap-3 rounded-md border border-line bg-panel/90 px-2 py-1 text-[10.5px] text-muted">
              <Legend color={colors.muted} label="copied" />
              <Legend color={colors.accent} label="computed" />
              <Legend color={colors.accent} label="aggregated" width={2.6} />
              <Legend color={colors.warning} label="filter" dash="5 4" />
              <Legend color={colors.warning} label="join" dash="2 3" />
              <span>· click a table to list its columns, a column to trace it · drag a table to move it · ⌘+scroll to zoom</span>
            </div>
          )}
          {warnings.length > 0 && !showWarnings && !columnTab && (
            <button
              className="absolute right-2 top-2 flex items-center gap-1 rounded-md border border-warning/40 bg-panel px-2 py-1 text-[11.5px] text-warning shadow"
              onMouseDown={(e) => e.stopPropagation()}
              onMouseUp={(e) => e.stopPropagation()}
              onClick={() => setShowWarnings(true)}
            >
              <AlertTriangle size={12} /> {warnings.length} not traced{graph?.partial ? " · partial" : ""}
            </button>
          )}
        </div>
        {(focusNode || (showWarnings && warnings.length > 0)) && (
          <SplitHandle
            side="right"
            width={panelW}
            min={260}
            max={() => Math.max(260, (area.current?.getBoundingClientRect().width ?? 1200) - 240)}
            onChange={setPanelW}
            onCommit={(w) => localStorage.setItem(PANEL_KEY, String(w))}
            onReset={() => {
              setPanelW(PANEL_DEFAULT);
              localStorage.removeItem(PANEL_KEY);
            }}
            label="Resize lineage details"
          />
        )}
        {(focusNode || (showWarnings && warnings.length > 0)) && (
          <aside className="shrink-0 overflow-y-auto border-l border-line bg-panel p-3 text-[12px]" style={{ width: panelW }} aria-label="Lineage details">
            {focusNode && (
              <div className="space-y-2">
                <div className="flex items-start justify-between gap-2">
                  <div className="min-w-0">
                    <div className="truncate font-mono font-medium">
                      {focusNode.name}
                      {focus?.column !== undefined ? `.${focus.column}` : ""}
                    </div>
                    <div className="text-[11px] text-muted">{kindLabel(focusNode)}</div>
                  </div>
                  <div className="flex shrink-0 items-center">
                    <button
                      className="icon-btn h-6 w-6"
                      aria-label="Open this column's lineage in a new tab"
                      title="Open in a new tab"
                      onClick={() => tab && openColumnLineage(tab, focusNode.name, focus?.column)}
                    >
                      <ExternalLink size={12} />
                    </button>
                    <button className="icon-btn h-6 w-6" aria-label="Close details" onClick={() => setFocus(null)}>
                      <X size={12} />
                    </button>
                  </div>
                </div>
                {flow && (
                  <div className="overflow-x-auto pb-1">
                    <div className="mx-auto w-max min-w-full">
                      <Flow f={flow} onShow={revealSql} onTrace={(node, column) => setFocus({ node, column })} top />
                    </div>
                  </div>
                )}
                {formula && flow && flow.children.length > 0 && (
                  <div>
                    <div className="mb-1 text-[11px] font-medium uppercase tracking-wide text-muted">In database columns</div>
                    <pre className="whitespace-pre-wrap break-words rounded bg-panel-2 px-2 py-1.5 font-mono text-[11px]">
                      {focus?.column} = {formula}
                    </pre>
                  </div>
                )}
                {(focusCol?.span ?? focusNode.span) && (
                  <button className="btn-ghost border border-line py-1" onClick={() => revealSql(focusCol?.span ?? focusNode.span)}>
                    <TextCursorInput size={12} /> Show in SQL
                  </button>
                )}
              </div>
            )}
            {showWarnings && warnings.length > 0 && (
              <div className={`space-y-1 ${focusNode ? "mt-4 border-t border-line pt-3" : ""}`}>
                <div className="flex items-center justify-between">
                  <span className="text-[11px] font-medium uppercase tracking-wide text-muted">Not traced</span>
                  <button className="icon-btn h-6 w-6" aria-label="Hide warnings" onClick={() => setShowWarnings(false)}>
                    <X size={12} />
                  </button>
                </div>
                {warnings.map((w, i) => (
                  <button key={i} className="block w-full rounded px-1 py-0.5 text-left hover:bg-hover" onClick={() => revealSql(w.span)} disabled={!w.span}>
                    {w.message}
                  </button>
                ))}
              </div>
            )}
          </aside>
        )}
      </div>
    </div>
  );
}

function Legend({ color, label, dash, width = 1.4 }: { color: string; label: string; dash?: string; width?: number }) {
  return (
    <span className="flex items-center gap-1">
      <svg width="18" height="6" aria-hidden>
        <line x1="0" y1="3" x2="18" y2="3" stroke={color} strokeWidth={width} strokeDasharray={dash} />
      </svg>
      {label}
    </span>
  );
}

const VIA_TEXT = { direct: "copied", transform: "computed", aggregate: "aggregated" } as const;

/**
 * A column's lineage as a small top-down flow: the column, an arrow with the
 * expression (and the joins/filters where several inputs meet), then the
 * input columns side by side ("+"), each continuing down to its table.
 */
function Flow({ f, onShow, onTrace, top = false }: { f: FlowNode; onShow: (r?: { start: number; end: number }) => void; onTrace: (node: string, column?: string) => void; top?: boolean }) {
  const n = f.node;
  const kind = n.kind === "table" ? (n.written ? "table (written)" : "table") : n.kind === "cte" ? "CTE" : n.kind === "result" ? "result" : n.kind === "set_op" ? "union" : "subquery";
  const many = f.children.length > 1;
  return (
    <div className="flex flex-col items-center">
      {f.through.length > 0 && <div className="mb-0.5 text-[10px] text-muted">via {f.through.join(" → ")}</div>}
      <div
        className={`group max-w-[15rem] rounded-md border px-2 py-1 text-center ${top ? "border-accent bg-accent/10" : f.repeat ? "border-dashed border-line" : "border-line bg-panel-2"}`}
      >
        <button className="block max-w-full truncate font-mono text-[12px] font-semibold hover:underline" onClick={() => onShow(f.span)} disabled={!f.span} title="Show in SQL">
          {f.column ?? "*"}
        </button>
        <div className="flex items-center justify-center gap-1 text-[10.5px] text-muted">
          <span className="truncate" title={n.name}>
            {kind === "table" ? n.name : `${n.name} · ${kind}`}
          </span>
          {!top && !f.repeat && (
            <button className="opacity-0 hover:text-fg group-hover:opacity-100" title="Trace from this column" onClick={() => onTrace(n.id, f.column)}>
              ⤷
            </button>
          )}
        </div>
        {f.repeat && <div className="text-[10px] italic text-muted">shown above</div>}
      </div>
      {f.children.length > 0 && (
        <>
          <div className="h-2 w-px bg-line" />
          <div className="flex max-w-[22rem] flex-col items-center gap-0.5 rounded border border-line bg-panel px-1.5 py-1 text-center">
            {f.expr ? (
              <span className={`break-words font-mono text-[11px] ${f.via === "aggregate" ? "font-semibold text-accent" : "text-accent"}`}>{f.expr}</span>
            ) : (
              <span className="text-[10.5px] text-muted">{VIA_TEXT[f.via ?? "direct"]}</span>
            )}
            {f.conditions.map((c, i) => (
              <button key={i} className="break-words font-mono text-[10.5px] text-warning hover:underline" onClick={() => onShow(c.span)} disabled={!c.span} title="Show in SQL">
                {conditionText(c)}
              </button>
            ))}
          </div>
          <div className="h-2 w-px bg-line" />
          <div className="text-[9px] leading-none text-muted">▼</div>
          <div className="flex items-start">
            {f.children.map((c, i) => (
              <div key={i} className="flex items-start">
                {i > 0 && (
                  <div className="relative px-1 pt-4 text-[12px] font-semibold text-muted">
                    <span className="absolute inset-x-0 top-0 h-px bg-line" />+
                  </div>
                )}
                <div className="relative flex flex-col items-center pt-2">
                  {/* The bar where the arrows to several inputs meet. */}
                  {many && (
                    <span
                      className="absolute top-0 h-px bg-line"
                      style={i === 0 ? { left: "50%", right: 0 } : i === f.children.length - 1 ? { left: 0, right: "50%" } : { left: 0, right: 0 }}
                    />
                  )}
                  {many && <span className="absolute left-1/2 top-0 h-2 w-px bg-line" />}
                  <Flow f={c} onShow={onShow} onTrace={onTrace} />
                </div>
              </div>
            ))}
          </div>
        </>
      )}
    </div>
  );
}
