import { useEffect, useLayoutEffect, useRef, useState } from "react";
import { createPortal } from "react-dom";
import { Check, ChevronDown, ShieldCheck } from "lucide-react";
import type { ConnectionView } from "../../lib/types";
import { placeComboList } from "../../lib/util";
import { useStore } from "../../store";
import { useAi } from "../../aiStore";
import { ConnDot } from "../ui";

/** The assistant's connections: the current one (from the editor) plus
 *  any others the user ticks, so one question can span several databases.
 *  A list anchored to the button like the model picker's: above it when
 *  there's no room below (the panel footer), never over the question box. */
export function ConnectionPicker({ current }: { current: ConnectionView | null }) {
  const connections = useStore((s) => s.connections);
  const also = useAi((s) => s.alsoConnections);
  const setAlso = useAi((s) => s.setAlsoConnections);
  const [open, setOpen] = useState(false);
  const [filter, setFilter] = useState("");
  const [active, setActive] = useState(0);
  const [pos, setPos] = useState<ReturnType<typeof placeComboList> | null>(null);
  const btn = useRef<HTMLButtonElement>(null);
  const box = useRef<HTMLDivElement>(null);
  const listRef = useRef<HTMLUListElement>(null);
  const usable = connections.filter((c) => c.ai_policy?.ai_enabled !== false);
  const extra = usable.filter((c) => c.id !== current?.id && also.includes(c.id));
  const q = filter.trim().toLowerCase();
  // Current first, then ticked, then the rest.
  const items = usable
    .filter((c) => !q || c.name.toLowerCase().includes(q) || c.config.kind.includes(q))
    .sort((a, b) => {
      const rank = (c: ConnectionView) => (c.id === current?.id ? 0 : also.includes(c.id) ? 1 : 2);
      return rank(a) - rank(b) || a.name.localeCompare(b.name);
    });
  const toggle = (id: string) => {
    if (id === current?.id) return;
    setAlso(also.includes(id) ? also.filter((x) => x !== id) : [...also, id]);
  };
  const close = () => {
    setOpen(false);
    setFilter("");
    setActive(0);
  };

  useLayoutEffect(() => {
    if (!open || !btn.current) return;
    const place = () => {
      const r = btn.current!.getBoundingClientRect();
      const p = placeComboList(r, { width: window.innerWidth, height: window.innerHeight }, 280);
      // Room for the filter box and footer too, but never past the window edge.
      const room = p.top !== undefined ? window.innerHeight - p.top - 4 : r.top - 6;
      setPos({ ...p, maxHeight: Math.max(0, Math.min(340, room)) });
    };
    place();
    window.addEventListener("resize", place);
    window.addEventListener("scroll", place, true);
    return () => {
      window.removeEventListener("resize", place);
      window.removeEventListener("scroll", place, true);
    };
  }, [open]);
  useEffect(() => {
    if (!open) return;
    const onDown = (e: MouseEvent) => {
      if (!box.current?.contains(e.target as Node) && !btn.current?.contains(e.target as Node)) close();
    };
    window.addEventListener("mousedown", onDown);
    return () => window.removeEventListener("mousedown", onDown);
  }, [open]);
  useEffect(() => {
    listRef.current?.children[active]?.scrollIntoView({ block: "nearest" });
  }, [active]);

  return (
    <>
      <button
        ref={btn}
        type="button"
        className={`flex min-w-0 items-center gap-1 rounded px-1 text-[11px] hover:bg-hover hover:text-fg ${open ? "bg-hover text-fg" : "text-muted"}`}
        title={["Connections the assistant uses", current?.name, ...extra.map((c) => c.name)].filter(Boolean).join("\n")}
        aria-haspopup="listbox"
        aria-expanded={open}
        onClick={() => (open ? close() : setOpen(true))}
      >
        <ConnDot color={current?.color} connected={current?.connected} />
        <span className="truncate">{current?.name ?? "no connection"}</span>
        {extra.length > 0 && <span className="shrink-0 rounded bg-accent/15 px-1 text-accent">+{extra.length}</span>}
        <ChevronDown size={10} className={`shrink-0 transition-transform ${open ? "rotate-180" : ""}`} />
      </button>
      {open &&
        pos &&
        createPortal(
          <div
            ref={box}
            className="fixed z-[70] flex flex-col rounded-md border border-line bg-panel text-fg shadow-lg"
            style={{ left: pos.left, top: pos.top, bottom: pos.bottom, width: pos.width, maxHeight: pos.maxHeight }}
          >
            <input
              autoFocus
              className="m-1 rounded border border-line bg-bg px-2 py-1 text-[11.5px] outline-none focus:border-accent"
              placeholder="Filter connections…"
              aria-label="Filter connections"
              role="combobox"
              aria-expanded
              aria-controls="ai-conn-list"
              aria-activedescendant={items[active] ? `ai-conn-${items[active].id}` : undefined}
              value={filter}
              onChange={(e) => {
                setFilter(e.target.value);
                setActive(0);
              }}
              onKeyDown={(e) => {
                if (e.key === "ArrowDown" || e.key === "ArrowUp") {
                  e.preventDefault();
                  const n = items.length;
                  if (n) setActive((a) => (e.key === "ArrowDown" ? (a + 1) % n : a <= 0 ? n - 1 : a - 1));
                } else if ((e.key === "Enter" || e.key === " ") && items[active] && (e.key === "Enter" || !filter)) {
                  e.preventDefault();
                  toggle(items[active].id);
                } else if (e.key === "Escape") {
                  e.preventDefault();
                  e.stopPropagation();
                  close();
                  btn.current?.focus();
                }
              }}
            />
            <ul ref={listRef} id="ai-conn-list" role="listbox" aria-multiselectable className="min-h-0 flex-1 overflow-auto py-0.5">
              {items.length === 0 && <li className="px-2 py-1 text-[11.5px] text-muted">{usable.length ? "No match" : "No connection has AI turned on."}</li>}
              {items.map((c, i) => {
                const isCurrent = c.id === current?.id;
                const on = isCurrent || also.includes(c.id);
                return (
                  <li
                    key={c.id}
                    id={`ai-conn-${c.id}`}
                    role="option"
                    aria-selected={on}
                    aria-disabled={isCurrent}
                    title={isCurrent ? "The current connection (from the editor) is always used" : on ? "Click to stop using it" : "Click to let the assistant use it too"}
                    className={`flex items-center gap-2 px-2 py-1 text-[11.5px] ${i === active ? "bg-hover" : ""} ${isCurrent ? "cursor-default" : "cursor-pointer"}`}
                    // mousedown, so the filter box keeps focus
                    onMouseDown={(e) => {
                      e.preventDefault();
                      toggle(c.id);
                    }}
                    onMouseEnter={() => setActive(i)}
                  >
                    <span className={`flex h-3.5 w-3.5 shrink-0 items-center justify-center rounded border ${on ? "border-accent bg-accent text-accent-fg" : "border-line"} ${isCurrent ? "opacity-60" : ""}`}>
                      {on && <Check size={10} />}
                    </span>
                    <ConnDot color={c.color} connected={c.connected} />
                    <span className={`min-w-0 flex-1 truncate ${on ? "text-accent" : ""}`}>{c.name}</span>
                    <span className="shrink-0 text-[10px] text-muted">{isCurrent ? "current" : c.config.kind}</span>
                  </li>
                );
              })}
            </ul>
            <div className="flex items-center gap-2 border-t border-line px-2 py-1 text-[10.5px] text-muted">
              <span className="min-w-0 flex-1 truncate">Ticked ones can be looked up and queried too</span>
              {extra.length > 0 && (
                <button type="button" className="shrink-0 text-accent hover:underline" onMouseDown={(e) => e.preventDefault()} onClick={() => setAlso([])}>
                  Only current
                </button>
              )}
            </div>
          </div>,
          document.body,
        )}
    </>
  );
}

/** Approve every request of this conversation without asking (off again for a new or another conversation). */
export function AutoApproveToggle() {
  const on = useAi((s) => s.autoApprove);
  const set = useAi((s) => s.setAutoApprove);
  return (
    <button
      type="button"
      aria-pressed={on}
      aria-label="Auto-approve this conversation"
      title={
        on
          ? "Auto-approve is on for this conversation: queries (writes too), job changes and runs, and edits are approved without asking. Click to ask again."
          : "Auto-approve: approve every request of this conversation (queries, writes, jobs, editor changes) without asking. Only for this conversation; a new chat asks again."
      }
      onClick={() => set(!on)}
      className={`flex h-6 shrink-0 items-center gap-1 rounded border px-1.5 text-[11px] ${
        on ? "border-warning/60 bg-warning/15 text-warning" : "border-line text-muted hover:border-accent/60 hover:text-fg"
      }`}
    >
      <ShieldCheck size={11} />
      {on && <span>Auto</span>}
    </button>
  );
}
