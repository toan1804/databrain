import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import { useEvent, useWidth } from "../lib/hooks";
import { save as saveDialog } from "@tauri-apps/plugin-dialog";
import { ArrowDown, ArrowRight, ChevronDown, ClipboardCopy, Columns3, Download, Loader2, Maximize2, Minus, Plus, RefreshCw, Rows3, Search, Share2 } from "lucide-react";
import { api, isTauri, toError } from "../lib/api";
import { kbd } from "../lib/platform";
import type { TableColumns } from "../lib/types";
import { erGraph } from "../lib/erGraph";
import { layout, normalize, trace, withPositions, type Direction, type Positions } from "../lib/lineageLayout";
import { EXPORTS, exportText, toMermaid, toSvg, type ExportFormat } from "../lib/lineageExport";
import { useStore, type Tab } from "../store";
import { copyText } from "./CatalogMenus";
import { svgToPng, themeColors } from "./LineageTab";
import { MenuItem, MenuSeparator, Popover } from "./ui";

type Load = { status: "loading" } | { status: "ready"; tables: TableColumns[] } | { status: "error"; message: string };

const DIR_KEY = "db.er.dir";

/** ER diagram of a schema (read-only tab): tables and their foreign keys, pan/zoom, export. */
export function ErTab({ tabId, visible }: { tabId: string; visible: boolean }) {
  const tab = useStore((s) => s.tabs.find((t) => t.id === tabId)) as Tab | undefined;
  const theme = useStore((s) => s.theme);
  const appearance = useStore((s) => s.appearance);
  const ref = tab?.er;
  const [load, setLoad] = useState<Load>({ status: "loading" });
  const [view, setView] = useState({ x: 0, y: 0, k: 1 });
  const [menu, setMenu] = useState<{ x: number; y: number } | null>(null);
  // Narrow tab (AI panel open): buttons drop their labels, like the editor's.
  const [barRef, barWidth] = useWidth<HTMLDivElement>();
  const iconOnly = barWidth < 860;
  const [dir, setDirState] = useState<Direction>(() => (localStorage.getItem(DIR_KEY) === "tb" ? "tb" : "lr"));
  const [pos, setPos] = useState<Positions>({});
  /** Columns listed: all by default (key and foreign key columns of wide tables). */
  const [expanded, setExpanded] = useState<Set<string> | "all">("all");
  const [focus, setFocus] = useState<{ node: string; column?: string } | null>(null);
  /** Only tables with a foreign key (in or out). */
  const [linkedOnly, setLinkedOnly] = useState(false);
  const [filter, setFilter] = useState("");
  const box = useRef<HTMLDivElement>(null);
  const seq = useRef(0);
  const fitted = useRef(false);

  const run = useCallback(async () => {
    if (!tab?.connection_id || !ref) return;
    const my = ++seq.current;
    setLoad({ status: "loading" });
    if (!isTauri()) return setLoad({ status: "error", message: "ER diagrams need the DataBrain desktop app" });
    try {
      const tables = await api.schemaTables(tab.connection_id, ref.schema);
      if (my !== seq.current) return;
      setLoad({ status: "ready", tables });
      setPos({});
      setFocus(null);
    } catch (e) {
      if (my === seq.current) setLoad({ status: "error", message: toError(e).message });
    }
  }, [tab?.connection_id, ref]);

  // `ref` is a new object whenever the tab changes; reload for a new schema only.
  const runOnce = useEvent(run);
  const schema = ref?.schema;
  useEffect(() => {
    void runOnce();
  }, [tab?.connection_id, schema, runOnce]);

  const er = useMemo(() => (load.status === "ready" && ref ? erGraph(ref.schema, load.tables) : null), [load, ref]);
  /** What the diagram draws: the filter and "linked only" applied. */
  const shown = useMemo(() => {
    if (!er) return null;
    const g = er.graph;
    const linked = new Set(g.edges.flatMap((e) => [e.from, e.to]));
    const q = filter.trim().toLowerCase();
    const keep = new Set(g.nodes.filter((n) => (!linkedOnly || linked.has(n.id)) && (!q || n.name.toLowerCase().includes(q))).map((n) => n.id));
    return { ...g, nodes: g.nodes.filter((n) => keep.has(n.id)), edges: g.edges.filter((e) => keep.has(e.from) && keep.has(e.to)) };
  }, [er, filter, linkedOnly]);
  const traced = useMemo(() => (shown && focus ? trace(shown, focus.node, focus.column) : null), [shown, focus]);
  const base = useMemo(() => (shown ? layout(shown, dir, { expanded, show: traced?.cols }) : null), [shown, dir, expanded, traced]);
  const lay = useMemo(() => (base ? withPositions(base, pos) : null), [base, pos]);
  // themeColors reads CSS variables, which change with the theme and appearance.
  const colors = useMemo(themeColors, [theme, appearance]);
  const inner = useMemo(() => (shown && lay ? toSvg(shown, lay, { colors, focus: traced, inner: true }) : ""), [shown, lay, colors, traced]);

  const setDir = (d: Direction) => {
    localStorage.setItem(DIR_KEY, d);
    setDirState(d);
    setPos({});
  };
  const toggleNode = (id: string) =>
    setExpanded((e) => {
      const all = shown?.nodes.map((n) => n.id) ?? [];
      const next = new Set(e === "all" ? all : e);
      if (next.has(id)) next.delete(id);
      else next.add(id);
      return next.size === all.length && all.length > 0 ? "all" : next;
    });

  const zoomBy = useCallback((f: number) => {
    const r = box.current?.getBoundingClientRect();
    const px = r ? r.width / 2 : 0;
    const py = r ? r.height / 2 : 0;
    setView((v) => {
      const k = Math.min(3, Math.max(0.1, v.k * f));
      return { k, x: px - ((px - v.x) * k) / v.k, y: py - ((py - v.y) * k) / v.k };
    });
  }, []);

  const fit = useCallback(() => {
    const el = box.current;
    if (!el || !lay || !lay.nodes.length) return;
    const r = el.getBoundingClientRect();
    if (!r.width || !r.height) return;
    const n = normalize(lay);
    const shiftX = Math.min(...lay.nodes.map((x) => x.x)) - Math.min(...n.nodes.map((x) => x.x));
    const shiftY = Math.min(...lay.nodes.map((x) => x.y)) - Math.min(...n.nodes.map((x) => x.y));
    const k = Math.min(1, (r.width - 24) / n.width, (r.height - 24) / n.height);
    setView({ k, x: (r.width - n.width * k) / 2 - shiftX * k, y: Math.max(12, (r.height - n.height * k) / 2) - shiftY * k });
  }, [lay]);
  useEffect(() => {
    fitted.current = false;
  }, [er, dir, linkedOnly, filter]);
  useEffect(() => {
    if (visible && lay && !fitted.current) {
      fitted.current = true;
      requestAnimationFrame(fit);
    }
  }, [visible, lay, fit]);

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
          const k = Math.min(3, Math.max(0.1, v.k * Math.exp(-e.deltaY * 0.01)));
          return { k, x: px - ((px - v.x) * k) / v.k, y: py - ((py - v.y) * k) / v.k };
        });
      } else setView((v) => ({ ...v, x: v.x - e.deltaX, y: v.y - e.deltaY }));
    };
    el.addEventListener("wheel", onWheel, { passive: false });
    return () => el.removeEventListener("wheel", onWheel);
  }, []);

  const doExport = async (format: ExportFormat) => {
    setMenu(null);
    if (!shown || !lay) return;
    const fmt = EXPORTS.find((e) => e.format === format)!;
    const base = (ref?.label ?? "schema").replace(/[^\w.-]+/g, "_") || "schema";
    try {
      const path = await saveDialog({ defaultPath: `${base}-er.${fmt.ext}`, filters: [{ name: fmt.label, extensions: [fmt.ext] }] });
      if (!path) return;
      const n = normalize(lay);
      if (format === "png") await api.saveBase64File(path, await svgToPng(toSvg(shown, n), n.width, n.height));
      else await api.saveTextFile(path, exportText(format, shown, lay));
      useStore.getState().toast(`Saved ${path.split(/[\\/]/).pop()}`, "success");
    } catch (e) {
      useStore.getState().toast(toError(e).message, "error");
    }
  };

  if (!tab || !ref) return null;
  const tableCount = er ? er.graph.nodes.length - er.external.length : 0;
  return (
    <div className="h-full min-h-0 flex-col" style={{ display: visible ? "flex" : "none" }}>
      <div ref={barRef} className="flex h-10 shrink-0 items-center gap-2 border-b border-line px-3 text-[12.5px]">
        <Share2 size={14} className="shrink-0 text-muted" />
        <span className="min-w-0 truncate font-medium">{tab.title}</span>
        {er && !iconOnly && (
          <span className="shrink-0 text-muted">
            · {tableCount} table{tableCount === 1 ? "" : "s"} · {er.foreignKeys} foreign key{er.foreignKeys === 1 ? "" : "s"}
            {er.external.length > 0 ? ` · ${er.external.length} in other schemas` : ""}
          </span>
        )}
        <div className="ml-auto flex shrink-0 items-center gap-1">
          <label className="flex h-6 items-center gap-1 rounded-md border border-line px-1.5 text-muted">
            <Search size={12} />
            <input className={`${iconOnly ? "w-20" : "w-32"} bg-transparent text-[11.5px] text-fg outline-none`} placeholder={iconOnly ? "Filter" : "Filter tables"} value={filter} onChange={(e) => setFilter(e.target.value)} aria-label="Filter tables" />
          </label>
          <label className="flex items-center gap-1 text-[11.5px] text-muted" title="Hide tables without foreign keys">
            <input type="checkbox" checked={linkedOnly} onChange={(e) => setLinkedOnly(e.target.checked)} /> {iconOnly ? "Linked" : "Linked only"}
          </label>
          <button
            className="btn-ghost py-1"
            disabled={!shown}
            onClick={() => setExpanded((e) => (e === "all" ? new Set() : "all"))}
            title={expanded === "all" ? "Show tables only (click a table to list its columns)" : "List the columns of every table"}
            aria-label={expanded === "all" ? "Tables only" : "All columns"}
          >
            {expanded === "all" ? <Rows3 size={13} /> : <Columns3 size={13} />} {!iconOnly && (expanded === "all" ? "Tables only" : "All columns")}
          </button>
          {Object.keys(pos).length > 0 && (
            <button className="btn-ghost py-1" onClick={() => setPos({})} title="Put moved tables back where the layout puts them">
              {iconOnly ? "Reset" : "Reset layout"}
            </button>
          )}
          <button className="btn-ghost py-1" onClick={fit} disabled={!lay} title="Fit the diagram to the window" aria-label="Fit">
            <Maximize2 size={13} /> {!iconOnly && "Fit"}
          </button>
          <button className="btn-ghost py-1" onClick={() => void run()} disabled={load.status === "loading"} title="Read the schema again" aria-label="Refresh">
            <RefreshCw size={13} className={load.status === "loading" ? "animate-spin" : ""} /> {!iconOnly && "Refresh"}
          </button>
          <button className="btn-ghost py-1" disabled={!shown} onClick={(e) => setMenu({ x: e.clientX, y: e.clientY + 8 })} aria-haspopup="menu" title="Export the diagram" aria-label="Export">
            <Download size={13} /> {!iconOnly && "Export"} <ChevronDown size={11} />
          </button>
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
              if (shown && lay) void copyText(toMermaid(shown, lay), "Mermaid diagram");
            }}
          />
        </Popover>
      )}
      <div
        ref={box}
        className="relative min-h-0 flex-1 cursor-grab overflow-hidden bg-bg active:cursor-grabbing"
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
          if (node) setPos((p) => ({ ...p, [node]: { x: Math.round(d.vx + (e.clientX - d.x) / view.k), y: Math.round(d.vy + (e.clientY - d.y) / view.k) } }));
          else setView((v) => ({ ...v, x: d.vx + e.clientX - d.x, y: d.vy + e.clientY - d.y }));
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
          if (column !== undefined) return setFocus((f) => (f && f.node === node && f.column === column ? null : { node, column }));
          if (target.closest("[data-head]")) {
            // The header: highlight the table's links (a second click lists or hides its columns).
            if (focus?.node === node && focus.column === undefined) return toggleNode(node);
            return setFocus({ node });
          }
          toggleNode(node);
        }}
        onMouseLeave={() => (drag.current = null)}
      >
        {load.status === "loading" && (
          <div className="absolute inset-0 flex items-center justify-center gap-2 text-[12.5px] text-muted" role="status">
            <Loader2 size={15} className="animate-spin text-accent" /> Reading tables and foreign keys…
          </div>
        )}
        {load.status === "error" && (
          <div className="absolute inset-0 flex items-center justify-center px-6 text-center text-[12.5px] text-danger" role="alert">
            {load.message}
          </div>
        )}
        {shown && shown.nodes.length === 0 && (
          <div className="absolute inset-0 flex items-center justify-center text-[12.5px] text-muted">
            {er?.graph.nodes.length ? "No tables match." : "This schema has no tables."}
          </div>
        )}
        {shown && lay && shown.nodes.length > 0 && (
          <svg className="absolute inset-0 h-full w-full select-none" role="img" aria-label="ER diagram">
            <g transform={`translate(${view.x},${view.y}) scale(${view.k})`} dangerouslySetInnerHTML={{ __html: inner }} />
          </svg>
        )}
        {shown && lay && (
          <div
            className="absolute bottom-2 right-2 flex flex-col overflow-hidden rounded-md border border-line bg-panel shadow"
            role="group"
            aria-label="Diagram view controls"
            onMouseDown={(e) => e.stopPropagation()}
            onMouseUp={(e) => e.stopPropagation()}
          >
            <button className="flex h-7 w-7 items-center justify-center text-muted hover:bg-hover hover:text-fg" aria-label="Zoom in" title="Zoom in" onClick={() => zoomBy(1.25)}>
              <Plus size={14} />
            </button>
            <div className="border-t border-line px-0.5 py-0.5 text-center text-[9.5px] tabular-nums text-muted" aria-live="polite" title="Zoom">
              {Math.round(view.k * 100)}%
            </div>
            <button className="flex h-7 w-7 items-center justify-center border-t border-line text-muted hover:bg-hover hover:text-fg" aria-label="Zoom out" title="Zoom out" onClick={() => zoomBy(0.8)}>
              <Minus size={14} />
            </button>
            <button
              className="flex h-7 w-7 items-center justify-center border-t border-line text-muted hover:bg-hover hover:text-fg"
              aria-label={dir === "lr" ? "Lay out top to bottom" : "Lay out left to right"}
              title={dir === "lr" ? "Switch to top to bottom (referenced tables on top)" : "Switch to left to right (referenced tables on the left)"}
              onClick={() => setDir(dir === "lr" ? "tb" : "lr")}
            >
              {dir === "lr" ? <ArrowDown size={14} /> : <ArrowRight size={14} />}
            </button>
          </div>
        )}
        {shown && shown.nodes.length > 0 && (
          <div className="pointer-events-none absolute bottom-2 left-2 rounded-md border border-line bg-panel/90 px-2 py-1 text-[10.5px] text-muted">
            Lines go from the referenced key to the foreign key · ↺ references its own table · click a table or column to highlight its links · drag a table to move it · {kbd("⌘")}+scroll to zoom
          </div>
        )}
      </div>
    </div>
  );
}
