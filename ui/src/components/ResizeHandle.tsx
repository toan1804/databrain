import { useRef } from "react";
import { clampHeight } from "../lib/resize";

/**
 * Horizontal grip under a block: drag to change its height, double-click
 * to reset, ↑/↓ (Shift = ×5) to resize with the keyboard. `onChange` fires
 * while dragging; `onCommit` once at the end (save).
 */
export function ResizeHandle({
  height,
  min,
  max,
  onChange,
  onCommit,
  onReset,
  label,
}: {
  /** Current height of the block (measured when unset). */
  height: () => number;
  min: number;
  max: number;
  onChange: (h: number) => void;
  onCommit: (h: number) => void;
  onReset: () => void;
  label: string;
}) {
  const drag = useRef<{ y: number; h: number; last: number } | null>(null);
  return (
    <div
      role="separator"
      aria-orientation="horizontal"
      aria-label={label}
      aria-valuemin={min}
      aria-valuemax={max}
      tabIndex={0}
      title={`${label}: drag to resize · double-click to reset`}
      className="group/rh relative z-10 flex h-2.5 cursor-row-resize touch-none items-center justify-center outline-none"
      onPointerDown={(e) => {
        if (e.button !== 0) return;
        e.preventDefault();
        (e.currentTarget as HTMLElement).setPointerCapture(e.pointerId);
        const h = height();
        drag.current = { y: e.clientY, h, last: h };
        document.body.style.cursor = "row-resize";
        document.body.style.userSelect = "none";
      }}
      onPointerMove={(e) => {
        const d = drag.current;
        if (!d) return;
        d.last = clampHeight(d.h + e.clientY - d.y, min, max);
        onChange(d.last);
      }}
      onPointerUp={(e) => {
        const d = drag.current;
        drag.current = null;
        document.body.style.cursor = "";
        document.body.style.userSelect = "";
        (e.currentTarget as HTMLElement).releasePointerCapture?.(e.pointerId);
        if (d && d.last !== d.h) onCommit(d.last);
      }}
      onDoubleClick={onReset}
      onKeyDown={(e) => {
        if (e.key !== "ArrowUp" && e.key !== "ArrowDown") return;
        e.preventDefault();
        const step = (e.shiftKey ? 100 : 20) * (e.key === "ArrowUp" ? -1 : 1);
        const h = clampHeight(height() + step, min, max);
        onChange(h);
        onCommit(h);
      }}
    >
      <span className="h-[3px] w-10 rounded-full bg-line transition-colors group-hover/rh:bg-accent group-focus/rh:bg-accent" />
    </div>
  );
}
