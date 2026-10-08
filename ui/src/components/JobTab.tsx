// Job editor tab: steps (nodes) on a canvas you can pan, zoom and drag, with
// links from upstream to downstream steps (drag a step's bottom handle onto
// another step), a side panel to edit the selected step's connection and SQL
// (or the job's schedule and runs), and runs with live status per step.
import { useCallback, useEffect, useMemo, useRef, useState, type PointerEvent as ReactPointerEvent } from "react";
import { open as openDialog } from "@tauri-apps/plugin-dialog";
import {
  AlertTriangle,
  FileDown,
  FolderOpen,
  FlaskConical,
  Info,
  ArrowDownToLine,
  CalendarClock,
  CheckCircle2,
  CircleDashed,
  Copy,
  Database,
  Eye,
  Loader2,
  Maximize2,
  Play,
  PlayCircle,
  Plus,
  Square,
  Trash2,
  XCircle,
} from "lucide-react";
import { api, toError } from "../lib/api";
import type { BatchLimits, FileFormat, Job, JobEdge, JobNode, JobRun, JobSchedule, LoadDryRun, LoadMode, NodeRunSummary, RunLogEntry, StepProgress } from "../lib/types";
import {
  NODE_H,
  NODE_W,
  addDownstream,
  addEdge,
  ancestors,
  descendants,
  edgeProblem,
  exportFileName,
  nameProblem as nameFormatProblem,
  missingLinks,
  newNode,
  nodeStatus,
  parseKeyColumns,
  STEP_ACTIONS,
  actionSettings,
  changeAction,
  usesDefaultRows,
  batchValue,
  readSql,
  removeEdge,
  removeNode,
  scheduleText,
  updateStep,
  upstreamOf,
} from "../lib/jobGraph";
import { formatCount, formatDuration, relativeTime } from "../lib/util";
import { isQueryTab, useStore } from "../store";
import { useJobs } from "../jobsStore";
import { openOutput, resultsConnection } from "../outputs";
import { CellEditor } from "./Notebook";
import { SplitHandle } from "./SplitHandle";
import { ConnDot, MenuItem, MenuSeparator, Popover } from "./ui";

const SAVE_DELAY = 600;
const PANEL_KEY = "db.job.panel";
const PANEL_DEFAULT = 440;
const EDITOR_KEY = (job: string, node: string) => `job:${job}:${node}`;

type Selection = { node: string } | { edge: JobEdge } | null;
type Menu = { x: number; y: number; node?: string; at?: { x: number; y: number } } | null;

const isResults = (c: { config: { options?: Record<string, string> } }) => c.config.options?.databrain_results === "1";

