// Job editor tab: steps (nodes) on a canvas you can pan, zoom and drag, with
// links from upstream to downstream steps (drag a step's bottom handle onto
// another step), a side panel to edit the selected step's connection and SQL
// (or the job's schedule and runs), and runs with live status per step.
import { useCallback, useEffect, useMemo, useRef, useState, type PointerEvent as ReactPointerEvent } from "react";
import { AlertTriangle, FileDown, ArrowDownToLine, CalendarClock, Copy, Database, Eye, Loader2, Maximize2, Play, PlayCircle, Plus, RotateCcw, Square, Trash2 } from "lucide-react";
import { api, toError } from "../lib/api";
import { kbd, modalOpen } from "../lib/platform";
import type { RunParams, Job, JobEdge, JobNode } from "../lib/types";
import { NODE_H, NODE_W, addDownstream, addEdge, ancestors, cappedReads, descendants, edgeProblem, missingLinks, newNode, nodeStatus, readSql, removeEdge, removeNode, scheduleText, updateStep, upstreamOf } from "../lib/jobGraph";
import { isQueryTab, useStore } from "../store";
import { useJobs } from "../jobsStore";
import { openOutput, resultsConnection } from "../outputs";
import { SplitHandle } from "./SplitHandle";
import { MenuItem, MenuSeparator, Popover } from "./ui";
import { JobPanel } from "./job/JobPanel";
import { NodeCard } from "./job/NodeCard";
import { NodePanel } from "./job/NodePanel";
import { EDITOR_KEY, isResults, latestTab } from "./job/shared";

const SAVE_DELAY = 600;
const PANEL_KEY = "db.job.panel";
const PANEL_DEFAULT = 440;
type Selection = { node: string } | { edge: JobEdge } | null;
type Menu = { x: number; y: number; node?: string; at?: { x: number; y: number } } | null;

