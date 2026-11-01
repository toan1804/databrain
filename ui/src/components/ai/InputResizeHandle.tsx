import { useRef } from "react";

export const INPUT_H_KEY = "db.ai.inputHeight";
export const INPUT_H_DEFAULT = 76;
const INPUT_H_MIN = 44;
/** At most most of the window, so the chat above stays visible. */
export const clampInputH = (h: number) => Math.round(Math.min(Math.max(INPUT_H_MIN, h), Math.max(INPUT_H_MIN, window.innerHeight * 0.6)));

/** Top edge of the question box: drag up/down to resize it, double-click
 *  to reset, ↑/↓ with the keyboard. */
export function InputResizeHandle({ height, onChange }: { height: number; onChange: (h: number) => void }) {
  const drag = useRef<{ y: number; h: number; last: number } | null>(null);
  const commit = (h: number) => {
    const v = clampInputH(h);
    onChange(v);
    localStorage.setItem(INPUT_H_KEY, String(v));
  };
  return (
    <div
      role="separator"
      aria-orientation="horizontal"
      aria-label="Resize the question box"
      aria-valuenow={height}
      aria-valuemin={INPUT_H_MIN}
      tabIndex={0}
      title="Drag to resize · double-click to reset"
      className="group/resize absolute inset-x-3 -top-1.5 z-10 h-3 cursor-row-resize outline-none"
      onPointerDown={(e) => {
        if (e.button !== 0) return;
        e.preventDefault();
        e.currentTarget.setPointerCapture(e.pointerId);
        drag.current = { y: e.clientY, h: height, last: height };
        document.body.style.cursor = "row-resize";
        document.body.style.userSelect = "none";
      }}
      onPointerMove={(e) => {
        const d = drag.current;
        if (!d) return;
        // Dragging up makes it taller.
        d.last = clampInputH(d.h + (d.y - e.clientY));
        onChange(d.last);
      }}
      onPointerUp={(e) => {
        const d = drag.current;
        drag.current = null;
        document.body.style.cursor = "";
        document.body.style.userSelect = "";
        e.currentTarget.releasePointerCapture?.(e.pointerId);
        if (d) commit(d.last);
      }}
      onDoubleClick={() => commit(INPUT_H_DEFAULT)}
      onKeyDown={(e) => {
        if (e.key === "ArrowUp" || e.key === "ArrowDown") {
          e.preventDefault();
          commit(height + (e.key === "ArrowUp" ? 16 : -16));
        }
      }}
    >
      <div className="mx-auto mt-[5px] h-[3px] w-10 rounded-full bg-line opacity-0 transition-opacity group-hover/resize:opacity-100 group-focus-visible/resize:opacity-100 group-active/resize:bg-accent" />
    </div>
  );
}