export function JobTab({ tabId, jobId, visible }: { tabId: string; jobId: string; visible: boolean }) {
  const toast = useStore((s) => s.toast);
  const connections = useStore((s) => s.connections);
  const run = useJobs((s) => s.runs[jobId]);
  const taken = useTakenNames(jobId);
  const running = useJobs((s) => !!s.running[jobId]);
  const [job, setJob] = useState<Job | null>(null);
  const [loadError, setLoadError] = useState<string | null>(null);
  const [save, setSave] = useState<{ state: "saved" | "pending" | "saving" | "error"; message?: string }>({ state: "saved" });
  const [sel, setSel] = useState<Selection>(null);
  const [menu, setMenu] = useState<Menu>(null);
  const [view, setView] = useState({ x: 40, y: 30, k: 1 });
  const [link, setLink] = useState<{ from: string; x: number; y: number } | null>(null);
  const [panelW, setPanelW] = useState(() => Number(localStorage.getItem(PANEL_KEY)) || PANEL_DEFAULT);
  const box = useRef<HTMLDivElement>(null);
  const area = useRef<HTMLDivElement>(null);
  const latest = useRef<Job | null>(null);
  const timer = useRef<ReturnType<typeof setTimeout> | undefined>(undefined);
  /** Edits not saved yet (saved when the tab closes). */
  const dirty = useRef(false);

  useEffect(() => {
    void useJobs.getState().init();
    // DuckDB steps run on the Results connection: make sure it exists (and is listed).
    void resultsConnection().catch(() => {});
    api
      .getJob(jobId)
      .then((j) => {
        latest.current = j;
        setJob(j);
      })
      .catch((e) => setLoadError(toError(e).message));
  }, [jobId]);

  const persist = useCallback(async () => {
    clearTimeout(timer.current);
    const j = latest.current;
    if (!j) return true;
    dirty.current = false;
    setSave({ state: "saving" });
    try {
      await api.saveJob(j);
      setSave({ state: "saved" });
      void useJobs.getState().refresh().catch(() => {});
      return true;
    } catch (e) {
      setSave({ state: "error", message: toError(e).message });
      return false;
    }
  }, []);

  /** Change the job locally and save it shortly after. */
  const update = useCallback(
    (fn: (j: Job) => Job) => {
      const cur = latest.current;
      if (!cur) return;
      const next = fn(cur);
      if (next === cur) return;
      latest.current = next;
      dirty.current = true;
      setJob(next);
      setSave({ state: "pending" });
      clearTimeout(timer.current);
      timer.current = setTimeout(() => void persist(), SAVE_DELAY);
    },
    [persist],
  );
  // Save what's pending when the tab closes.
  useEffect(() => () => void (dirty.current && persist()), [persist]);

  const updateNode = useCallback((id: string, patch: Partial<JobNode>) => update((j) => updateStep(j, id, patch)), [update]);

  // The tab title follows the job's name.
  useEffect(() => {
    if (job) useStore.getState().updateTab(tabId, { title: job.name });
  }, [job?.name]); // eslint-disable-line react-hooks/exhaustive-deps

  // A finished run's results become the steps' last run (already saved by the app).
  useEffect(() => {
    if (!run || run.status === "running" || !latest.current) return;
    const done = new Map(run.nodes.map((r) => [r.node_id, r]));
    const next = { ...latest.current, nodes: latest.current.nodes.map((n) => (done.has(n.id) ? { ...n, last_run: done.get(n.id)! } : n)) };
    latest.current = next;
    setJob(next);
  }, [run]);

  const start = async (nodes?: string[]) => {
    if (!(await persist())) return toast("Fix the job first: " + (save.message ?? "it could not be saved"), "error");
    await useJobs.getState().run(jobId, nodes);
  };

  // ---------------------------------------------------------------- canvas

  const toCanvas = (cx: number, cy: number) => {
    const r = box.current!.getBoundingClientRect();
    return { x: (cx - r.left - view.x) / view.k, y: (cy - r.top - view.y) / view.k };
  };

  const fit = useCallback(() => {
    const j = latest.current;
    const r = box.current?.getBoundingClientRect();
    if (!j || !r || !j.nodes.length) return;
    const minX = Math.min(...j.nodes.map((n) => n.x));
    const minY = Math.min(...j.nodes.map((n) => n.y));
    const maxX = Math.max(...j.nodes.map((n) => n.x + NODE_W));
    const maxY = Math.max(...j.nodes.map((n) => n.y + NODE_H));
    const k = Math.min(1.2, Math.max(0.3, Math.min((r.width - 80) / (maxX - minX), (r.height - 80) / (maxY - minY))));
    setView({ k, x: (r.width - (maxX - minX) * k) / 2 - minX * k, y: (r.height - (maxY - minY) * k) / 2 - minY * k });
  }, []);
  const fitted = useRef(false);
  useEffect(() => {
    if (job && visible && !fitted.current) {
      fitted.current = true;
      requestAnimationFrame(fit);
    }
  }, [job, visible, fit]);

  const onBackgroundDown = (e: ReactPointerEvent) => {
    if (e.button !== 0 || (e.target as HTMLElement).closest("[data-node],[data-edge]")) return;
    setSel(null);
    const start = { cx: e.clientX, cy: e.clientY, ...view };
    const move = (ev: PointerEvent) => setView((v) => ({ ...v, x: start.x + ev.clientX - start.cx, y: start.y + ev.clientY - start.cy }));
    const up = () => {
      window.removeEventListener("pointermove", move);
      window.removeEventListener("pointerup", up);
    };
    window.addEventListener("pointermove", move);
    window.addEventListener("pointerup", up);
  };

  useEffect(() => {
    const el = box.current;
    if (!el) return;
    const wheel = (e: WheelEvent) => {
      e.preventDefault();
      const r = el.getBoundingClientRect();
      if (e.ctrlKey || e.metaKey) {
        setView((v) => {
          const k = Math.min(2, Math.max(0.25, v.k * Math.exp(-e.deltaY / 300)));
          const px = e.clientX - r.left;
          const py = e.clientY - r.top;
          return { k, x: px - ((px - v.x) * k) / v.k, y: py - ((py - v.y) * k) / v.k };
        });
      } else setView((v) => ({ ...v, x: v.x - e.deltaX, y: v.y - e.deltaY }));
    };
    el.addEventListener("wheel", wheel, { passive: false });
    return () => el.removeEventListener("wheel", wheel);
  }, [job !== null]); // eslint-disable-line react-hooks/exhaustive-deps

  const onNodeDown = (e: ReactPointerEvent, n: JobNode) => {
    if (e.button !== 0 || (e.target as HTMLElement).closest("[data-port],button,input,select")) return;
    e.stopPropagation();
    setSel({ node: n.id });
    const start = { cx: e.clientX, cy: e.clientY, x: n.x, y: n.y };
    let moved = false;
    const move = (ev: PointerEvent) => {
      const dx = (ev.clientX - start.cx) / view.k;
      const dy = (ev.clientY - start.cy) / view.k;
      if (!moved && Math.hypot(dx, dy) < 3) return;
      moved = true;
      const x = Math.round(start.x + dx);
      const y = Math.round(start.y + dy);
      const cur = latest.current;
      if (!cur) return;
      // Live position without saving on every move.
      latest.current = { ...cur, nodes: cur.nodes.map((m) => (m.id === n.id ? { ...m, x, y } : m)) };
      setJob(latest.current);
    };
    const up = () => {
      window.removeEventListener("pointermove", move);
      window.removeEventListener("pointerup", up);
      if (moved) update((j) => ({ ...j }));
    };
    window.addEventListener("pointermove", move);
    window.addEventListener("pointerup", up);
  };

  /** Drag from a step's bottom handle onto another step: the second runs after the first. */
  const onPortDown = (e: ReactPointerEvent, n: JobNode) => {
    if (e.button !== 0) return;
    e.stopPropagation();
    e.preventDefault();
    const p = toCanvas(e.clientX, e.clientY);
    setLink({ from: n.id, ...p });
    const move = (ev: PointerEvent) => setLink({ from: n.id, ...toCanvas(ev.clientX, ev.clientY) });
    const up = (ev: PointerEvent) => {
      window.removeEventListener("pointermove", move);
      window.removeEventListener("pointerup", up);
      setLink(null);
      const target = (document.elementFromPoint(ev.clientX, ev.clientY) as HTMLElement | null)?.closest("[data-node]")?.getAttribute("data-node");
      const cur = latest.current;
      if (!cur) return;
      if (!target) {
        // Dropped on empty canvas: a new DuckDB step there, reading this one.
        const at = toCanvas(ev.clientX, ev.clientY);
        const from = cur.nodes.find((x) => x.id === n.id)!;
        if (Math.hypot(at.x - p.x, at.y - p.y) < 30) return;
        const node = newNode(cur, { name: `${from.name}_next`, sql: readSql([from.name]), x: Math.round(at.x - NODE_W / 2), y: Math.round(at.y) }, taken.keys());
        update((j) => ({ ...j, nodes: [...j.nodes, node], edges: [...j.edges, { from: n.id, to: node.id }] }));
        setSel({ node: node.id });
        return;
      }
      if (target === n.id) return;
      const problem = edgeProblem(cur, n.id, target);
      if (problem) return toast(problem, "info");
      update((j) => addEdge(j, n.id, target));
    };
    window.addEventListener("pointermove", move);
    window.addEventListener("pointerup", up);
  };

  // Delete / Backspace removes the selected step or link (not while typing).
  useEffect(() => {
    if (!visible) return;
    const key = (e: KeyboardEvent) => {
      if (e.key !== "Delete" && e.key !== "Backspace") return;
      const t = e.target as HTMLElement;
      if (t.closest("input,textarea,select,.cm-editor,[contenteditable]")) return;
      if (!box.current?.contains(document.activeElement) && document.activeElement !== document.body) return;
      if (sel && "edge" in sel) {
        update((j) => removeEdge(j, sel.edge));
        setSel(null);
      } else if (sel && "node" in sel) deleteNode(sel.node);
    };
    window.addEventListener("keydown", key);
    return () => window.removeEventListener("keydown", key);
  }); // eslint-disable-line react-hooks/exhaustive-deps

  const deleteNode = (id: string) => {
    const n = latest.current?.nodes.find((x) => x.id === id);
    if (!n) return;
    const go = () => {
      update((j) => removeNode(j, id));
      setSel(null);
    };
    if (!n.sql.trim()) return go();
    useStore.getState().askConfirm({ title: `Delete step "${n.name}"?`, reasons: ["Its SQL and links are removed from the job."], confirmLabel: "Delete", onConfirm: go });
  };

  const addAt = (at: { x: number; y: number }, init: Partial<JobNode> = {}) => {
    const cur = latest.current;
    if (!cur) return;
    const node = newNode(cur, { connection_id: connections.find((c) => !isResults(c))?.id ?? null, ...init, x: Math.round(at.x), y: Math.round(at.y) }, taken.keys());
    update((j) => ({ ...j, nodes: [...j.nodes, node] }));
    setSel({ node: node.id });
  };

  const downstream = (id: string, kind: "query" | "load" | "export") => {
    const cur = latest.current;
    if (!cur) return;
    const r = addDownstream(cur, id, kind, {}, taken.keys());
    update(() => r.job);
    setSel({ node: r.id });
  };

  const duplicate = (id: string) => {
    const cur = latest.current;
    const n = cur?.nodes.find((x) => x.id === id);
    if (!cur || !n) return;
    const { id: _id, ...rest } = n;
    const copy = newNode(cur, { ...rest, name: n.name, x: n.x + 30, y: n.y + 30, last_run: null }, taken.keys());
    update((j) => ({ ...j, nodes: [...j.nodes, copy], edges: [...j.edges, ...upstreamOf(j, id).map((from) => ({ from, to: copy.id }))] }));
    setSel({ node: copy.id });
  };

  const selected = sel && "node" in sel ? job?.nodes.find((n) => n.id === sel.node) : undefined;
  const related = useMemo(() => {
    if (!job || !selected) return null;
    return { up: ancestors(job, selected.id), down: descendants(job, selected.id) };
  }, [job, selected]);
  const warnings = useMemo(() => (job ? missingLinks(job) : []), [job]);

  if (loadError) return <div className="p-6 text-[13px] text-danger">{loadError}</div>;
  if (!job) {
    return (
      <div className="flex h-full items-center justify-center gap-2 text-muted">
        <Loader2 size={15} className="animate-spin text-accent" /> Opening job…
      </div>
    );
  }

  const byId = new Map(job.nodes.map((n) => [n.id, n]));
  const edgePath = (a: JobNode, b: JobNode) => {
    const x1 = a.x + NODE_W / 2;
    const y1 = a.y + NODE_H;
    const x2 = b.x + NODE_W / 2;
    const y2 = b.y;
    const dy = Math.max(40, Math.abs(y2 - y1) / 2);
    return `M ${x1} ${y1} C ${x1} ${y1 + dy}, ${x2} ${y2 - dy}, ${x2} ${y2 - 6}`;
  };

  return (
    <div className="flex h-full min-h-0 flex-col bg-bg">
      <div className="flex h-10 shrink-0 items-center gap-2 border-b border-line bg-panel px-3 text-[12.5px]">
        <input
          className="w-56 min-w-0 rounded border border-transparent bg-transparent px-1.5 py-0.5 font-medium hover:border-line focus:border-accent focus:outline-none"
          value={job.name}
          aria-label="Job name"
          onChange={(e) => update((j) => ({ ...j, name: e.target.value }))}
        />
        <span className="flex items-center gap-1 text-[11.5px] text-muted" title={job.schedule.enabled ? "Runs while DataBrain is open" : undefined}>
          <CalendarClock size={12} /> {scheduleText(job.schedule)}
        </span>
        <SaveState save={save} />
        <div className="ml-auto flex items-center gap-1">
          <button className="btn-ghost py-1" onClick={() => addAt(toCanvas((box.current?.getBoundingClientRect().left ?? 0) + 60, (box.current?.getBoundingClientRect().top ?? 0) + 40))}>
            <Plus size={13} /> Step
          </button>
          <button className="btn-ghost py-1" onClick={fit} title="Fit to window">
            <Maximize2 size={13} /> Fit
          </button>
          {running ? (
            <button className="btn-ghost py-1 text-danger" onClick={() => void useJobs.getState().cancel(jobId)}>
              <Square size={12} /> Stop
            </button>
          ) : (
            <button className="btn-primary py-1" onClick={() => void start()} disabled={!job.nodes.length}>
              <Play size={13} /> Run job
            </button>
          )}
        </div>
      </div>
      <div ref={area} className="relative flex min-h-0 flex-1">
        <div
          ref={box}
          tabIndex={0}
          className="relative min-w-0 flex-1 cursor-grab overflow-hidden outline-none active:cursor-grabbing"
          style={{ backgroundImage: "radial-gradient(var(--border) 1px, transparent 1px)", backgroundSize: `${20 * view.k}px ${20 * view.k}px`, backgroundPosition: `${view.x}px ${view.y}px` }}
          onPointerDown={onBackgroundDown}
          onContextMenu={(e) => {
            if ((e.target as HTMLElement).closest("[data-node]")) return;
            e.preventDefault();
            setMenu({ x: e.clientX, y: e.clientY, at: toCanvas(e.clientX, e.clientY) });
          }}
          aria-label="Job steps"
        >
          <div className="absolute left-0 top-0 origin-top-left" style={{ transform: `translate(${view.x}px, ${view.y}px) scale(${view.k})` }}>
            <svg className="pointer-events-none absolute left-0 top-0 overflow-visible" width={1} height={1}>
              <defs>
                <marker id={`arrow-${tabId}`} viewBox="0 0 10 10" refX="8" refY="5" markerWidth="7" markerHeight="7" orient="auto-start-reverse">
                  <path d="M 0 0 L 10 5 L 0 10 z" fill="var(--muted)" />
                </marker>
                <marker id={`arrow-on-${tabId}`} viewBox="0 0 10 10" refX="8" refY="5" markerWidth="7" markerHeight="7" orient="auto-start-reverse">
                  <path d="M 0 0 L 10 5 L 0 10 z" fill="var(--accent)" />
                </marker>
              </defs>
              {job.edges.map((e) => {
                const a = byId.get(e.from);
                const b = byId.get(e.to);
                if (!a || !b) return null;
                const isSel = !!sel && "edge" in sel && sel.edge.from === e.from && sel.edge.to === e.to;
                const on = isSel || (selected && (e.to === selected.id || e.from === selected.id || (related?.up.has(e.to) ?? false) || (related?.down.has(e.from) ?? false)));
                const d = edgePath(a, b);
                return (
                  <g key={`${e.from}>${e.to}`}>
                    <path
                      data-edge
                      d={d}
                      fill="none"
                      stroke="transparent"
                      strokeWidth={14}
                      className="pointer-events-auto cursor-pointer"
                      onPointerDown={(ev) => {
                        ev.stopPropagation();
                        setSel({ edge: e });
                        box.current?.focus();
                      }}
                      onContextMenu={(ev) => {
                        ev.preventDefault();
                        ev.stopPropagation();
                        update((j) => removeEdge(j, e));
                      }}
                    >
                      <title>{`${a.name} → ${b.name} · click, then Delete to remove (or right-click)`}</title>
                    </path>
                    <path d={d} fill="none" stroke={on ? "var(--accent)" : "var(--muted)"} strokeWidth={isSel ? 2.6 : on ? 2 : 1.4} markerEnd={`url(#${on ? `arrow-on-${tabId}` : `arrow-${tabId}`})`} opacity={on || !selected ? 1 : 0.45} />
                  </g>
                );
              })}
              {link && byId.get(link.from) && (
                <path d={`M ${byId.get(link.from)!.x + NODE_W / 2} ${byId.get(link.from)!.y + NODE_H} L ${link.x} ${link.y}`} stroke="var(--accent)" strokeWidth={1.6} strokeDasharray="5 4" fill="none" />
              )}
            </svg>
            {job.nodes.map((n) => (
              <NodeCard
                key={n.id}
                node={n}
                status={nodeStatus(n, run)}
                selected={selected?.id === n.id}
                dim={!!selected && selected.id !== n.id && !related?.up.has(n.id) && !related?.down.has(n.id)}
                role={selected && related?.up.has(n.id) ? "upstream" : selected && related?.down.has(n.id) ? "downstream" : undefined}
                linking={!!link && link.from !== n.id}
                onPointerDown={(e) => onNodeDown(e, n)}
                onPortDown={(e) => onPortDown(e, n)}
                onContextMenu={(e) => {
                  e.preventDefault();
                  e.stopPropagation();
                  setSel({ node: n.id });
                  setMenu({ x: e.clientX, y: e.clientY, node: n.id });
                }}
              />
            ))}
          </div>
          {job.nodes.length === 0 && (
            <div className="pointer-events-none absolute inset-0 flex items-center justify-center text-[12.5px] text-muted">Right-click to add a step</div>
          )}
          <div className="pointer-events-none absolute bottom-2 left-2 rounded-md border border-line bg-panel/90 px-2 py-1 text-[10.5px] text-muted">
            Drag a step's ● handle onto another step to run it after · right-click a step to add downstream steps · drag steps to move · ⌘+scroll to zoom
          </div>
          {warnings.length > 0 && (
            <div className="absolute right-2 top-2 max-w-sm space-y-1">
              {warnings.map((w) => (
                <button
                  key={`${w.node}:${w.reads}`}
                  className="flex w-full items-center gap-1.5 rounded-md border border-warning/40 bg-panel px-2 py-1 text-left text-[11.5px] text-warning shadow"
                  onPointerDown={(e) => e.stopPropagation()}
                  onClick={() => update((j) => addEdge(j, w.reads, w.node))}
                  title="Link them so it runs after the step it reads"
                >
                  <AlertTriangle size={12} className="shrink-0" />
                  <span className="truncate">
                    {byId.get(w.node)?.name} reads results.{byId.get(w.reads)?.name} but doesn't run after it · Link
                  </span>
                </button>
              ))}
            </div>
          )}
        </div>
        <SplitHandle
          side="right"
          width={panelW}
          min={300}
          max={() => Math.max(300, (area.current?.getBoundingClientRect().width ?? 1200) - 260)}
          onChange={setPanelW}
          onCommit={(w) => localStorage.setItem(PANEL_KEY, String(w))}
          onReset={() => {
            setPanelW(PANEL_DEFAULT);
            localStorage.removeItem(PANEL_KEY);
          }}
          label="Resize step panel"
        />
        <aside className="shrink-0 overflow-y-auto border-l border-line bg-panel text-[12px]" style={{ width: panelW }} aria-label="Step details">
          {selected ? (
            <NodePanel
              key={selected.id}
              job={job}
              taken={taken}
              node={selected}
              status={nodeStatus(selected, run)}
              run={run}
              running={running}
              onChange={(patch) => updateNode(selected.id, patch)}
              onRun={() => void start([selected.id])}
              onSelect={(id) => setSel({ node: id })}
              onDelete={() => deleteNode(selected.id)}
            />
          ) : (
            <JobPanel job={job} run={run} onChange={(patch) => update((j) => ({ ...j, ...patch }))} onSelect={(id) => setSel({ node: id })} />
          )}
        </aside>
      </div>
      {menu && (
        <Popover x={menu.x} y={menu.y} onClose={() => setMenu(null)}>
          {menu.node ? (
            <>
              <MenuItem icon={<Play size={13} />} label="Run this step" disabled={running} onClick={() => (setMenu(null), void start([menu.node!]))} />
              <MenuItem
                icon={<PlayCircle size={13} />}
                label="Run from here"
                hint="this step and everything downstream"
                disabled={running}
                onClick={() => (setMenu(null), void start([menu.node!, ...descendants(job, menu.node!)]))}
              />
              <MenuSeparator />
              <div className="px-2.5 pb-0.5 pt-1 text-[10.5px] uppercase tracking-wide text-muted">Add a step after this one that…</div>
              <MenuItem icon={<Database size={13} />} label="Queries this output" hint="DuckDB" onClick={() => (setMenu(null), downstream(menu.node!, "query"))} />
              <MenuItem icon={<ArrowDownToLine size={13} />} label="Loads it into a connection" onClick={() => (setMenu(null), downstream(menu.node!, "load"))} />
              <MenuItem icon={<FileDown size={13} />} label="Saves it to a file" hint="CSV, Parquet, JSON" onClick={() => (setMenu(null), downstream(menu.node!, "export"))} />
              <OutputMenuItem jobId={jobId} node={byId.get(menu.node)!} onDone={() => setMenu(null)} />
              <MenuSeparator />
              <MenuItem icon={<Copy size={13} />} label="Duplicate" onClick={() => (setMenu(null), duplicate(menu.node!))} />
              <MenuItem icon={<Trash2 size={13} />} label="Delete step" danger onClick={() => (setMenu(null), deleteNode(menu.node!))} />
            </>
          ) : (
            <>
              <MenuItem icon={<Plus size={13} />} label="Add step here" onClick={() => (setMenu(null), addAt(menu.at!))} />
              <MenuItem icon={<Database size={13} />} label="Add DuckDB step here" hint="reads outputs of other steps" onClick={() => (setMenu(null), addAt(menu.at!, { connection_id: null }))} />
              <QueryTabItems
                onPick={(t) => {
                  setMenu(null);
                  addAt(menu.at!, { name: t.title.toLowerCase(), sql: t.sql, connection_id: t.connection_id ?? null });
                }}
              />
            </>
          )}
        </Popover>
      )}
    </div>
  );
}