export function JobTab({ tabId, jobId, visible }: { tabId: string; jobId: string; visible: boolean }) {
  const toast = useStore((s) => s.toast);
  const connections = useStore((s) => s.connections);
  const run = useJobs((s) => s.runs[jobId]);
  const taken = useTakenNames(jobId);
  // Running per the run's own events too (the job list can lag behind).
  const running = useJobs((s) => !!s.running[jobId] || s.runs[jobId]?.status === "running");
  // The last run, also one from before this session (for "Resume").
  const savedRun = useJobs((s) => s.jobs.find((j) => j.id === jobId)?.last_run ?? undefined);
  const jobSavedAt = useJobs((s) => s.jobs.find((j) => j.id === jobId)?.updated_at ?? 0);
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
    // Changed elsewhere (the assistant's update_job): show the saved version.
    const onChanged = (ev: Event) => {
      if ((ev as CustomEvent<{ id: string }>).detail?.id !== jobId) return;
      clearTimeout(timer.current);
      dirty.current = false;
      void api
        .getJob(jobId)
        .then((j) => {
          latest.current = j;
          setJob(j);
          setSave({ state: "saved" });
        })
        .catch(() => {});
    };
    window.addEventListener("db:job-changed", onChanged);
    return () => window.removeEventListener("db:job-changed", onChanged);
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
  const name = job?.name;
  useEffect(() => {
    if (name !== undefined) useStore.getState().updateTab(tabId, { title: name });
  }, [name, tabId]);

  // A finished run's results become the steps' last run (already saved by the app).
  useEffect(() => {
    if (!run || run.status === "running" || !latest.current) return;
    const done = new Map(run.nodes.map((r) => [r.node_id, r]));
    const next = { ...latest.current, nodes: latest.current.nodes.map((n) => (done.has(n.id) ? { ...n, last_run: done.get(n.id)! } : n)) };
    latest.current = next;
    setJob(next);
  }, [run]);

  const start = async (nodes?: string[], params?: RunParams) => {
    if (!(await persist())) return toast("Fix the job first: " + (save.message ?? "it could not be saved"), "error");
    await useJobs.getState().run(jobId, nodes, params);
  };
  const resume = async () => {
    if (!(await persist())) return toast("Fix the job first: " + (save.message ?? "it could not be saved"), "error");
    await useJobs.getState().resume(jobId);
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

  // The canvas (and `box`) exists once the job has loaded.
  const loaded = job !== null;
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
  }, [loaded]);

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

  const deleteNode = useCallback(
    (id: string) => {
      const n = latest.current?.nodes.find((x) => x.id === id);
      if (!n) return;
      const go = () => {
        update((j) => removeNode(j, id));
        setSel(null);
      };
      if (!n.sql.trim()) return go();
      useStore.getState().askConfirm({ title: `Delete step "${n.name}"?`, reasons: ["Its SQL and links are removed from the job."], confirmLabel: "Delete", onConfirm: go });
    },
    [update],
  );

  // Delete / Backspace removes the selected step or link (not while typing).
  useEffect(() => {
    if (!visible) return;
    const key = (e: KeyboardEvent) => {
      if (e.key !== "Delete" && e.key !== "Backspace") return;
      if (modalOpen()) return;
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
  }, [visible, sel, update, deleteNode]);

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
  // Steps reading a query tab's output that stopped at its row limit or memory: they'd fail.
  const outputs = useStore((s) => s.outputs);
  const capped = useMemo(() => {
    const resultsId = connections.find((c) => c.config.kind === "duckdb" && c.config.options?.databrain_results === "1")?.id;
    return job ? cappedReads(job, outputs, resultsId) : [];
  }, [job, outputs, connections]);

  if (loadError) return <div className="p-6 text-[13px] text-danger">{loadError}</div>;
  if (!job) {
    return (
      <div className="flex h-full items-center justify-center gap-2 text-muted">
        <Loader2 size={15} className="animate-spin text-accent" /> Opening job…
      </div>
    );
  }

  const byId = new Map(job.nodes.map((n) => [n.id, n]));
  // A run that failed or was stopped can be resumed: the steps that did not
  // succeed run again, the others keep their outputs. Suggested first once
  // the job or a connection it uses changed since (the user fixed something).
  const lastRun = run && savedRun ? (run.id >= savedRun.id ? run : savedRun) : (run ?? savedRun);
  const canResume = !running && !!lastRun && (lastRun.status === "error" || lastRun.status === "cancelled") && job.nodes.some((n) => n.last_run?.status !== "success");
  const failedAt = lastRun?.finished_at ?? lastRun?.started_at ?? 0;
  const usedConns = new Set(job.nodes.flatMap((n) => [n.connection_id, n.target_connection_id]).filter(Boolean));
  const fixedSince = canResume && (jobSavedAt > failedAt || connections.some((c) => usedConns.has(c.id) && c.updated_at > failedAt));
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
            <>
              {canResume && (
                <button
                  className={`${fixedSince ? "btn-primary" : "btn-ghost"} py-1`}
                  onClick={() => void resume()}
                  title="Run again the steps that failed, were skipped or stopped, and the steps after them. Steps that succeeded keep their outputs."
                >
                  <RotateCcw size={13} /> Resume
                </button>
              )}
              <button className={`${fixedSince ? "btn-ghost" : "btn-primary"} py-1`} onClick={() => void start()} disabled={!job.nodes.length}>
                <Play size={13} /> Run job
              </button>
            </>
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
                onStop={() => void useJobs.getState().cancelStep(jobId, n.id)}
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
            Drag a step's ● handle onto another step to run it after · right-click a step to add downstream steps · drag steps to move · {kbd("⌘")}+scroll to zoom
          </div>
          {warnings.length + capped.length > 0 && (
            <div className="absolute right-2 top-2 max-w-sm space-y-1">
              {capped.map((w) => (
                <div
                  key={`capped:${w.node}:${w.reads}`}
                  role="alert"
                  className="flex w-full items-start gap-1.5 rounded-md border border-warning/40 bg-panel px-2 py-1 text-[11.5px] text-warning shadow"
                >
                  <AlertTriangle size={12} className="mt-0.5 shrink-0" />
                  <span>
                    {byId.get(w.node)?.name} reads results.{w.reads}, which holds only its first {w.rows.toLocaleString()} rows (a row limit or the memory a
                    result may keep): the step fails before it runs. Run that query without a row limit first.
                  </span>
                </div>
              ))}
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
            <JobPanel
              job={job}
              run={run}
              canResume={canResume}
              onResume={() => void resume()}
              onRunWith={(params) => void start(undefined, params)}
              onChange={(patch) => update((j) => ({ ...j, ...patch }))}
              onSelect={(id) => setSel({ node: id })}
            />
          )}
        </aside>
      </div>
      {menu && (
        <Popover x={menu.x} y={menu.y} onClose={() => setMenu(null)}>
          {menu.node ? (
            <>
              {nodeStatus(byId.get(menu.node)!, run)?.status === "running" ? (
                <MenuItem icon={<Square size={12} />} label="Stop this step" danger onClick={() => (setMenu(null), void useJobs.getState().cancelStep(jobId, menu.node!))} />
              ) : (
                <MenuItem icon={<Play size={13} />} label="Run this step" disabled={running} onClick={() => (setMenu(null), void start([menu.node!]))} />
              )}
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
        <MenuItem key={t.id} label={t.title} hint={t.sql.trim().split("\n")[0].slice(0, 40)} onClick={() => onPick(latestTab(t))} />
      ))}
    </>
  );
}

function OutputMenuItem({ jobId, node, onDone }: { jobId: string; node: JobNode; onDone: () => void }) {
  const o = useStore((s) => s.outputs.find((x) => x.tab_id === EDITOR_KEY(jobId, node.id) && x.active && x.state !== "evicted"));
  return <MenuItem icon={<Eye size={13} />} label="View output" hint={o ? `results.${node.name}` : "run the step first"} disabled={!o} onClick={() => (onDone(), o && void openOutput(o))} />;
}
