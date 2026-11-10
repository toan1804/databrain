import { useEffect, useLayoutEffect, useRef, useState, type KeyboardEvent as ReactKeyboardEvent } from "react";
import { ChevronLeft, ChevronRight, List } from "lucide-react";
import { outputLabel } from "../../outputs";
import { formatCount, sqlPreview } from "../../lib/util";
import type { StatementRun } from "../../store";
import { Popover } from "../ui";
import { StatusIcon } from "./RunStatus";

/** From this many results the "All results" list is offered even when they all fit. */
export const LIST_FROM = 8;

function statusLabel(s: StatementRun) {
  return s.result ? "Result" : s.error ? (s.status === "cancelled" ? "Cancelled" : "Error") : "Running";
}

/**
 * The run's results as tabs: a strip that scrolls sideways (wheel, the
 * arrows, or by keyboard) and keeps the shown result in view, plus a list of
 * all results to jump to one by number, SQL or output name.
 */
export function ResultTabs({
  statements,
  active,
  onSelect,
}: {
  /** Statements that have something to show. */
  statements: StatementRun[];
  /** `plan.index` of the result shown; `null` while Messages is shown. */
  active: number | null;
  onSelect: (index: number) => void;
}) {
  const strip = useRef<HTMLDivElement>(null);
  /** Where the arrows' smooth scroll is going (see `page`). */
  const goal = useRef<number | null>(null);
  const [edges, setEdges] = useState({ left: false, right: false });
  const [list, setList] = useState<{ x: number; y: number; height: number } | null>(null);
  const many = statements.length > 1;

  const measure = () => {
    const el = strip.current;
    if (!el) return;
    const left = el.scrollLeft > 1;
    const right = el.scrollLeft + el.clientWidth < el.scrollWidth - 1;
    setEdges((e) => (e.left === left && e.right === right ? e : { left, right }));
  };

  useLayoutEffect(measure, [statements.length]);
  useEffect(() => {
    const el = strip.current;
    if (!el) return;
    // A vertical wheel scrolls the strip sideways when it overflows.
    const onWheel = (e: WheelEvent) => {
      if (Math.abs(e.deltaY) <= Math.abs(e.deltaX) || el.scrollWidth <= el.clientWidth) return;
      e.preventDefault();
      goal.current = null;
      el.scrollLeft += e.deltaY;
    };
    el.addEventListener("wheel", onWheel, { passive: false });
    const ro = typeof ResizeObserver === "undefined" ? null : new ResizeObserver(measure);
    ro?.observe(el);
    return () => {
      el.removeEventListener("wheel", onWheel);
      ro?.disconnect();
    };
  }, []);
  // The shown result stays in view (a new run, the list, the keyboard).
  useEffect(() => {
    if (active === null) return;
    strip.current?.querySelector<HTMLElement>(`[data-index="${active}"]`)?.scrollIntoView?.({ block: "nearest", inline: "nearest" });
  }, [active, statements.length]);

  // Where the arrows' smooth scroll is going: quick clicks add up instead of restarting from mid-way.
  const page = (dir: 1 | -1) => {
    const el = strip.current;
    if (!el) return;
    const max = el.scrollWidth - el.clientWidth;
    const next = Math.max(0, Math.min(max, (goal.current ?? el.scrollLeft) + dir * Math.max(120, el.clientWidth * 0.7)));
    goal.current = next;
    if (el.scrollTo) el.scrollTo({ left: next, behavior: "smooth" });
    else el.scrollLeft = next;
  };
  const onScroll = () => {
    const el = strip.current;
    if (el && goal.current !== null && Math.abs(el.scrollLeft - goal.current) < 2) goal.current = null;
    measure();
  };

  const select = (index: number, focus = false) => {
    onSelect(index);
    if (focus) requestAnimationFrame(() => strip.current?.querySelector<HTMLElement>(`[data-index="${index}"]`)?.focus());
  };

  const onKey = (e: ReactKeyboardEvent) => {
    const at = statements.findIndex((s) => s.plan.index === active);
    const to =
      e.key === "ArrowRight" ? Math.min(statements.length - 1, at + 1)
      : e.key === "ArrowLeft" ? Math.max(0, at - 1)
      : e.key === "Home" ? 0
      : e.key === "End" ? statements.length - 1
      : null;
    if (to === null || to < 0 || !statements[to]) return;
    e.preventDefault();
    select(statements[to].plan.index, true);
  };

  const overflow = edges.left || edges.right;
  const showList = overflow || statements.length >= LIST_FROM;

  return (
    <div className="flex min-w-0 flex-1 items-center gap-0.5">
      {overflow && (
        <button className="icon-btn h-6 w-5 shrink-0" aria-label="Scroll results left" title="Earlier results" disabled={!edges.left} onClick={() => page(-1)}>
          <ChevronLeft size={13} />
        </button>
      )}
      <div
        ref={strip}
        role="tablist"
        aria-label="Results"
        onScroll={onScroll}
        onKeyDown={onKey}
        className="no-scrollbar flex min-w-0 flex-1 items-center gap-0.5 overflow-x-auto"
      >
        {statements.map((s) => {
          const isActive = active === s.plan.index;
          return (
            <button
              key={s.plan.index}
              data-index={s.plan.index}
              role="tab"
              aria-selected={isActive}
              tabIndex={isActive || (active === null && s === statements[0]) ? 0 : -1}
              onClick={() => select(s.plan.index)}
              title={s.plan.sql}
              className={`flex h-7 max-w-[220px] shrink-0 items-center gap-1.5 rounded-md px-2 text-[12px] ${
                isActive ? "bg-hover text-fg" : "text-muted hover:text-fg"
              }`}
            >
              <StatusIcon s={s} />
              <span className="truncate">
                {many ? `${s.plan.index + 1}. ` : ""}
                {statusLabel(s)}
              </span>
              {s.result && <span className="text-[11px] text-muted">{formatCount(s.result.total_rows)}</span>}
              {s.output && <span className="font-mono text-[10.5px] text-muted">{outputLabel(s.output)}</span>}
            </button>
          );
        })}
      </div>
      {overflow && (
        <button className="icon-btn h-6 w-5 shrink-0" aria-label="Scroll results right" title="Later results" disabled={!edges.right} onClick={() => page(1)}>
          <ChevronRight size={13} />
        </button>
      )}
      {showList && (
        <button
          className="icon-btn h-6 w-6 shrink-0"
          aria-label="All results"
          aria-haspopup="dialog"
          aria-expanded={!!list}
          title={`All ${statements.length} results`}
          // The open list closes on a press outside it; this button toggles it instead.
          onMouseDown={(e) => list && e.nativeEvent.stopPropagation()}
          onClick={(e) => {
            if (list) return setList(null);
            // Under the button, right-aligned to it; the list gets the height left below.
            const r = e.currentTarget.getBoundingClientRect();
            const width = Math.min(420, window.innerWidth * 0.9);
            const y = r.bottom + 4;
            setList({ x: Math.max(4, r.right - width), y, height: Math.max(120, window.innerHeight - y - 56) });
          }}
        >
          <List size={13} />
        </button>
      )}
      {list && (
        <ResultList
          statements={statements}
          active={active}
          x={list.x}
          y={list.y}
          height={list.height}
          onClose={() => setList(null)}
          onPick={(i) => {
            setList(null);
            select(i, true);
          }}
        />
      )}
    </div>
  );
}