/**
 * Output names used outside this job (lowercase → who uses it): other jobs'
 * steps and named outputs of query tabs and notebooks.
 */
function useTakenNames(jobId: string): Map<string, string> {
  const jobs = useJobs((s) => s.jobs);
  const outputs = useStore((s) => s.outputs);
  const tabs = useStore((s) => s.tabs);
  return useMemo(() => {
    const m = new Map<string, string>();
    for (const o of outputs) {
      if (!o.name || o.tab_id.startsWith("job:")) continue;
      const tab = tabs.find((t) => t.id === o.tab_id);
      m.set(o.name.toLowerCase(), o.tab_id.startsWith("nb:") ? `output ${o.handle} of a notebook cell` : `output ${o.handle}${tab ? ` of "${tab.title}"` : ""}`);
    }
    for (const j of jobs) if (j.id !== jobId) for (const n of j.step_names ?? []) m.set(n.toLowerCase(), `step "${n}" of job "${j.name}"`);
    return m;
  }, [jobs, outputs, tabs, jobId]);
}

function SaveState({ save }: { save: { state: string; message?: string } }) {
  if (save.state === "error")
    return (
      <span className="flex min-w-0 items-center gap-1 truncate text-[11.5px] text-danger" title={save.message}>
        <AlertTriangle size={12} className="shrink-0" /> Not saved: {save.message}
      </span>
    );
  return <span className="text-[11px] text-muted">{save.state === "saved" ? "Saved" : "Saving…"}</span>;
}

/** Open query tabs, to start a step from one (its connection and SQL). */
function QueryTabItems({ onPick }: { onPick: (t: { title: string; sql: string; connection_id?: string | null }) => void }) {
  // Select the stable array and filter outside the selector (a new array per call loops in zustand 5).
  const all = useStore((s) => s.tabs);
  const tabs = useMemo(() => all.filter(isQueryTab).filter((t) => t.sql.trim()), [all]);
  if (!tabs.length) return null;
  return (
    <>
      <MenuSeparator />
      <div className="px-3 pb-0.5 pt-1 text-[10.5px] uppercase tracking-wide text-muted">From a query tab</div>
      {tabs.slice(0, 12).map((t) => (
        <MenuItem key={t.id} label={t.title} hint={t.sql.trim().split("\n")[0].slice(0, 40)} onClick={() => onPick(t)} />
      ))}
    </>
  );
}

function OutputMenuItem({ jobId, node, onDone }: { jobId: string; node: JobNode; onDone: () => void }) {
  const o = useStore((s) => s.outputs.find((x) => x.tab_id === EDITOR_KEY(jobId, node.id) && x.active && x.state !== "evicted"));
  return <MenuItem icon={<Eye size={13} />} label="View output" hint={o ? `results.${node.name}` : "run the step first"} disabled={!o} onClick={() => (onDone(), o && void openOutput(o))} />;
}

function StatusIcon({ status }: { status?: NodeRunSummary["status"] }) {
  switch (status) {
    case "running":
      return <Loader2 size={12} className="shrink-0 animate-spin text-accent" />;
    case "success":
      return <CheckCircle2 size={12} className="shrink-0 text-success" />;
    case "error":
      return <XCircle size={12} className="shrink-0 text-danger" />;
    case "skipped":
    case "cancelled":
    case "pending":
      return <CircleDashed size={12} className="shrink-0 text-muted" />;
    default:
      return null;
  }
}

