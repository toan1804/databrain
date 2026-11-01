import { useEffect, useMemo, useRef, useState, type ReactNode } from "react";
import { ClipboardCopy, Columns3, Eye, FunctionSquare, KeyRound, ListOrdered, Play, Table2, TextCursorInput } from "lucide-react";
import { toError } from "../../lib/api";
import type { ConnectionView, DbObject } from "../../lib/types";
import {
  GROUP_PAGE,
  isRelationKind,
  columnList,
  groupOpenByDefault,
  pageObjects,
  tablePath,
  treeKey,
  groupLabel,
  type ObjectGroupName,
} from "../../lib/catalog";
import { selectTop } from "../CatalogSearch";
import { ColumnMenu, ObjectMenu, copyText, insertColumns, insertText } from "../CatalogMenus";
import { formatCount, quoteIdent } from "../../lib/util";
import { useStore } from "../../store";
import { editorBridge } from "../../editorBridge";
import { Row, useTreeOpen } from "./primitives";

export function ObjectGroup({ name, items, conn, schema, depth }: { name: ObjectGroupName; items: DbObject[]; conn: ConnectionView; schema: string; depth: number }) {
  const [open, setOpen] = useTreeOpen(treeKey.group(conn.id, schema, name), groupOpenByDefault(name));
  const [limit, setLimit] = useState(GROUP_PAGE);
  const [filter, setFilter] = useState("");
  // An object revealed by ⌘P search is rendered even past the page.
  const focus = useStore((s) => (s.treeFocus?.startsWith(`o|${conn.id}|${schema}|`) ? s.treeFocus : null));
  const { shown, matched } = useMemo(
    () => pageObjects(items, filter, limit, focus ? (o) => treeKey.object(conn.id, o.schema, o.name) === focus : undefined),
    [items, filter, limit, focus, conn.id],
  );
  const big = items.length > GROUP_PAGE;
  const rest = matched - Math.min(limit, matched);
  return (
    <div>
      <Row
        depth={depth}
        expanded={open}
        icon={groupIcon(name)}
        label={<span className="text-[11.5px] font-medium uppercase tracking-wide text-muted">{groupLabel(name, conn.config.kind)}</span>}
        meta={filter ? `${matched}/${items.length}` : items.length}
        onClick={() => setOpen(!open)}
      />
      {open && big && (
        <div className="py-0.5 pr-1" style={{ paddingLeft: 22 + (depth + 1) * 14 }}>
          <input
            className="field h-6 py-0 text-[12px]"
            placeholder={`Filter ${items.length.toLocaleString()} ${name.toLowerCase()}…`}
            aria-label={`Filter ${name.toLowerCase()} in ${schema}`}
            value={filter}
            onChange={(e) => {
              setFilter(e.target.value);
              setLimit(GROUP_PAGE);
            }}
            onKeyDown={(e) => e.key === "Escape" && setFilter("")}
          />
        </div>
      )}
      {open && shown.map((o) => <ObjectNode key={`${o.kind}:${o.name}`} obj={o} conn={conn} depth={depth + 1} />)}
      {open && items.length === 0 && (
        <div className="py-1 text-[12px] text-muted" style={{ paddingLeft: 26 + (depth + 1) * 14 }}>
          No {name.toLowerCase()}
        </div>
      )}
      {open && filter && matched === 0 && <div className="py-1 text-[12px] text-muted" style={{ paddingLeft: 26 + (depth + 1) * 14 }}>No match</div>}
      {open && rest > 0 && (
        <div className="flex items-center gap-2 py-0.5 text-[12px]" style={{ paddingLeft: 26 + (depth + 1) * 14 }}>
          <span className="text-muted">{rest.toLocaleString()} more</span>
          <button className="text-accent hover:underline" onClick={() => setLimit((l) => l + GROUP_PAGE * 5)}>
            Show {Math.min(rest, GROUP_PAGE * 5).toLocaleString()} more
          </button>
          {rest > GROUP_PAGE * 5 && (
            <button className="text-muted hover:text-fg hover:underline" onClick={() => setLimit(Number.MAX_SAFE_INTEGER)} title="Rendering thousands of rows can be slow">
              Show all
            </button>
          )}
        </div>
      )}
    </div>
  );
}

function groupIcon(name: ObjectGroupName): ReactNode {
  switch (name) {
    case "Functions":
    case "Procedures":
      return <FunctionSquare size={12} />;
    case "Sequences":
      return <ListOrdered size={12} />;
    default:
      return null;
  }
}

