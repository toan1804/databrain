import { useEffect, useLayoutEffect, useRef, useState, type ReactNode } from "react";
import { createPortal } from "react-dom";
import { AlertTriangle, CheckCircle2, Info, X, XCircle } from "lucide-react";
import { useStore } from "../store";
import { comboMatches, placeComboList } from "../lib/util";
import { ExportsTray } from "./ExportsTray";

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
        // An open suggestion list (ComboInput) closes first, not the dialog.
        const t = e.target as HTMLElement | null;
        if (t?.getAttribute?.("role") === "combobox" && t.getAttribute("aria-expanded") === "true") return;
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

/**
 * Text input with a themed list of suggestions (replaces `<datalist>`, whose
 * native popup in the macOS WebView ignores the app theme: dark text on a
 * dark list once the field has a value). Free text is still allowed.
 */
export function ComboInput({
  value,
  options,
  onChange,
  className = "",
  listMinWidth = 220,
  ...rest
}: {
  value: string;
  options: string[];
  onChange: (v: string) => void;
  className?: string;
  listMinWidth?: number;
  placeholder?: string;
  "aria-label"?: string;
  id?: string;
  disabled?: boolean;
}) {
  const ref = useRef<HTMLInputElement>(null);
  const listRef = useRef<HTMLUListElement>(null);
  const [open, setOpen] = useState(false);
  const [active, setActive] = useState(-1);
  const [pos, setPos] = useState<{ left: number; top?: number; bottom?: number; width: number; maxHeight: number } | null>(null);
  const items = open ? comboMatches(options, value) : [];
  const listId = useRef(`combo-${Math.random().toString(36).slice(2)}`).current;
  const shown = open && items.length > 0;

  // Place the list under the field, or above it when there is no room below (AI panel footer).
  useLayoutEffect(() => {
    if (!shown || !ref.current) return;
    const place = () => {
      const r = ref.current!.getBoundingClientRect();
      setPos(placeComboList(r, { width: window.innerWidth, height: window.innerHeight }, listMinWidth));
    };
    place();
    window.addEventListener("resize", place);
    window.addEventListener("scroll", place, true);
    return () => {
      window.removeEventListener("resize", place);
      window.removeEventListener("scroll", place, true);
    };
  }, [shown, listMinWidth]);
  useEffect(() => {
    if (active >= 0) listRef.current?.children[active]?.scrollIntoView({ block: "nearest" });
  }, [active]);

  const pick = (v: string) => {
    onChange(v);
    setOpen(false);
    setActive(-1);
  };

  return (
    <>
      <input
        {...rest}
        ref={ref}
        className={className}
        value={value}
        role="combobox"
        aria-autocomplete="list"
        aria-expanded={shown}
        aria-controls={listId}
        aria-activedescendant={shown && active >= 0 ? `${listId}-${active}` : undefined}
        autoComplete="off"
        spellCheck={false}
        onChange={(e) => {
          onChange(e.target.value);
          setOpen(true);
          setActive(-1);
        }}
        onFocus={() => setOpen(true)}
        onClick={() => setOpen(true)}
        onBlur={() => setOpen(false)}
        onKeyDown={(e) => {
          if (e.key === "ArrowDown" || e.key === "ArrowUp") {
            e.preventDefault();
            if (!shown) return setOpen(true);
            const n = items.length;
            setActive((a) => (e.key === "ArrowDown" ? (a + 1) % n : a <= 0 ? n - 1 : a - 1));
          } else if (e.key === "Enter" && shown && active >= 0) {
            e.preventDefault();
            pick(items[active]);
          } else if (e.key === "Escape" && shown) {
            // Close the list only, not the dialog around it.
            e.preventDefault();
            e.stopPropagation();
            setOpen(false);
          }
        }}
      />
      {shown &&
        pos &&
        createPortal(
          <ul
            ref={listRef}
            id={listId}
            role="listbox"
            className="fixed z-[70] overflow-auto rounded-md border border-line bg-panel py-0.5 text-fg shadow-lg"
            style={{ left: pos.left, top: pos.top, bottom: pos.bottom, width: pos.width, maxHeight: pos.maxHeight }}
          >
            {items.map((o, i) => (
              <li
                key={o}
                id={`${listId}-${i}`}
                role="option"
                aria-selected={i === active}
                className={`cursor-pointer truncate px-2 py-1 font-mono text-[11.5px] ${i === active ? "bg-hover" : ""} ${o === value ? "text-accent" : ""}`}
                title={o}
                // mousedown, so the field doesn't blur (and close the list) first
                onMouseDown={(e) => {
                  e.preventDefault();
                  pick(o);
                }}
                onMouseEnter={() => setActive(i)}
              >
                {o}
              </li>
            ))}
          </ul>,
          document.body,
        )}
    </>
  );
}

export function Toasts() {
  const toasts = useStore((s) => s.toasts);
  const dismiss = useStore((s) => s.dismissToast);
  return (
    <div className="pointer-events-none fixed bottom-4 right-4 z-[60] flex w-[360px] flex-col gap-2" aria-live="polite">
      <ExportsTray />
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