function statusText(s: NodeRunSummary | null): string {
  if (!s || !s.status) return "Not run yet";
  switch (s.status) {
    case "running":
      return s.progress ? `${s.progress.phase}: ${formatCount(s.progress.done)} / ${formatCount(s.progress.total)}` : "Running…";
    case "pending":
      return "Waiting for upstream steps";
    case "skipped":
      return "Skipped: an upstream step failed";
    case "cancelled":
      return "Cancelled";
    case "error":
      return s.error ?? "Failed";
    default:
      return `${s.rows != null ? `${formatCount(s.rows)} rows · ` : ""}${s.file ? `${s.file.split(/[/\\]/).pop()} · ` : ""}${formatDuration(s.duration_ms)}${s.finished_at ? ` · ${relativeTime(s.finished_at)}` : ""}`;
  }
}

function NodeCard({
  node,
  status,
  selected,
  dim,
  role,
  linking,
  onPointerDown,
  onPortDown,
  onContextMenu,
}: {
  node: JobNode;
  status: NodeRunSummary | null;
  selected: boolean;
  dim: boolean;
  role?: "upstream" | "downstream";
  linking: boolean;
  onPointerDown: (e: ReactPointerEvent) => void;
  onPortDown: (e: ReactPointerEvent) => void;
  onContextMenu: (e: React.MouseEvent) => void;
}) {
  const conn = useStore((s) => s.connections.find((c) => c.id === node.connection_id));
  const target = useStore((s) => s.connections.find((c) => c.id === node.target_connection_id));
  const tone =
    status?.status === "error" ? "border-danger/70" : status?.status === "running" ? "border-accent" : status?.status === "success" ? "border-success/60" : "border-line";
  return (
    <div
      data-node={node.id}
      className={`absolute flex cursor-pointer select-none flex-col rounded-lg border bg-panel px-2.5 py-1.5 shadow-sm transition-opacity ${tone} ${
        selected ? "ring-2 ring-accent" : linking ? "hover:ring-2 hover:ring-accent/60" : ""
      } ${dim ? "opacity-50" : ""}`}
      style={{ left: node.x, top: node.y, width: NODE_W, height: NODE_H }}
      onPointerDown={onPointerDown}
      onContextMenu={onContextMenu}
    >
      <span className="absolute -top-[5px] left-1/2 h-2.5 w-2.5 -translate-x-1/2 rounded-full border border-line bg-panel-2" aria-hidden />
      <div className="flex items-center gap-1.5">
        {node.kind === "load" ? (
          <ArrowDownToLine size={12} className="shrink-0 text-accent" />
        ) : node.kind === "export" ? (
          <FileDown size={12} className="shrink-0 text-accent" />
        ) : (
          <Database size={12} className="shrink-0 text-muted" />
        )}
        <span className="min-w-0 flex-1 truncate font-mono text-[12px] font-semibold">{node.name}</span>
        {role && <span className="shrink-0 rounded bg-accent/10 px-1 text-[9.5px] uppercase text-accent">{role}</span>}
      </div>
      <div className="mt-0.5 flex min-w-0 items-center gap-1 text-[10.5px] text-muted">
        {node.kind === "export" ? (
          <span className="truncate" title={node.export_folder || "Downloads"}>
            DuckDB → {node.export_file?.trim() || `${node.name}_…`}
            {node.export_file?.toLowerCase().endsWith(`.${node.export_format ?? "csv"}`) ? "" : `.${node.export_format ?? "csv"}`} in {folderName(node.export_folder)}
          </span>
        ) : node.kind === "load" ? (
          <span className="truncate">
            DuckDB → {target?.name ?? "choose a connection"}
            {node.target_table ? `.${node.target_table}` : ""}
          </span>
        ) : conn && !isResults(conn) ? (
          <>
            <ConnDot color={conn.color} />
            <span className="truncate">{conn.name}</span>
          </>
        ) : (
          <span className="truncate">Results (DuckDB)</span>
        )}
      </div>
      {status?.status === "running" && status.progress && (
        <div className="mt-1">
          <ProgressBar p={status.progress} compact />
        </div>
      )}
      <div className={`mt-auto flex min-w-0 items-center gap-1 text-[10.5px] ${status?.status === "error" ? "text-danger" : "text-muted"}`} title={status?.error ?? undefined}>
        <StatusIcon status={status?.status} />
        <span className="truncate">{node.sql.trim() || node.kind !== "query" ? statusText(status) : "No SQL yet"}</span>
      </div>
      <span
        data-port
        role="button"
        aria-label={`Link ${node.name} to a downstream step`}
        title="Drag onto a step to run it after this one (or onto empty space to add one)"
        className="absolute -bottom-[7px] left-1/2 h-3.5 w-3.5 -translate-x-1/2 cursor-crosshair rounded-full border-2 border-accent bg-panel hover:scale-125"
        onPointerDown={onPortDown}
      />
    </div>
  );
}

const field = "w-full rounded-md border border-line bg-bg px-2 py-1 text-[12px] focus:border-accent focus:outline-none";
const label = "mb-1 block text-[11px] font-medium uppercase tracking-wide text-muted";

