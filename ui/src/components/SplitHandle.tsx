import { useRef } from "react";

/**
 * Vertical splitter between two columns. A wide invisible hit area sits above
 * both neighbours (z-index), with a thin line that lights up on hover/drag.
 * `side`: which neighbour the handle resizes ("left" grows when dragged right,
 * "right" grows when dragged left). Double-click resets; ←/→ resize by keyboard.
 */
export function SplitHandle({
  side,
  width,
  min,
  max,
  onChange,
  onCommit,
  onReset,
  label,
}: {
  side: "left" | "right";
  width: number;
  min: number;
  max: () => number;
  onChange: (w: number) => void;
  onCommit: (w: number) => void;
  onReset: () => void;
  label: string;
}) {
  const drag = useRef<{ x: number; w: number; last: number } | null>(null);
  const clamp = (w: number) => Math.round(Math.min(Math.max(min, max()), Math.max(min, w)));
  return (
    <div
      role="separator"
      aria-orientation="vertical"
      aria-label={label}
      aria-valuenow={width}
      aria-valuemin={min}
      tabIndex={0}
      title={`${label}: drag · double-click to reset`}
      className="group/split relative z-20 w-0 shrink-0 outline-none"
      onPointerDown={(e) => {
        if (e.button !== 0) return;
        e.preventDefault();
        (e.currentTarget as HTMLElement).setPointerCapture(e.pointerId);
        drag.current = { x: e.clientX, w: width, last: width };
        document.body.style.cursor = "col-resize";
        document.body.style.userSelect = "none";
      }}
      onPointerMove={(e) => {
        const d = drag.current;
        if (!d) return;
        const dx = e.clientX - d.x;
        d.last = clamp(side === "left" ? d.w + dx : d.w - dx);
        onChange(d.last);
      }}
      onPointerUp={(e) => {
        const d = drag.current;
        drag.current = null;
        document.body.style.cursor = "";
        document.body.style.userSelect = "";
        (e.currentTarget as HTMLElement).releasePointerCapture?.(e.pointerId);
        if (d) onCommit(d.last);
      }}
      onDoubleClick={onReset}
      onKeyDown={(e) => {
        if (e.key !== "ArrowLeft" && e.key !== "ArrowRight") return;
        e.preventDefault();
        const grow = (e.key === "ArrowRight") === (side === "left");
        const w = clamp(width + (grow ? 1 : -1) * (e.shiftKey ? 80 : 20));
        onChange(w);
        onCommit(w);
      }}
    >
      {/* 8px hit area centred on the boundary */}
      <div className="absolute inset-y-0 -left-1 w-2 cursor-col-resize">
        <div className="mx-auto h-full w-px bg-transparent transition-colors group-hover/split:bg-accent/60 group-focus/split:bg-accent group-active/split:bg-accent" />
      </div>
    </div>
  );
}
