import { useEffect, useLayoutEffect, useRef, useState, type ReactNode } from "react";
import { createPortal } from "react-dom";
import { AlertTriangle, CheckCircle2, Info, X, XCircle } from "lucide-react";
import { useStore } from "../store";

export function Modal({
  title,
  onClose,
  children,
  footer,
  width = 520,
}: {
  title: ReactNode;
  onClose: () => void;
  children: ReactNode;
  footer?: ReactNode;
  width?: number;
}) {
  const ref = useRef<HTMLDivElement>(null);
  // Latest onClose without re-running the effects: callers pass a new
  // function on every render, which used to re-focus the first field on each
  // keystroke (typing in a search box lost the cursor).
  const closeRef = useRef(onClose);
  closeRef.current = onClose;
  useEffect(() => {
    const onKey = (e: KeyboardEvent) => {
      if (e.key === "Escape") {
        e.stopPropagation();
        closeRef.current();
      }
    };
    window.addEventListener("keydown", onKey, true);
    return () => window.removeEventListener("keydown", onKey, true);
  }, []);
  // Focus once when opened: an autoFocus field, else the first field in the body.
  useEffect(() => {
    const root = ref.current;
    if (!root || root.contains(document.activeElement)) return;
    const first = root.querySelector<HTMLElement>("[autofocus], .modal-body input, .modal-body select, .modal-body textarea, input, select, textarea, button");
    first?.focus();
  }, []);

  return createPortal(
    <div
      className="fixed inset-0 z-50 flex items-start justify-center bg-black/40 pt-[10vh] backdrop-blur-[2px]"
      onMouseDown={(e) => e.target === e.currentTarget && onClose()}
    >
      <div
        ref={ref}
        role="dialog"
        aria-modal="true"
        aria-label={typeof title === "string" ? title : undefined}
        className="flex max-h-[80vh] flex-col rounded-xl border border-line bg-panel shadow-2xl"
        style={{ width }}
      >
        <div className="flex items-center justify-between border-b border-line px-4 py-3">
          <div className="text-[14px] font-semibold">{title}</div>
          <button className="icon-btn" onClick={onClose} aria-label="Close">
            <X size={15} />
          </button>
        </div>
        <div className="modal-body min-h-0 flex-1 overflow-auto p-4">{children}</div>
        {footer && (
          <div className="flex items-center justify-end gap-2 border-t border-line px-4 py-3">
            {footer}
          </div>
        )}
      </div>
    </div>,
    document.body,
  );
}

/** Floating panel anchored to a screen point; closes on outside click / Escape. */
export function Popover({
  x,
  y,
  onClose,
  children,
  className = "",
}: {
  x: number;
  y: number;
  onClose: () => void;
  children: ReactNode;
  className?: string;
}) {
  const ref = useRef<HTMLDivElement>(null);
  const [pos, setPos] = useState({ x, y });
  useLayoutEffect(() => {
    const el = ref.current;
    if (!el) return;
    const r = el.getBoundingClientRect();
    setPos({
      x: Math.max(4, Math.min(x, window.innerWidth - r.width - 8)),
      y: Math.max(4, Math.min(y, window.innerHeight - r.height - 8)),
    });
  }, [x, y]);
  useEffect(() => {
    const onDown = (e: MouseEvent) => {
      if (ref.current && !ref.current.contains(e.target as Node)) onClose();
    };
    const onKey = (e: KeyboardEvent) => e.key === "Escape" && onClose();
    const t = setTimeout(() => window.addEventListener("mousedown", onDown), 0);
    window.addEventListener("keydown", onKey);
    return () => {
      clearTimeout(t);
      window.removeEventListener("mousedown", onDown);
      window.removeEventListener("keydown", onKey);
    };
  }, [onClose]);
  return createPortal(
    <div
      ref={ref}
      className={`fixed z-50 rounded-lg border border-line bg-panel p-1 shadow-xl ${className}`}
      style={{ left: pos.x, top: pos.y }}
    >
      {children}
    </div>,
    document.body,
  );
}