function NodePanel({
  job,
  node,
  status,
  run,
  running,
  onChange,
  onRun,
  onSelect,
  onDelete,
  taken,
}: {
  job: Job;
  taken: Map<string, string>;
  node: JobNode;
  status: NodeRunSummary | null;
  /** The job's current or last run in this session. */
  run?: JobRun;
  running: boolean;
  onChange: (patch: Partial<JobNode>) => void;
  onRun: () => void;
  onSelect: (id: string) => void;
  onDelete: () => void;
}) {
  const connections = useStore((s) => s.connections);
  const allTabs = useStore((s) => s.tabs);
  const queryTabs = useMemo(() => allTabs.filter(isQueryTab), [allTabs]);
  const output = useStore((s) => s.outputs.find((x) => x.tab_id === EDITOR_KEY(job.id, node.id) && x.active && x.state !== "evicted"));
  const [name, setName] = useState(node.name);
  const ups = upstreamOf(job, node.id).map((id) => job.nodes.find((n) => n.id === id)!).filter(Boolean);
  const downs = job.edges.filter((e) => e.from === node.id).map((e) => job.nodes.find((n) => n.id === e.to)!).filter(Boolean);
  // Known at once: format, this job's steps, other jobs' steps and named outputs.
  const localProblem =
    nameFormatProblem(name) ??
    (job.nodes.some((n) => n.id !== node.id && n.name.toLowerCase() === name.toLowerCase())
      ? "Another step of this job has this name"
      : taken.has(name.toLowerCase())
        ? `results.${name} is already used by ${taken.get(name.toLowerCase())}`
        : null);
  // Then the app (notebook cells' output names too), shortly after typing.
  const [remote, setRemote] = useState<{ name: string; user: string | null } | null>(null);
  useEffect(() => {
    if (localProblem || name === node.name) return;
    let live = true;
    const t = setTimeout(() => {
      api
        .outputNameUser(name, job.id)
        .then((user) => {
          if (!live) return;
          setRemote({ name, user });
          if (!user) onChange({ name });
        })
        .catch(() => {});
    }, 250);
    return () => {
      live = false;
      clearTimeout(t);
    };
  }, [name, localProblem]); // eslint-disable-line react-hooks/exhaustive-deps
  const nameProblem = localProblem ?? (remote?.name === name && remote.user ? `results.${name} is already used by ${remote.user}` : null);
  const results = connections.find(isResults);
  const runsOn = node.kind !== "query" || !node.connection_id ? results : connections.find((c) => c.id === node.connection_id);
  const others = connections.filter((c) => !isResults(c));
  const mentions = (u: JobNode) => new RegExp(`\\bresults\\s*\\.\\s*"?${u.name}"?\\b`, "i").test(node.sql);

  return (
    <div className="space-y-3 p-3">
      <div className="flex items-center gap-2">
        <div className="min-w-0 flex-1">
          <label className={label} htmlFor={`name-${node.id}`}>
            Step (output name)
          </label>
          <input
            id={`name-${node.id}`}
            className={`${field} font-mono ${nameProblem ? "border-danger" : ""}`}
            value={name}
            onChange={(e) => setName(e.target.value)}
            onBlur={() => nameProblem && setName(node.name)}
          />
          {nameProblem ? (
            <div className="mt-0.5 text-[11px] text-danger">{nameProblem}. Kept: {node.name}</div>
          ) : (
            <div className="mt-0.5 text-[11px] text-muted">Unique across the app: only this step produces results.{name || node.name}</div>
          )}
        </div>
        <button className="btn-primary mt-4 py-1" onClick={onRun} disabled={running} title="Run only this step (upstream outputs must exist)">
          <Play size={12} /> Run step
        </button>
        <button className="icon-btn mt-4 h-7 w-7" aria-label="Delete step" title="Delete step" onClick={onDelete}>
          <Trash2 size={13} />
        </button>
      </div>

      <ActionPicker node={node} upstreams={ups.map((u) => u.name)} onChange={onChange} />

      {node.kind === "query" && (
        <div className="grid grid-cols-2 gap-2">
          <div>
            <label className={label} htmlFor={`conn-${node.id}`}>
              Connection
            </label>
            <select id={`conn-${node.id}`} className={field} value={runsOn && runsOn !== results ? runsOn.id : ""} onChange={(e) => onChange({ connection_id: e.target.value || null })}>
              <option value="">Results (DuckDB) · reads other steps</option>
              {others.map((c) => (
                <option key={c.id} value={c.id}>
                  {c.name}
                </option>
              ))}
            </select>
          </div>
          <div>
            <label className={label} htmlFor={`from-${node.id}`}>
              Copy from a query tab
            </label>
            <select
              id={`from-${node.id}`}
              className={field}
              value=""
              disabled={!queryTabs.length}
              onChange={(e) => {
                const t = queryTabs.find((x) => x.id === e.target.value);
                if (!t) return;
                const apply = () => onChange({ sql: t.sql, connection_id: t.connection_id ?? null });
                if (node.sql.trim() && node.sql !== t.sql)
                  useStore.getState().askConfirm({ title: `Replace the SQL of "${node.name}"?`, reasons: [`It gets the SQL and connection of "${t.title}".`], confirmLabel: "Replace", onConfirm: apply });
                else apply();
              }}
            >
              <option value="">{queryTabs.length ? "Choose a tab…" : "No query tabs open"}</option>
              {queryTabs.map((t) => (
                <option key={t.id} value={t.id}>
                  {t.title}
                </option>
              ))}
            </select>
          </div>
        </div>
      )}

      {node.kind === "query" ? (
        <>
        {ups.length > 0 && (
          <div className="text-[11.5px]">
            <span className="text-muted">{runsOn === results ? "Reads (click to insert): " : "Runs after: "}</span>
            {ups.map((u, i) => (
              <span key={u.id}>
                {i > 0 && ", "}
                <button
                  className={`font-mono hover:underline ${runsOn === results && !mentions(u) ? "text-warning" : "text-accent"}`}
                  title={runsOn === results ? (mentions(u) ? `Uses results.${u.name}` : `Doesn't use results.${u.name} yet: click to add it`) : `Select ${u.name}`}
                  onClick={() => (runsOn === results && !mentions(u) ? onChange({ sql: node.sql.trim() ? `${node.sql.trimEnd()}\n-- results.${u.name}` : readSql([u.name]) }) : onSelect(u.id))}
                >
                  results.{u.name}
                </button>
              </span>
            ))}
            {runsOn !== results && <div className="mt-0.5 text-[11px] text-muted">Outputs of other steps can only be read by DuckDB steps; this step only waits for them.</div>}
          </div>
        )}

        <div>
          <label className={label}>
            SQL {runsOn ? `on ${runsOn === results ? "Results (DuckDB)" : runsOn.name}` : ""}
          </label>
          <CellEditor
            editorKey={EDITOR_KEY(job.id, node.id)}
            source={node.sql}
            connectionId={node.kind !== "query" ? (results?.id ?? null) : (node.connection_id ?? results?.id ?? null)}
            height={240}
            onResize={() => {}}
            onChange={(sql) => onChange({ sql })}
            onRun={onRun}
            onRunAdvance={onRun}
            onFocus={() => {}}
          />
        </div>
        </>
      ) : (
        <RowsSection job={job} node={node} ups={ups} onChange={onChange} onSelect={onSelect} />
      )}

      {node.kind === "export" && <ExportFields job={job} node={node} onChange={onChange} />}

      {node.kind === "load" && <LoadFields job={job} node={node} onChange={onChange} />}

      <div className={`rounded-md border px-2 py-1.5 ${status?.status === "error" ? "border-danger/50 bg-danger/5" : "border-line"}`}>
        <div className="flex items-center gap-1.5">
          <StatusIcon status={status?.status} />
          <span className={`min-w-0 flex-1 whitespace-pre-wrap break-words ${status?.status === "error" ? "text-danger" : "text-muted"}`}>{statusText(status)}</span>
          {status?.status === "success" && status.file && (
            <button
              className="btn-ghost shrink-0 py-0.5 text-[11.5px]"
              title={status.file}
              onClick={() => api.revealJobFile(job.id, node.id).catch((e) => useStore.getState().toast(toError(e).message, "error"))}
            >
              <FolderOpen size={12} /> Show file
            </button>
          )}
          {output && (
            <button className="btn-ghost shrink-0 py-0.5 text-[11.5px]" onClick={() => void openOutput(output)}>
              <Eye size={12} /> View output
            </button>
          )}
        </div>
        {status?.status === "running" && status.progress && (
          <div className="mt-1.5">
            <ProgressBar p={status.progress} />
          </div>
        )}
        {status?.notices?.map((n, i) => (
          <div key={i} className="mt-1 flex items-start gap-1 text-[11.5px] text-accent">
            <Info size={12} className="mt-0.5 shrink-0" /> <span className="min-w-0 break-words">{n}</span>
          </div>
        ))}
      </div>

      {run?.nodes.some((r) => r.node_id === node.id) && (
        <details open={status?.status === "running" || status?.status === "error"} className="rounded-md border border-line px-2 py-1.5">
          <summary className="cursor-pointer text-[11px] font-medium uppercase tracking-wide text-muted">Log of this step ({run.status === "running" ? "running now" : relativeTime(run.started_at)})</summary>
          <div className="mt-1.5">
            <RunLog job={job} run={run} step={node.id} />
          </div>
        </details>
      )}

      {(ups.length > 0 || downs.length > 0) && (
        <div className="grid grid-cols-2 gap-2 text-[11.5px]">
          <NodeList title="Upstream" nodes={ups} onSelect={onSelect} empty="none: runs first" />
          <NodeList title="Downstream" nodes={downs} onSelect={onSelect} empty="none" />
        </div>
      )}
    </div>
  );
}

/** Rows written so far by a running load step. */
function ProgressBar({ p, compact = false }: { p: StepProgress; compact?: boolean }) {
  const pct = p.total > 0 ? Math.min(100, (p.done / p.total) * 100) : 100;
  return (
    <div className={compact ? "" : "space-y-0.5"}>
      <div
        className={`w-full overflow-hidden rounded-full bg-line ${compact ? "h-1" : "h-1.5"}`}
        role="progressbar"
        aria-label={p.phase}
        aria-valuemin={0}
        aria-valuemax={p.total}
        aria-valuenow={p.done}
      >
        <div className="h-full rounded-full bg-accent transition-[width] duration-200" style={{ width: `${pct}%` }} />
      </div>
      {!compact && (
        <div className="flex justify-between text-[11px] text-muted">
          <span>{p.phase}</span>
          <span>
            {formatCount(p.done)} / {formatCount(p.total)} rows · {Math.floor(pct)}%
          </span>
        </div>
      )}
    </div>
  );
}

/** Log of a run: live from events while it runs, else read from the app. */
function useRunLog(jobId: string, run: JobRun | undefined): { entries: RunLogEntry[]; loading: boolean } {
  const live = useJobs((s) => (run ? s.logs[run.id] : undefined));
  const [saved, setSaved] = useState<{ id: number; entries: RunLogEntry[] } | null>(null);
  const running = run?.status === "running";
  useEffect(() => {
    if (!run || running) return;
    let ok = true;
    api
      .jobRunLog(jobId, run.id)
      .then((entries) => ok && setSaved({ id: run.id, entries }))
      .catch(() => ok && setSaved({ id: run.id, entries: [] }));
    return () => {
      ok = false;
    };
  }, [jobId, run?.id, running]); // eslint-disable-line react-hooks/exhaustive-deps
  if (!run) return { entries: [], loading: false };
  if (saved?.id === run.id && !running) return { entries: saved.entries, loading: false };
  return { entries: live ?? [], loading: !running && !live };
}

const LEVEL_TONE: Record<RunLogEntry["level"], string> = { info: "text-muted", success: "text-success", warning: "text-warning", error: "text-danger" };

function logText(entries: RunLogEntry[]): string {
  return entries
    .map((e) => {
      const head = `${new Date(e.at).toLocaleTimeString()} ${e.level.toUpperCase().padEnd(7)} ${e.step ? `[${e.step}] ` : ""}${e.message}${e.duration_ms != null ? ` (${formatDuration(e.duration_ms)})` : ""}`;
      return e.sql ? `${head}\n    ${e.sql.replace(/\n/g, "\n    ")}` : head;
    })
    .join("\n");
}

/** A run's log: what ran on which step, SQL, rows, errors. */
function RunLog({ job, run, step, onSelect }: { job: Job; run: JobRun | undefined; step?: string; onSelect?: (id: string) => void }) {
  const { entries, loading } = useRunLog(job.id, run);
  const [filter, setFilter] = useState<string>(step ?? "");
  const [onlyProblems, setOnlyProblems] = useState(false);
  const [openSql, setOpenSql] = useState<Set<number>>(new Set());
  const end = useRef<HTMLDivElement>(null);
  const follow = useRef(true);
  useEffect(() => setFilter(step ?? ""), [step]);
  const shown = entries.filter((e) => (!filter || e.node_id === filter || (!e.node_id && !step)) && (!onlyProblems || e.level === "error" || e.level === "warning"));
  useEffect(() => {
    if (run?.status === "running" && follow.current) end.current?.scrollIntoView({ block: "nearest" });
  }, [shown.length, run?.status]);
  if (!run) return <div className="text-[11.5px] text-muted">No run yet.</div>;
  const steps = job.nodes.filter((n) => entries.some((e) => e.node_id === n.id));
  return (
    <div className="space-y-1">
      <div className="flex items-center gap-1.5">
        {!step && (
          <select className="rounded-md border border-line bg-bg px-1.5 py-0.5 text-[11.5px]" value={filter} onChange={(e) => setFilter(e.target.value)} aria-label="Show the log of">
            <option value="">All steps</option>
            {steps.map((n) => (
              <option key={n.id} value={n.id}>
                {n.name}
              </option>
            ))}
          </select>
        )}
        <label className="flex items-center gap-1 text-[11.5px]">
          <input type="checkbox" checked={onlyProblems} onChange={(e) => setOnlyProblems(e.target.checked)} /> Errors and warnings only
        </label>
        <button
          className="btn-ghost ml-auto py-0 text-[11px]"
          disabled={!shown.length}
          onClick={() => void navigator.clipboard.writeText(logText(shown)).then(() => useStore.getState().toast("Log copied", "info"))}
        >
          <Copy size={11} /> Copy
        </button>
      </div>
      <div
        className="max-h-80 overflow-y-auto rounded-md border border-line bg-bg px-1.5 py-1 font-mono text-[11px] leading-[1.45]"
        role="log"
        aria-live={run.status === "running" ? "polite" : "off"}
        onScroll={(e) => {
          const el = e.currentTarget;
          follow.current = el.scrollHeight - el.scrollTop - el.clientHeight < 24;
        }}
      >
        {loading && <div className="text-muted">Loading…</div>}
        {!loading && !shown.length && <div className="text-muted">{entries.length ? "Nothing matches." : run.status === "running" ? "Starting…" : "No log for this run (runs before logs were kept have none)."}</div>}
        {shown.map((e) => (
          <div key={e.seq} className="py-px">
            <div className="flex items-baseline gap-1.5">
              <span className="shrink-0 text-muted">{new Date(e.at).toLocaleTimeString()}</span>
              {e.step && !step && (
                <button className="shrink-0 text-accent hover:underline" onClick={() => e.node_id && onSelect?.(e.node_id)}>
                  {e.step}
                </button>
              )}
              <span className={`min-w-0 flex-1 whitespace-pre-wrap break-words ${LEVEL_TONE[e.level] === "text-muted" ? "text-fg" : LEVEL_TONE[e.level]}`}>{e.message}</span>
              {e.duration_ms != null && <span className="shrink-0 text-muted">{formatDuration(e.duration_ms)}</span>}
              {e.sql && (
                <button
                  className="shrink-0 text-muted hover:text-fg"
                  aria-expanded={openSql.has(e.seq)}
                  onClick={() =>
                    setOpenSql((s) => {
                      const n = new Set(s);
                      if (n.has(e.seq)) n.delete(e.seq);
                      else n.add(e.seq);
                      return n;
                    })
                  }
                >
                  SQL
                </button>
              )}
            </div>
            {e.sql && openSql.has(e.seq) && <pre className="ml-14 mt-0.5 max-h-40 overflow-auto whitespace-pre-wrap break-all rounded bg-panel-2 px-1.5 py-1 text-[10.5px]">{e.sql}</pre>}
          </div>
        ))}
        <div ref={end} />
      </div>
    </div>
  );
}