/** Every result of the run, filterable by number, SQL or output name. */
function ResultList({
  statements,
  active,
  x,
  y,
  height,
  onClose,
  onPick,
}: {
  statements: StatementRun[];
  active: number | null;
  x: number;
  y: number;
  /** Most height the list may take (the space below its button). */
  height: number;
  onClose: () => void;
  onPick: (index: number) => void;
}) {
  const [q, setQ] = useState("");
  const listRef = useRef<HTMLUListElement>(null);
  const term = q.trim().toLowerCase();
  // A number picks that result (as numbered on the tabs); other text matches the SQL or output name.
  const byNumber = /^\d+$/.test(term);
  const items = !term
    ? statements
    : byNumber
      ? statements.filter((s) => String(s.plan.index + 1) === term)
      : statements.filter((s) => s.plan.sql.toLowerCase().includes(term) || (s.output && outputLabel(s.output).toLowerCase().includes(term)));
  const [at, setAt] = useState(() => Math.max(0, statements.findIndex((s) => s.plan.index === active)));
  const cur = Math.min(at, Math.max(0, items.length - 1));
  useEffect(() => {
    listRef.current?.children[cur]?.scrollIntoView?.({ block: "nearest" });
  }, [cur]);

  return (
    <Popover x={x} y={y} onClose={onClose} className="w-[420px] max-w-[90vw]">
      <input
        autoFocus
        className="field mb-1 py-1"
        placeholder="Find a result: number, SQL or output name"
        aria-label="Find a result"
        aria-controls="result-list"
        aria-activedescendant={items[cur] ? `result-opt-${items[cur].plan.index}` : undefined}
        value={q}
        onChange={(e) => {
          setQ(e.target.value);
          setAt(0);
        }}
        onKeyDown={(e) => {
          if (e.key === "ArrowDown" || e.key === "ArrowUp") {
            e.preventDefault();
            const n = items.length;
            if (n) setAt((a) => (e.key === "ArrowDown" ? (Math.min(a, n - 1) + 1) % n : (Math.min(a, n - 1) - 1 + n) % n));
          } else if (e.key === "Enter" && items[cur]) {
            e.preventDefault();
            onPick(items[cur].plan.index);
          }
        }}
      />
      <ul ref={listRef} id="result-list" role="listbox" aria-label="Matching results" className="overflow-auto" style={{ maxHeight: Math.min(320, height) }}>
        {items.map((s, i) => (
          <li
            key={s.plan.index}
            id={`result-opt-${s.plan.index}`}
            role="option"
            aria-selected={s.plan.index === active}
            onMouseDown={(e) => e.preventDefault()}
            onClick={() => onPick(s.plan.index)}
            onMouseEnter={() => setAt(i)}
            className={`flex cursor-pointer items-center gap-2 rounded-md px-2 py-1 text-[12px] ${i === cur ? "bg-hover" : ""} ${
              s.plan.index === active ? "text-fg" : "text-muted"
            }`}
          >
            <StatusIcon s={s} />
            <span className="w-8 shrink-0 text-right tabular-nums">{s.plan.index + 1}.</span>
            <span className="min-w-0 flex-1 truncate font-mono text-[11.5px] text-fg">{sqlPreview(s.plan.sql, 90)}</span>
            {s.result && <span className="shrink-0 text-[11px]">{formatCount(s.result.total_rows)} rows</span>}
            {!s.result && <span className="shrink-0 text-[11px]">{statusLabel(s)}</span>}
            {s.output && <span className="shrink-0 font-mono text-[10.5px]">{outputLabel(s.output)}</span>}
          </li>
        ))}
        {items.length === 0 && <li className="px-2 py-1 text-[12px] text-muted">No result matches</li>}
      </ul>
    </Popover>
  );
}