export function MenuItem({
  icon,
  label,
  hint,
  onClick,
  danger,
  disabled,
}: {
  icon?: ReactNode;
  label: string;
  hint?: string;
  onClick: () => void;
  danger?: boolean;
  disabled?: boolean;
}) {
  return (
    <button
      disabled={disabled}
      onClick={onClick}
      className={`flex w-full items-center gap-2 rounded-md px-2 py-1.5 text-left text-[13px] hover:bg-hover disabled:opacity-40 ${
        danger ? "text-danger" : "text-fg"
      }`}
    >
      <span className="flex w-4 justify-center text-muted">{icon}</span>
      <span className="flex-1">{label}</span>
      {hint && <span className="text-[11px] text-muted">{hint}</span>}
    </button>
  );
}

export function MenuSeparator() {
  return <div className="my-1 h-px bg-line" />;
}

export function Toasts() {
  const toasts = useStore((s) => s.toasts);
  const dismiss = useStore((s) => s.dismissToast);
  return (
    <div className="pointer-events-none fixed bottom-4 right-4 z-[60] flex w-[360px] flex-col gap-2" aria-live="polite">
      {toasts.map((t) => (
        <div
          key={t.id}
          role={t.kind === "error" ? "alert" : "status"}
          className="pointer-events-auto flex items-start gap-2 rounded-lg border border-line bg-panel px-3 py-2.5 shadow-lg"
        >
          {t.kind === "error" ? (
            <XCircle size={16} className="mt-px shrink-0 text-danger" />
          ) : t.kind === "success" ? (
            <CheckCircle2 size={16} className="mt-px shrink-0 text-success" />
          ) : (
            <Info size={16} className="mt-px shrink-0 text-accent" />
          )}
          <div className="flex-1 whitespace-pre-wrap break-words text-[12.5px]">{t.message}</div>
          <button className="text-muted hover:text-fg" onClick={() => dismiss(t.id)} aria-label="Dismiss">
            <X size={14} />
          </button>
        </div>
      ))}
    </div>
  );
}

export function ConfirmDialog() {
  const confirm = useStore((s) => s.confirm);
  const dismiss = () => useStore.getState().askConfirm(null);
  const close = () => {
    dismiss();
    confirm?.onCancel?.();
  };
  if (!confirm) return null;
  return (
    <Modal
      title={
        <span className="flex items-center gap-2">
          <AlertTriangle size={16} className="text-warning" />
          {confirm.title}
        </span>
      }
      onClose={close}
      footer={
        <>
          <button className="btn-ghost" onClick={close}>
            Cancel
          </button>
          <button
            className="btn-danger"
            onClick={() => {
              dismiss();
              confirm.onConfirm();
            }}
          >
            {confirm.confirmLabel}
          </button>
        </>
      }
    >
      <ul className="list-disc space-y-1.5 pl-5 text-[13px]">
        {confirm.reasons.map((r, i) => (
          <li key={i}>{r}</li>
        ))}
      </ul>
    </Modal>
  );
}

export function EnvBadge({ env }: { env: string }) {
  if (env === "none") return null;
  const cls =
    env === "prod"
      ? "bg-danger/15 text-danger"
      : env === "staging"
        ? "bg-warning/15 text-warning"
        : "bg-success/15 text-success";
  return (
    <span className={`rounded px-1 py-px text-[9.5px] font-semibold uppercase tracking-wide ${cls}`}>
      {env}
    </span>
  );
}

export function ConnDot({ color, connected }: { color?: string | null; connected?: boolean }) {
  return (
    <span
      className="inline-block h-2 w-2 shrink-0 rounded-full"
      style={{
        background: color || "var(--muted)",
        boxShadow: connected ? `0 0 0 2px color-mix(in srgb, ${color || "var(--success)"} 30%, transparent)` : undefined,
      }}
    />
  );
}