function actionIcon(kind: JobNode["kind"], size = 14) {
  return kind === "load" ? <ArrowDownToLine size={size} className="shrink-0 text-accent" /> : kind === "export" ? <FileDown size={size} className="shrink-0 text-accent" /> : <Database size={size} className="shrink-0 text-accent" />;
}

/** The one thing this step does, and a way to change it (clearing the old action's settings). */
function ActionPicker({ node, upstreams, onChange }: { node: JobNode; upstreams: string[]; onChange: (patch: Partial<JobNode>) => void }) {
  const cur = STEP_ACTIONS.find((a) => a.kind === node.kind) ?? STEP_ACTIONS[0];
  const pick = (kind: JobNode["kind"]) => {
    const next = STEP_ACTIONS.find((a) => a.kind === kind)!;
    const apply = () => onChange(changeAction(node, kind, upstreams));
    const lost = actionSettings(node);
    if (!lost.length) return apply();
    useStore.getState().askConfirm({
      title: `Change "${node.name}" to "${next.title}"?`,
      reasons: [`A step does one thing. ${lost.join(", ").replace(/^./, (c) => c.toUpperCase())} will be removed.`, "To keep this action too, add another step after this one instead."],
      confirmLabel: "Change action",
      onConfirm: apply,
    });
  };
  return (
    <section aria-label="Action" className="rounded-md border border-accent/40 bg-accent/5 p-2">
      <div className={label}>Action</div>
      <div className="flex items-start gap-2">
        <span className="mt-0.5">{actionIcon(node.kind)}</span>
        <div className="min-w-0 flex-1">
          <div className="text-[12.5px] font-semibold">{cur.title}</div>
          <div className="text-[11px] text-muted">{cur.hint}</div>
        </div>
        <select aria-label="Change action" className="w-auto shrink-0 rounded-md border border-line bg-bg px-1.5 py-0.5 text-[11.5px]" value="" onChange={(e) => e.target.value && pick(e.target.value as JobNode["kind"])}>
          <option value="">Change…</option>
          {STEP_ACTIONS.filter((a) => a.kind !== node.kind).map((a) => (
            <option key={a.kind} value={a.kind}>
              {a.title}
            </option>
          ))}
        </select>
      </div>
    </section>
  );
}

/** Rows a load/export step writes: all rows of its upstream step, or a DuckDB query. */
function RowsSection({ job, node, ups, onChange, onSelect }: { job: Job; node: JobNode; ups: JobNode[]; onChange: (patch: Partial<JobNode>) => void; onSelect: (id: string) => void }) {
  const names = ups.map((u) => u.name);
  const results = useStore((s) => s.connections.find(isResults));
  const plain = usesDefaultRows(node, names);
  const [editing, setEditing] = useState(!plain);
  const what = node.kind === "load" ? "load" : "save";
  return (
    <section aria-label="Rows" className="space-y-1.5">
      <div className={label}>Rows to {what}</div>
      {!ups.length && !node.sql.trim() && (
        <div className="flex items-start gap-1 text-[11.5px] text-warning">
          <AlertTriangle size={12} className="mt-0.5 shrink-0" /> Link an upstream step (drag its ● handle onto this one), or write SQL that selects the rows.
        </div>
      )}
      {!editing && ups.length > 0 ? (
        <div className="flex items-center gap-2 rounded-md border border-line px-2 py-1.5 text-[12px]">
          <span className="min-w-0 flex-1">
            All rows of{" "}
            <button className="font-mono text-accent hover:underline" onClick={() => onSelect(ups[0].id)}>
              results.{ups[0].name}
            </button>
            {ups.length > 1 && <span className="text-muted"> (the first upstream step; use SQL to combine several)</span>}
          </span>
          <button className="btn-ghost shrink-0 py-0.5 text-[11.5px]" onClick={() => (setEditing(true), onChange({ sql: node.sql.trim() ? node.sql : readSql(names) }))}>
            Filter or reshape with SQL…
          </button>
        </div>
      ) : (
        <div>
          <div className="mb-1 flex items-center gap-2 text-[11px] text-muted">
            <span className="flex-1">DuckDB query; upstream outputs are results.&lt;step&gt;</span>
            {ups.length > 0 && (
              <button className="btn-ghost py-0 text-[11px]" onClick={() => (setEditing(false), onChange({ sql: "" }))}>
                Use all rows of results.{ups[0].name}
              </button>
            )}
          </div>
          <CellEditor
            editorKey={EDITOR_KEY(job.id, node.id)}
            source={node.sql}
            connectionId={results?.id ?? null}
            height={160}
            onResize={() => {}}
            onChange={(sql) => onChange({ sql })}
            onRun={() => {}}
            onRunAdvance={() => {}}
            onFocus={() => {}}
          />
        </div>
      )}
    </section>
  );
}

/** Rows and KB per INSERT, within what the target connection allows. */
function BatchFields({ node, kind, onChange }: { node: JobNode; kind?: string; onChange: (patch: Partial<JobNode>) => void }) {
  const [limits, setLimits] = useState<BatchLimits | null>(null);
  useEffect(() => {
    if (!kind) return setLimits(null);
    let ok = true;
    api
      .loadBatchLimits(kind)
      .then((l) => ok && setLimits(l))
      .catch(() => ok && setLimits(null));
    return () => {
      ok = false;
    };
  }, [kind]);
  const [rows, setRows] = useState(node.batch_rows ? String(node.batch_rows) : "");
  const [kb, setKb] = useState(node.batch_kb ? String(node.batch_kb) : "");
  useEffect(() => {
    setRows(node.batch_rows ? String(node.batch_rows) : "");
    setKb(node.batch_kb ? String(node.batch_kb) : "");
  }, [node.id, node.batch_rows, node.batch_kb]);
  // A saved value past a new target's maximum is lowered at once.
  useEffect(() => {
    if (!limits) return;
    const patch: Partial<JobNode> = {};
    if (node.batch_rows && node.batch_rows > limits.max_rows) patch.batch_rows = limits.max_rows;
    if (node.batch_kb && node.batch_kb * 1024 > limits.max_bytes) patch.batch_kb = Math.floor(limits.max_bytes / 1024);
    if (Object.keys(patch).length) onChange(patch);
  }, [limits]); // eslint-disable-line react-hooks/exhaustive-deps
  const maxKb = limits ? Math.floor(limits.max_bytes / 1024) : 65536;
  const over = (text: string, max: number) => !!text.trim() && Number(text) > max;
  return (
    <div>
      <div className="grid grid-cols-2 gap-2">
        <div>
          <label className={label} htmlFor={`batch-rows-${node.id}`}>
            Rows per INSERT
          </label>
          <input
            id={`batch-rows-${node.id}`}
            type="number"
            min={1}
            max={limits?.max_rows}
            step={1}
            inputMode="numeric"
            className={`${field} ${limits && over(rows, limits.max_rows) ? "border-warning" : ""}`}
            placeholder={limits ? `${formatCount(limits.default_rows)} (default)` : "default"}
            value={rows}
            onChange={(e) => setRows(e.target.value)}
            onBlur={() => {
              const v = batchValue(rows, limits?.max_rows ?? Number.MAX_SAFE_INTEGER);
              setRows(v ? String(v) : "");
              onChange({ batch_rows: v });
            }}
            aria-describedby={`batch-help-${node.id}`}
          />
        </div>
        <div>
          <label className={label} htmlFor={`batch-kb-${node.id}`}>
            Max KB per INSERT
          </label>
          <input
            id={`batch-kb-${node.id}`}
            type="number"
            min={1}
            max={maxKb}
            step={1}
            inputMode="numeric"
            className={`${field} ${limits && over(kb, maxKb) ? "border-warning" : ""}`}
            placeholder={limits ? `${formatCount(Math.floor(limits.default_bytes / 1024))} (default)` : "default"}
            value={kb}
            onChange={(e) => setKb(e.target.value)}
            onBlur={() => {
              const v = batchValue(kb, maxKb);
              setKb(v ? String(v) : "");
              onChange({ batch_kb: v });
            }}
            aria-describedby={`batch-help-${node.id}`}
          />
        </div>
      </div>
      <div id={`batch-help-${node.id}`} className="mt-0.5 space-y-0.5 text-[11px] text-muted">
        <div>A statement ends at whichever comes first: the rows or the size of its SQL text. Lower them when the database rejects large statements.</div>
        {limits ? (
          <>
            <div>
              At most {formatCount(limits.max_rows)} rows: {limits.rows_note}.
            </div>
            <div>
              At most {formatCount(maxKb)} KB: {limits.bytes_note}.
            </div>
          </>
        ) : (
          <div>Choose the connection to see its limits.</div>
        )}
      </div>
    </div>
  );
}

