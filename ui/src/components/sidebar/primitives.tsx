import { useCallback, type ReactNode } from "react";
import { treeKeyDown } from "../../lib/treeKeys";
import { ChevronDown, ChevronRight, Loader2, Search } from "lucide-react";
import { useStore } from "../../store";

export function PanelHeader({ title, children }: { title: string; children?: ReactNode }) {
  return (
    <div className="flex h-10 shrink-0 items-center justify-between px-3">
      <span className="text-[11px] font-semibold uppercase tracking-wider text-muted">{title}</span>
      <div className="flex items-center gap-0.5">{children}</div>
    </div>
  );
}

export function SearchBox({ value, onChange, placeholder }: { value: string; onChange: (v: string) => void; placeholder: string }) {
  return (
    <div className="px-2 pb-2">
      <div className="relative">
        <Search size={13} className="absolute left-2 top-1/2 -translate-y-1/2 text-muted" />
        <input
          className="field py-1 pl-7"
          placeholder={placeholder}
          value={value}
          aria-label={placeholder}
          onChange={(e) => onChange(e.target.value)}
        />
      </div>
    </div>
  );
}


export function Row({
  depth,
  expanded,
  loading,
  icon,
  label,
  meta,
  onClick,
  onDoubleClick,
  onContextMenu,
  actions,
  active,
  title,
  onDelete,
}: {
  depth: number;
  expanded?: boolean;
  loading?: boolean;
  icon: ReactNode;
  label: ReactNode;
  meta?: ReactNode;
  onClick?: () => void;
  onDoubleClick?: () => void;
  onContextMenu?: (e: React.MouseEvent) => void;
  actions?: ReactNode;
  active?: boolean;
  title?: string;
  /** ⌘⌫ / Ctrl+Delete on the focused row. */
  onDelete?: () => void;
}) {
  return (
    <div
      role="treeitem"
      aria-expanded={expanded}
      tabIndex={0}
      title={title}
      onClick={onClick}
      onDoubleClick={onDoubleClick}
      onContextMenu={onContextMenu}
      onKeyDown={(e) => {
        if (treeKeyDown(e, { expanded, toggle: onClick })) return;
        if (e.key === "Enter" || e.key === " ") {
          e.preventDefault();
          onClick?.();
        } else if (onDelete && (e.key === "Backspace" || e.key === "Delete") && (e.metaKey || e.ctrlKey)) {
          e.preventDefault();
          onDelete();
        }
      }}
      className={`group flex h-[26px] cursor-pointer items-center gap-1.5 rounded-md pr-1 text-[13px] hover:bg-hover ${
        active ? "bg-hover" : ""
      }`}
      style={{ paddingLeft: 4 + depth * 14 }}
    >
      <span className="flex w-3.5 shrink-0 justify-center text-muted">
        {loading ? (
          <Loader2 size={12} className="animate-spin" />
        ) : expanded === undefined ? null : expanded ? (
          <ChevronDown size={13} />
        ) : (
          <ChevronRight size={13} />
        )}
      </span>
      <span className="flex shrink-0 text-muted">{icon}</span>
      <span className="min-w-0 flex-1 truncate">{label}</span>
      {meta && <span className="shrink-0 text-[11px] text-muted group-hover:hidden">{meta}</span>}
      {actions && <span className="hidden shrink-0 items-center group-hover:flex">{actions}</span>}
    </div>
  );
}

/** Expansion state kept in the store, so catalog search can reveal objects. */
export function useTreeOpen(key: string, fallback: boolean): [boolean, (open: boolean) => void] {
  const stored = useStore((s) => s.treeOpen[key]);
  const setTreeOpen = useStore((s) => s.setTreeOpen);
  return [stored ?? fallback, useCallback((open: boolean) => setTreeOpen(key, open), [key, setTreeOpen])];
}