export function ObjectNode({ obj, conn, depth }: { obj: DbObject; conn: ConnectionView; depth: number }) {
  const columns = useStore((s) => s.columns[`${conn.id}|${obj.schema}|${obj.name}`]);
  const loadColumns = useStore((s) => s.loadColumns);
  const toast = useStore((s) => s.toast);
  // Read at click time: subscribing every row to the active tab re-renders the whole tree on tab switches.
  const activeTab = () => useStore.getState().activeTabId;
  const key = treeKey.object(conn.id, obj.schema, obj.name);
  const focused = useStore((s) => s.treeFocus === key);
  const ref = useRef<HTMLDivElement>(null);
  const [expanded, setExpanded] = useState(false);
  const [loading, setLoading] = useState(false);
  const [menu, setMenu] = useState<{ x: number; y: number } | null>(null);
  const [colMenu, setColMenu] = useState<{ x: number; y: number; column: string } | null>(null);
  const isRelation = isRelationKind(obj.kind);

  // Revealed from search: scroll into view, highlight briefly.
  useEffect(() => {
    if (!focused) return;
    const el = ref.current;
    el?.scrollIntoView({ block: "center" });
    (el?.querySelector('[role="treeitem"]') as HTMLElement | null)?.focus({ preventScroll: true });
    const t = setTimeout(() => {
      if (useStore.getState().treeFocus === key) useStore.setState({ treeFocus: null });
    }, 2500);
    return () => clearTimeout(t);
  }, [focused, key]);

  const toggle = async () => {
    if (!isRelation) return;
    const next = !expanded;
    setExpanded(next);
    if (next && !columns) {
      setLoading(true);
      try {
        await loadColumns(conn.id, obj.schema, obj.name);
      } catch (e) {
        toast(toError(e).message, "error");
      } finally {
        setLoading(false);
      }
    }
  };

  const icon =
    obj.kind === "view" || obj.kind === "materialized_view" ? (
      <Eye size={13} />
    ) : isRelation ? (
      <Table2 size={13} />
    ) : obj.kind === "sequence" ? (
      <ListOrdered size={13} />
    ) : (
      <FunctionSquare size={13} />
    );

  return (
    <div ref={ref} className={focused ? "rounded-md ring-1 ring-accent/60" : undefined}>
      <Row
        depth={depth}
        expanded={isRelation ? expanded : undefined}
        loading={loading}
        icon={icon}
        label={obj.name}
        title={obj.comment}
        active={focused}
        meta={obj.row_estimate != null && obj.row_estimate > 0 ? formatCount(obj.row_estimate) : undefined}
        onClick={toggle}
        onDoubleClick={() => editorBridge.insert(activeTab(), quoteIdent(conn.config.kind, obj.name))}
        onContextMenu={(e) => {
          e.preventDefault();
          setMenu({ x: e.clientX, y: e.clientY });
        }}
        actions={
          <>
            <button
              className="icon-btn h-6 w-6"
              title="Copy table path"
              aria-label="Copy table path"
              onClick={(e) => {
                e.stopPropagation();
                void copyText(tablePath(conn.config.kind, obj.schema, obj.name), "table path");
              }}
            >
              <ClipboardCopy size={12} />
            </button>
            {isRelation && (
              <button
                className="icon-btn h-6 w-6"
                title="Insert column names into the editor"
                aria-label="Insert column names into the editor"
                onClick={(e) => {
                  e.stopPropagation();
                  void insertColumns(conn, obj);
                }}
              >
                <Columns3 size={12} />
              </button>
            )}
            {isRelation && (
              <button
                className="icon-btn h-6 w-6"
                title="Select top 100 rows"
                aria-label="Select top 100 rows"
                onClick={(e) => {
                  e.stopPropagation();
                  selectTop(conn, obj);
                }}
              >
                <Play size={12} />
              </button>
            )}
          </>
        }
      />
      {menu && <ObjectMenu conn={conn} obj={obj} at={menu} onClose={() => setMenu(null)} onSelectTop={() => selectTop(conn, obj)} />}
      {colMenu && <ColumnMenu conn={conn} obj={obj} column={colMenu.column} at={colMenu} onClose={() => setColMenu(null)} />}
      {expanded && columns && columns.length > 1 && (
        <Row
          depth={depth + 1}
          icon={<Columns3 size={12} className="text-accent" />}
          label={<span className="text-[12px] text-accent">Insert all {columns.length} columns</span>}
          title="Insert every column name, comma-separated, at the editor cursor"
          onClick={() => insertText(columnList(conn.config.kind, columns.map((c) => c.name)))}
        />
      )}
      {expanded &&
        columns?.map((c) => (
          <Row
            key={c.name}
            depth={depth + 1}
            icon={c.is_primary_key ? <KeyRound size={12} className="text-warning" /> : <Columns3 size={12} />}
            label={
              <span>
                {c.name}
                <span className="ml-1.5 font-mono text-[11px] text-muted">
                  {c.data_type}
                  {c.nullable ? "" : " not null"}
                </span>
              </span>
            }
            title={c.comment ?? "Double-click to insert"}
            onDoubleClick={() => editorBridge.insert(activeTab(), quoteIdent(conn.config.kind, c.name))}
            onContextMenu={(e) => {
              e.preventDefault();
              setColMenu({ x: e.clientX, y: e.clientY, column: c.name });
            }}
            actions={
              <>
                <button
                  className="icon-btn h-6 w-6"
                  title="Insert column name into the editor"
                  aria-label={`Insert ${c.name} into the editor`}
                  onClick={(e) => {
                    e.stopPropagation();
                    insertText(quoteIdent(conn.config.kind, c.name));
                  }}
                >
                  <TextCursorInput size={12} />
                </button>
                <button
                  className="icon-btn h-6 w-6"
                  title="Copy column name"
                  aria-label={`Copy ${c.name}`}
                  onClick={(e) => {
                    e.stopPropagation();
                    void copyText(c.name, "column name");
                  }}
                >
                  <ClipboardCopy size={12} />
                </button>
              </>
            }
          />
        ))}
    </div>
  );
}