const LOAD_MODES: { mode: LoadMode; text: string; hint: string }[] = [
  { mode: "append", text: "Insert", hint: "Insert the rows. A missing table can be created from the rows' columns." },
  { mode: "truncate", text: "Delete rows, then insert", hint: "Delete every row of the table, then insert. A missing table can be created." },
  { mode: "replace", text: "Replace table", hint: "Drop the table if it exists and create it again from the rows' columns, then insert." },
  { mode: "update", text: "Update by key", hint: "The table must exist. Rows whose key columns match are updated; the others are ignored." },
  { mode: "merge", text: "Merge (upsert) by key", hint: "The table must exist. Rows whose key columns match are updated, the others inserted." },
];

/** Target, mode, key columns, before/after SQL and dry run of a load step. */
function LoadFields({ job, node, onChange }: { job: Job; node: JobNode; onChange: (patch: Partial<JobNode>) => void }) {
  const connections = useStore((s) => s.connections);
  const target = connections.find((c) => c.id === node.target_connection_id);
  const mode = LOAD_MODES.find((m) => m.mode === node.load_mode) ?? LOAD_MODES[0];
  const keyed = node.load_mode === "update" || node.load_mode === "merge";
  const [keys, setKeys] = useState((node.key_columns ?? []).join(", "));
  useEffect(() => setKeys((node.key_columns ?? []).join(", ")), [node.id]); // eslint-disable-line react-hooks/exhaustive-deps
  const [dry, setDry] = useState<{ busy: boolean; report?: LoadDryRun; error?: string }>({ busy: false });
  // A report is about the settings it was made with.
  useEffect(() => setDry({ busy: false }), [node.id]);
  const [showBefore, setShowBefore] = useState(!!node.load_before_sql?.trim());
  const [showAfter, setShowAfter] = useState(!!node.load_after_sql?.trim());
  const runDry = async () => {
    setDry({ busy: true });
    try {
      setDry({ busy: false, report: await api.dryRunJobStep(job, node.id) });
    } catch (e) {
      setDry({ busy: false, error: toError(e).message });
    }
  };
  const sqlBox = (which: "before" | "after", value: string | null | undefined) => (
    <div>
      <label className={label}>
        {which === "before" ? "Before SQL" : "After SQL"} (on {target?.name ?? "the target connection"}, {which === "before" ? "before the rows are written" : "after they are written"})
      </label>
      <CellEditor
        editorKey={`${EDITOR_KEY(job.id, node.id)}:${which}`}
        source={value ?? ""}
        connectionId={node.target_connection_id ?? null}
        height={110}
        onResize={() => {}}
        onChange={(sql) => onChange(which === "before" ? { load_before_sql: sql } : { load_after_sql: sql })}
        onRun={() => {}}
        onRunAdvance={() => {}}
        onFocus={() => {}}
      />
    </div>
  );
  return (
    <div className="space-y-2 rounded-md border border-line p-2">
      <div className="grid grid-cols-2 gap-2">
        <div>
          <label className={label} htmlFor={`target-${node.id}`}>
            Into connection
          </label>
          <select id={`target-${node.id}`} className={field} value={node.target_connection_id ?? ""} onChange={(e) => onChange({ target_connection_id: e.target.value || null })}>
            <option value="">Choose…</option>
            {connections.map((c) => (
              <option key={c.id} value={c.id} disabled={c.config.read_only}>
                {c.name}
                {c.config.read_only ? " (read-only)" : ""}
              </option>
            ))}
          </select>
        </div>
        <div>
          <label className={label} htmlFor={`table-${node.id}`}>
            Table
          </label>
          <input id={`table-${node.id}`} className={`${field} font-mono`} placeholder="schema.table" value={node.target_table ?? ""} onChange={(e) => onChange({ target_table: e.target.value })} />
        </div>
      </div>
      <div>
        <label className={label} htmlFor={`mode-${node.id}`}>
          Mode
        </label>
        <select id={`mode-${node.id}`} className={field} value={mode.mode} onChange={(e) => onChange({ load_mode: e.target.value as LoadMode })}>
          {LOAD_MODES.map((m) => (
            <option key={m.mode} value={m.mode}>
              {m.text}
            </option>
          ))}
        </select>
        <div className="mt-0.5 text-[11px] text-muted">{mode.hint}</div>
      </div>
      {(node.load_mode === "append" || node.load_mode === "truncate") && (
        <label className="flex items-center gap-1.5 text-[12px]">
          <input type="checkbox" checked={node.create_table ?? true} onChange={(e) => onChange({ create_table: e.target.checked })} />
          Create the table if it's missing (from the rows' columns; you get a notice)
        </label>
      )}
      {keyed && (
        <div>
          <label className={label} htmlFor={`keys-${node.id}`}>
            Key columns
          </label>
          <input
            id={`keys-${node.id}`}
            className={`${field} font-mono ${parseKeyColumns(keys).length ? "" : "border-warning"}`}
            placeholder="id, region"
            value={keys}
            onChange={(e) => setKeys(e.target.value)}
            onBlur={() => onChange({ key_columns: parseKeyColumns(keys) })}
          />
          <div className="mt-0.5 text-[11px] text-muted">
            {parseKeyColumns(keys).length ? "Comma-separated columns that identify a row (in the rows and the table)." : "Required: the columns that identify a row, comma-separated."}
          </div>
        </div>
      )}
      <BatchFields node={node} kind={target?.config.kind} onChange={onChange} />
      <div className="flex flex-wrap gap-1.5">
        {!showBefore && (
          <button className="btn-ghost border border-line py-0.5 text-[11.5px]" onClick={() => setShowBefore(true)}>
            <Plus size={12} /> Before SQL
          </button>
        )}
        {!showAfter && (
          <button className="btn-ghost border border-line py-0.5 text-[11.5px]" onClick={() => setShowAfter(true)}>
            <Plus size={12} /> After SQL
          </button>
        )}
      </div>
      {showBefore && sqlBox("before", node.load_before_sql)}
      {showAfter && sqlBox("after", node.load_after_sql)}
      {target?.env === "prod" && (
        <div className="flex items-center gap-1 text-[11.5px] text-warning">
          <AlertTriangle size={12} /> {target.name} is a production connection: runs write to it without asking.
        </div>
      )}
      <div className="flex items-center gap-2">
        <button
          className="btn-ghost border border-line py-1 text-[12px]"
          disabled={dry.busy || !node.target_connection_id}
          title="Check every statement of this step (before SQL, the load, after SQL) without keeping any change"
          onClick={() => void runDry()}
        >
          {dry.busy ? <Loader2 size={12} className="animate-spin" /> : <FlaskConical size={12} />} Dry run
        </button>
        <span className="text-[11px] text-muted">Checks the SQL with up to 1,000 of the rows; nothing is kept.</span>
      </div>
      {dry.error && <div className="text-[11.5px] text-danger">{dry.error}</div>}
      {dry.report && <DryRunReport report={dry.report} />}
    </div>
  );
}

function DryRunReport({ report }: { report: LoadDryRun }) {
  const how =
    report.method === "transaction"
      ? `Ran on ${report.sample_rows != null ? `${formatCount(report.sample_rows)} sample rows` : "no rows"} in a transaction, then rolled back`
      : report.method === "explain"
        ? "Checked statement by statement without running them (this database can't roll the whole step back)"
        : "The step could not be planned";
  return (
    <div className={`rounded-md border px-2 py-1.5 text-[11.5px] ${report.ok ? "border-success/50" : "border-danger/50 bg-danger/5"}`} role="status">
      <div className="flex items-center gap-1.5 font-medium">
        {report.ok ? <CheckCircle2 size={12} className="text-success" /> : <XCircle size={12} className="text-danger" />}
        {report.ok ? "Dry run passed" : "Dry run found problems"}
      </div>
      <div className="text-muted">
        {how}.
        {report.batch && ` INSERTs of up to ${formatCount(report.batch.rows)} rows / ${formatCount(Math.floor(report.batch.bytes / 1024))} KB.`}
      </div>
      {report.rows_error && <div className="mt-0.5 text-danger">Rows: {report.rows_error}</div>}
      {report.notices.map((n, i) => (
        <div key={i} className="mt-0.5 flex items-start gap-1 text-accent">
          <Info size={12} className="mt-0.5 shrink-0" /> <span className="min-w-0 break-words">{n}</span>
        </div>
      ))}
      <ul className="mt-1 space-y-0.5">
        {report.checks.map((c, i) => (
          <li key={i} className="flex items-start gap-1.5">
            {c.status === "ok" ? (
              <CheckCircle2 size={12} className="mt-0.5 shrink-0 text-success" />
            ) : c.status === "error" ? (
              <XCircle size={12} className="mt-0.5 shrink-0 text-danger" />
            ) : c.status === "warning" ? (
              <AlertTriangle size={12} className="mt-0.5 shrink-0 text-warning" />
            ) : (
              <CircleDashed size={12} className="mt-0.5 shrink-0 text-muted" />
            )}
            <div className="min-w-0 flex-1">
              <span className="font-medium">{c.label}</span>
              {c.message && <span className={c.status === "error" ? " text-danger" : " text-muted"}> · {c.message}</span>}
              {c.sql && (
                <div className="truncate font-mono text-[10.5px] text-muted" title={c.sql}>
                  {c.sql}
                </div>
              )}
            </div>
          </li>
        ))}
      </ul>
    </div>
  );
}

/** Last part of a folder path ("Downloads" when none is set). */
function folderName(f?: string | null): string {
  const t = f?.trim().replace(/[/\\]+$/, "");
  return t ? (t.split(/[/\\]/).pop() ?? t) : "Downloads";
}

/** Folder, file name and format of an export step. */
function ExportFields({ job, node, onChange }: { job: Job; node: JobNode; onChange: (patch: Partial<JobNode>) => void }) {
  const [downloads, setDownloads] = useState<string | null>(null);
  useEffect(() => {
    api
      .defaultExportFolder()
      .then(setDownloads)
      .catch(() => {});
  }, []);
  const format = node.export_format ?? "csv";
  const folder = node.export_folder?.trim() || downloads || "Downloads";
  const preview = exportFileName(node.export_file, node.name, job.name, format);
  const choose = async () => {
    const picked = await openDialog({ directory: true, multiple: false, defaultPath: node.export_folder?.trim() || downloads || undefined, title: "Folder to save the file in" }).catch(() => null);
    if (typeof picked === "string") onChange({ export_folder: picked });
  };
  return (
    <div className="space-y-2 rounded-md border border-line p-2">
      <div>
        <label className={label}>Folder</label>
        <div className="flex gap-1">
          <div className={`${field} min-w-0 flex-1 truncate font-mono text-[11.5px]`} title={folder}>
            {folder}
          </div>
          <button className="btn-ghost shrink-0 py-1" onClick={() => void choose()}>
            <FolderOpen size={12} /> Choose…
          </button>
          {node.export_folder && (
            <button className="btn-ghost shrink-0 py-1" title="Use the Downloads folder" onClick={() => onChange({ export_folder: null })}>
              Downloads
            </button>
          )}
        </div>
      </div>
      <div className="grid grid-cols-[1fr_auto] gap-2">
        <div className="min-w-0">
          <label className={label} htmlFor={`file-${node.id}`}>
            File name
          </label>
          <input
            id={`file-${node.id}`}
            className={`${field} font-mono`}
            placeholder="{step}_{date}_{time}  (generated)"
            value={node.export_file ?? ""}
            onChange={(e) => onChange({ export_file: e.target.value || null })}
          />
        </div>
        <div>
          <label className={label} htmlFor={`fmt-${node.id}`}>
            Format
          </label>
          <select id={`fmt-${node.id}`} className={field} value={format} onChange={(e) => onChange({ export_format: e.target.value as FileFormat })}>
            <option value="csv">CSV</option>
            <option value="parquet">Parquet</option>
            <option value="json">JSON (lines)</option>
          </select>
        </div>
      </div>
      <div className="text-[11px] text-muted">
        Next run writes <span className="font-mono text-fg">{preview}</span>
        {node.export_file?.trim() && !/\{(date|time)\}/.test(node.export_file) ? " (replaced on each run)" : ""}. In the name, {"{step}"}, {"{job}"}, {"{date}"} and {"{time}"} are filled in; leave it empty for a new name per run.
      </div>
    </div>
  );
}

function NodeList({ title, nodes, onSelect, empty }: { title: string; nodes: JobNode[]; onSelect: (id: string) => void; empty: string }) {
  return (
    <div>
      <div className={label}>{title}</div>
      {nodes.length === 0 ? (
        <div className="text-muted">{empty}</div>
      ) : (
        nodes.map((n) => (
          <button key={n.id} className="block max-w-full truncate font-mono text-accent hover:underline" onClick={() => onSelect(n.id)}>
            {n.name}
          </button>
        ))
      )}
    </div>
  );
}

const WEEKDAYS = ["Sun", "Mon", "Tue", "Wed", "Thu", "Fri", "Sat"];
const ORDER = [1, 2, 3, 4, 5, 6, 0];

function JobPanel({ job, run, onChange, onSelect }: { job: Job; run?: JobRun; onChange: (patch: Partial<Job>) => void; onSelect: (id: string) => void }) {
  const summary = useJobs((s) => s.jobs.find((j) => j.id === job.id));
  const [runs, setRuns] = useState<JobRun[]>([]);
  const [open, setOpen] = useState<number | null>(null);
  const s = job.schedule;
  const set = (patch: Partial<JobSchedule>) => onChange({ schedule: { ...s, ...patch } });
  useEffect(() => {
    if (run?.status === "running") return;
    api
      .jobRuns(job.id, 30)
      .then(setRuns)
      .catch(() => {});
  }, [job.id, run?.status, run?.id]);
  const shown = run?.status === "running" ? [run, ...runs.filter((r) => r.id !== run.id)] : runs;

  return (
    <div className="space-y-4 p-3">
      <div className="text-[11.5px] text-muted">Select a step to edit its connection and SQL. Steps run after their upstream steps; steps that don't depend on each other run at the same time.</div>
      <section className="space-y-2">
        <div className="flex items-center justify-between">
          <div className={label}>Schedule</div>
          <label className="flex items-center gap-1.5 text-[12px]">
            <input type="checkbox" checked={s.enabled} onChange={(e) => set({ enabled: e.target.checked })} /> Run on a schedule
          </label>
        </div>
        <div className={`space-y-2 ${s.enabled ? "" : "pointer-events-none opacity-50"}`}>
          <div className="flex rounded-md border border-line p-0.5" role="group" aria-label="Schedule kind">
            {(["interval", "daily"] as const).map((m) => (
              <button key={m} className={`flex-1 rounded px-2 py-1 text-[11.5px] ${s.mode === m ? "bg-accent/15 font-medium text-accent" : "text-muted hover:text-fg"}`} onClick={() => set({ mode: m })}>
                {m === "interval" ? "Every…" : "At a time of day"}
              </button>
            ))}
          </div>
          {s.mode === "interval" ? (
            <div className="flex items-center gap-2">
              <span>Every</span>
              <input type="number" min={1} className={`${field} w-20`} value={s.minutes} onChange={(e) => set({ minutes: Math.max(1, Number(e.target.value) || 1) })} aria-label="Minutes" />
              <span>minutes</span>
              <select className={`${field} w-auto`} value="" onChange={(e) => e.target.value && set({ minutes: Number(e.target.value) })} aria-label="Common intervals">
                <option value="">Presets…</option>
                <option value="15">15 min</option>
                <option value="60">1 hour</option>
                <option value="360">6 hours</option>
                <option value="1440">1 day</option>
              </select>
            </div>
          ) : (
            <div className="space-y-2">
              <div className="flex items-center gap-2">
                <span>At</span>
                <input type="time" className={`${field} w-28`} value={s.at} onChange={(e) => set({ at: e.target.value || "08:00" })} aria-label="Time" />
                <span className="text-muted">local time</span>
              </div>
              <div className="flex gap-1" role="group" aria-label="Days">
                {ORDER.map((d) => {
                  const on = s.weekdays.length === 0 || s.weekdays.includes(d);
                  return (
                    <button
                      key={d}
                      aria-pressed={on}
                      className={`flex-1 rounded border px-1 py-0.5 text-[11px] ${on ? "border-accent/50 bg-accent/10 text-accent" : "border-line text-muted"}`}
                      onClick={() => {
                        const cur = s.weekdays.length === 0 ? ORDER : s.weekdays;
                        const next = on ? cur.filter((x) => x !== d) : [...cur, d];
                        set({ weekdays: next.length === 7 ? [] : next });
                      }}
                    >
                      {WEEKDAYS[d]}
                    </button>
                  );
                })}
              </div>
            </div>
          )}
          <div className="text-[11.5px] text-muted">
            {summary?.next_run_at ? `Next run ${new Date(summary.next_run_at).toLocaleString()}` : s.enabled ? "Saving…" : ""} · runs while DataBrain is open; a run missed while it was closed runs once at the next start.
          </div>
        </div>
      </section>

      <section>
        <div className={label}>Runs</div>
        {shown.length === 0 && <div className="text-muted">No runs yet.</div>}
        <ul className="space-y-1">
          {shown.map((r) => (
            <li key={r.id} className="rounded-md border border-line">
              <button className="flex w-full items-center gap-1.5 px-2 py-1 text-left" onClick={() => setOpen(open === r.id ? null : r.id)}>
                <StatusIcon status={r.status === "running" ? "running" : r.status} />
                <span className="min-w-0 flex-1 truncate">
                  {new Date(r.started_at).toLocaleString()}
                  <span className="text-muted"> · {r.trigger === "schedule" ? "scheduled" : "manual"}</span>
                </span>
                <span className="shrink-0 text-[11px] text-muted">{r.finished_at ? formatDuration(r.finished_at - r.started_at) : "running"}</span>
              </button>
              {(open === r.id || (open === null && r === shown[0] && (r.status === "error" || r.status === "running"))) && (
                <div className="space-y-1.5 border-t border-line px-2 py-1.5">
                  <ul className="space-y-0.5">
                    {r.nodes.map((n) => (
                      <li key={n.node_id} className="space-y-0.5">
                        <div className="flex items-start gap-1.5">
                          <StatusIcon status={n.status} />
                          <button className="shrink-0 font-mono hover:underline" onClick={() => onSelect(n.node_id)}>
                            {n.name}
                          </button>
                          <span className={`min-w-0 break-words text-[11px] ${n.status === "error" ? "text-danger" : "text-muted"}`}>{statusText(n)}</span>
                        </div>
                        {n.status === "running" && n.progress && (
                          <div className="pl-5">
                            <ProgressBar p={n.progress} />
                          </div>
                        )}
                      </li>
                    ))}
                  </ul>
                  <div className={label}>Log</div>
                  <RunLog job={job} run={r} onSelect={onSelect} />
                </div>
              )}
            </li>
          ))}
        </ul>
      </section>
    </div>
  );
}
