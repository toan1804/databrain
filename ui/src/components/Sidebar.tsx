import { useCallback, useEffect, useMemo, useRef, useState, type ReactNode } from "react";
import {
  AlertCircle,
  Ban,
  Bookmark,
  CheckCircle2,
  ChevronDown,
  ChevronRight,
  ClipboardCopy,
  Columns3,
  Database,
  Eye,
  FileCode2,
  FileSearch,
  FolderPlus,
  FunctionSquare,
  LogIn,
  NotebookPen,
  Sparkles,
  History,
  KeyRound,
  Layers,
  Library,
  Loader2,
  MoreHorizontal,
  Pencil,
  Play,
  Plug,
  Plus,
  RefreshCw,
  Search,
  Table2,
  TextCursorInput,
  Trash2,
  Unplug,
} from "lucide-react";
import { open as openDialog } from "@tauri-apps/plugin-dialog";
import { api, toError } from "../lib/api";
import type { ConnectionView, DbObject, HistoryEntry, NotebookSummary, SavedQuery, SchemaInfo } from "../lib/types";
import { columnList, groupOf, groupSchemas, schemaLabel, schemaPath, tablePath, treeKey, type CatalogGroup } from "../lib/catalog";
import { CatalogSearch, selectTop } from "./CatalogSearch";
import { CatalogMenu, ColumnMenu, ObjectMenu, SchemaMenu, copyText, insertColumns, insertText } from "./CatalogMenus";
import { formatCount, formatDuration, relativeTime, sqlPreview, quoteIdent } from "../lib/util";
import { useAi } from "../aiStore";
import { DEFAULT_POLICY } from "./ConnectionDialog";
import { FolderTree, MoveToFolderItems, createFolder, dragProps } from "./FolderTree";
import { OutputsPanel } from "./OutputsPanel";
import { openOutput } from "../outputs";
import { useStore, type SidebarPanel } from "../store";
import { editorBridge } from "../editorBridge";
import { ConnDot, EnvBadge, MenuItem, MenuSeparator, Popover } from "./ui";

export function Sidebar() {
  const panel = useStore((s) => s.sidebarPanel);
  return (
    <div className="flex h-full min-w-0">
      <ActivityRail />
      <div className="flex min-w-0 flex-1 flex-col border-r border-line bg-panel">
        {panel === "connections" && <ConnectionsPanel />}
        {panel === "saved" && <SavedPanel />}
        {panel === "notebooks" && <NotebooksPanel />}
        {panel === "outputs" && <OutputsPanel />}
        {panel === "history" && <HistoryPanel />}
      </div>
    </div>
  );
}

function ActivityRail() {
  const panel = useStore((s) => s.sidebarPanel);
  const setPanel = useStore((s) => s.setSidebarPanel);
  const aiOpen = useAi((s) => s.open);
  const setAiOpen = useAi((s) => s.setOpen);
  const item = (p: SidebarPanel, icon: ReactNode, label: string) => (
    <button
      key={p}
      title={label}
      aria-label={label}
      aria-pressed={panel === p}
      onClick={() => setPanel(p)}
      className={`relative flex h-10 w-10 items-center justify-center rounded-lg transition-colors ${
        panel === p ? "bg-hover text-fg" : "text-muted hover:text-fg"
      }`}
    >
      {panel === p && <span className="absolute left-[-6px] h-5 w-[3px] rounded-r bg-accent" />}
      {icon}
    </button>
  );
  return (
    <div className="flex w-12 shrink-0 flex-col items-center gap-1 border-r border-line bg-bg pt-2">
      {item("connections", <Database size={18} />, "Connections")}
      {item("saved", <Bookmark size={18} />, "Saved queries")}
      {item("notebooks", <NotebookPen size={18} />, "Notebooks")}
      {item("outputs", <Layers size={18} />, "Outputs")}
      {item("history", <History size={18} />, "History")}
      <div className="flex-1" />
      <button
        title="AI assistant (⌘L)"
        aria-label="AI assistant"
        aria-pressed={aiOpen}
        onClick={() => setAiOpen(!aiOpen)}
        className={`mb-2 flex h-10 w-10 items-center justify-center rounded-lg ${aiOpen ? "bg-accent/15 text-accent" : "text-muted hover:text-fg"}`}
      >
        <Sparkles size={18} />
      </button>
    </div>
  );
}

function PanelHeader({ title, children }: { title: string; children?: ReactNode }) {
  return (
    <div className="flex h-10 shrink-0 items-center justify-between px-3">
      <span className="text-[11px] font-semibold uppercase tracking-wider text-muted">{title}</span>
      <div className="flex items-center gap-0.5">{children}</div>
    </div>
  );
}

function SearchBox({ value, onChange, placeholder }: { value: string; onChange: (v: string) => void; placeholder: string }) {
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

// ------------------------------------------------------------------ connections

/** Open a local file with DuckDB: reuse/create a "Local files" connection and run a scan query. */
export async function queryLocalFile() {
  const st = useStore.getState();
  const picked = await openDialog({
    multiple: false,
    directory: false,
    filters: [{ name: "Data files", extensions: ["csv", "tsv", "txt", "parquet", "json", "ndjson", "jsonl", "xlsx", "gz"] }],
  });
  if (typeof picked !== "string") return;
  try {
    const sql = await api.fileScanSql(picked);
    let conn = st.connections.find((c) => c.config.kind === "duckdb" && !c.config.file_path);
    if (!conn) {
      const saved = await api.saveConnection({
        id: "",
        name: "Local files (DuckDB)",
        config: { kind: "duckdb", auth: { method: "none" }, ssl_mode: "disable", read_only: false, options: {} },
        color: "#fbbf24",
        env: "none",
        has_secret: false,
        ai_policy: DEFAULT_POLICY,
        created_at: 0,
        updated_at: 0,
      }, null);
      await st.refreshConnections();
      conn = useStore.getState().connections.find((c) => c.id === saved.id);
    }
    if (!conn) return;
    const id = useStore.getState().newTab({ title: picked.split(/[\\/]/).pop() ?? "file", sql, connection_id: conn.id });
    void useStore.getState().runTab(id, "all", { doc: sql, selFrom: 0, selTo: 0, cursor: 0 });
  } catch (e) {
    st.toast(toError(e).message, "error");
  }
}

function ConnectionsPanel() {
  const connections = useStore((s) => s.connections);
  const openDialog = useStore((s) => s.openConnectionDialog);
  const query = useStore((s) => s.catalogSearch.query.trim().toLowerCase());
  const named = query ? connections.filter((c) => c.name.toLowerCase().includes(query)) : connections;
  const tree = (items: ConnectionView[], filtering: boolean) => (
    <div role="tree" aria-label="Connections">
      <FolderTree
        kind="connections"
        items={items}
        itemId={(c) => c.id}
        itemFolder={(c) => c.folder_id}
        filtering={filtering}
        renderItem={(c) => <ConnectionNode conn={c} />}
        newItem={{ label: "New connection here", create: (folderId) => openDialog(null, folderId) }}
      />
    </div>
  );
  return (
    <>
      <PanelHeader title="Connections">
        <button className="icon-btn" title="Query a local file (CSV, Parquet, JSON, Excel) with DuckDB" aria-label="Query a local file" onClick={() => void queryLocalFile()}>
          <FileSearch size={14} />
        </button>
        <button className="icon-btn" title="New folder" aria-label="New folder" onClick={() => void createFolder("connections")}>
          <FolderPlus size={14} />
        </button>
        <button className="icon-btn" title="New connection" aria-label="New connection" onClick={() => openDialog(null)}>
          <Plus size={15} />
        </button>
      </PanelHeader>
      {connections.length === 0 ? (
        <div className="px-3 py-8 text-center text-[12.5px] text-muted">
          <Database size={28} className="mx-auto mb-3 opacity-50" />
          No connections yet.
          <button className="btn-primary mx-auto mt-3" onClick={() => openDialog(null)}>
            <Plus size={14} /> Add connection
          </button>
        </div>
      ) : (
        <CatalogSearch tree={tree(connections, false)} connectionsTree={named.length ? tree(named, true) : null} />
      )}
    </>
  );
}

function Row({
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
function useTreeOpen(key: string, fallback: boolean): [boolean, (open: boolean) => void] {
  const stored = useStore((s) => s.treeOpen[key]);
  const setTreeOpen = useStore((s) => s.setTreeOpen);
  return [stored ?? fallback, useCallback((open: boolean) => setTreeOpen(key, open), [key, setTreeOpen])];
}

function ConnectionNode({ conn }: { conn: ConnectionView }) {
  const schemas = useStore((s) => s.schemas[conn.id]);
  const loadSchemas = useStore((s) => s.loadSchemas);
  const disconnect = useStore((s) => s.disconnect);
  const openDialog = useStore((s) => s.openConnectionDialog);
  const newTab = useStore((s) => s.newTab);
  const toast = useStore((s) => s.toast);
  const activeConn = useStore((s) => s.tabs.find((t) => t.id === s.activeTabId)?.connection_id);
  const [expanded, setExpanded] = useTreeOpen(treeKey.conn(conn.id), false);
  const [loading, setLoading] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [menu, setMenu] = useState<{ x: number; y: number } | null>(null);

  const load = useCallback(
    async (force = false) => {
      setLoading(true);
      setError(null);
      try {
        await loadSchemas(conn.id, force);
      } catch (e) {
        setError(toError(e).message);
        setExpanded(false);
        toast(`${conn.name}: ${toError(e).message}`, "error");
      } finally {
        setLoading(false);
      }
    },
    [conn.id, conn.name, loadSchemas, toast, setExpanded],
  );

  // Load on expand (by click or by a search reveal).
  useEffect(() => {
    if (expanded && !schemas && !loading && !error) void load();
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [expanded, schemas]);

  const toggle = () => {
    if (!expanded) setError(null);
    setExpanded(!expanded);
  };

  const openMenu = (e: React.MouseEvent) => {
    e.preventDefault();
    e.stopPropagation();
    setMenu({ x: e.clientX, y: e.clientY });
  };

  const catalogs = useMemo(() => (schemas ? groupSchemas(schemas) : null), [schemas]);
  const interactive = ["oauth_browser", "device_code", "external_browser"].includes(conn.config.auth.method);
  return (
    <div {...dragProps("connections", conn.id)}>
      <Row
        depth={0}
        expanded={expanded}
        loading={loading}
        active={activeConn === conn.id}
        icon={<ConnDot color={conn.color} connected={conn.connected} />}
        title={error ?? undefined}
        label={
          <span className="flex items-center gap-1.5">
            <span className="truncate font-medium">{conn.name}</span>
            <EnvBadge env={conn.env} />
            {conn.config.read_only && <span className="text-[10px] text-muted">RO</span>}
            {error && <AlertCircle size={12} className="text-danger" />}
          </span>
        }
        onClick={toggle}
        onContextMenu={openMenu}
        onDelete={() => void useStore.getState().deleteConnection(conn.id)}
        actions={
          <>
            <button
              className="icon-btn h-6 w-6"
              title="Find table in this connection"
              aria-label="Find table in this connection"
              onClick={(e) => {
                e.stopPropagation();
                useStore.getState().openCatalogSearch(conn.id);
              }}
            >
              <Search size={13} />
            </button>
            <button
              className="icon-btn h-6 w-6"
              title="New query"
              aria-label="New query"
              onClick={(e) => {
                e.stopPropagation();
                newTab({ connection_id: conn.id });
              }}
            >
              <FileCode2 size={13} />
            </button>
            <button className="icon-btn h-6 w-6" aria-label="More" onClick={openMenu}>
              <MoreHorizontal size={13} />
            </button>
          </>
        }
      />
      {menu && (
        <Popover x={menu.x} y={menu.y} onClose={() => setMenu(null)} className="max-h-[75vh] w-60 overflow-auto">
          <MenuItem icon={<FileCode2 size={13} />} label="New query" onClick={() => { setMenu(null); newTab({ connection_id: conn.id }); }} />
          <MenuItem icon={<Search size={13} />} label="Find table…" onClick={() => { setMenu(null); useStore.getState().openCatalogSearch(conn.id); }} />
          <MenuItem icon={<RefreshCw size={13} />} label={conn.connected ? "Refresh" : "Connect"} onClick={() => { setMenu(null); setExpanded(true); void load(true); }} />
          <MenuItem icon={<NotebookPen size={13} />} label="New notebook" onClick={() => { setMenu(null); void useStore.getState().newNotebook(conn.id); }} />
          <MenuItem icon={<Pencil size={13} />} label="Edit connection" onClick={() => { setMenu(null); openDialog(conn); }} />
          {interactive && (
            <MenuItem icon={<LogIn size={13} />} label="Sign in again" onClick={() => { setMenu(null); void useStore.getState().signInConnection(conn.id); }} />
          )}
          <MenuItem
            icon={<Sparkles size={13} />}
            label="Index for AI"
            onClick={() => {
              setMenu(null);
              void useAi.getState().indexKnowledge(conn.id);
              useAi.getState().setOpen(true, "knowledge");
            }}
          />
          {conn.connected && (
            <MenuItem icon={<Unplug size={13} />} label="Disconnect" onClick={() => { setMenu(null); setExpanded(false); void disconnect(conn.id); }} />
          )}
          <MenuSeparator />
          <MoveToFolderItems kind="connections" id={conn.id} current={conn.folder_id} onDone={() => setMenu(null)} />
          <MenuSeparator />
          <MenuItem icon={<Trash2 size={13} />} label="Delete connection…" hint="⌘⌫" danger onClick={() => { setMenu(null); void useStore.getState().deleteConnection(conn.id); }} />
        </Popover>
      )}
      {expanded &&
        schemas &&
        (catalogs
          ? catalogs.map((g) => (
              <CatalogNode key={g.name} conn={conn} group={g} defaultOpen={g.isDefault || catalogs.length === 1} />
            ))
          : schemas.map((s) => (
              <SchemaNode key={s.name} conn={conn} schema={s} depth={1} defaultOpen={s.is_default || schemas.length === 1} />
            )))}
      {expanded && schemas?.length === 0 && <div className="py-1 pl-10 text-[12px] text-muted">No schemas</div>}
    </div>
  );
}

/** Top level of three-level engines: Databricks catalog, Snowflake database, BigQuery project, DuckDB database. */
function CatalogNode({ conn, group, defaultOpen }: { conn: ConnectionView; group: CatalogGroup; defaultOpen: boolean }) {
  const [expanded, setExpanded] = useTreeOpen(treeKey.catalog(conn.id, group.name), defaultOpen);
  const noun = conn.config.kind === "bigquery" ? "Project" : conn.config.kind === "databricks" ? "Catalog" : "Database";
  const [menu, setMenu] = useState<{ x: number; y: number } | null>(null);
  return (
    <div>
      <Row
        depth={1}
        expanded={expanded}
        icon={<Library size={13} />}
        label={group.name}
        title={`${noun} ${group.name}`}
        meta={group.schemas.length}
        onClick={() => setExpanded(!expanded)}
        onContextMenu={(e) => {
          e.preventDefault();
          setMenu({ x: e.clientX, y: e.clientY });
        }}
        actions={
          <button
            className="icon-btn h-6 w-6"
            title={`Copy ${noun.toLowerCase()} name`}
            aria-label={`Copy ${noun.toLowerCase()} name`}
            onClick={(e) => {
              e.stopPropagation();
              void copyText(group.name, `${noun.toLowerCase()} name`);
            }}
          >
            <ClipboardCopy size={12} />
          </button>
        }
      />
      {menu && <CatalogMenu conn={conn} catalog={group.name} at={menu} onClose={() => setMenu(null)} />}
      {expanded &&
        group.schemas.map((s) => (
          <SchemaNode key={s.name} conn={conn} schema={s} depth={2} defaultOpen={s.is_default || group.schemas.length === 1} />
        ))}
    </div>
  );
}

function SchemaNode({ conn, schema: info, depth, defaultOpen }: { conn: ConnectionView; schema: SchemaInfo; depth: number; defaultOpen: boolean }) {
  const schema = info.name;
  const objects = useStore((s) => s.objects[`${conn.id}|${schema}`]);
  const loadObjects = useStore((s) => s.loadObjects);
  const toast = useStore((s) => s.toast);
  const [expanded, setExpanded] = useTreeOpen(treeKey.schema(conn.id, schema), defaultOpen);
  const [loading, setLoading] = useState(false);
  const [menu, setMenu] = useState<{ x: number; y: number } | null>(null);

  const load = useCallback(
    async (force = false) => {
      setLoading(true);
      try {
        await loadObjects(conn.id, schema, force);
      } catch (e) {
        toast(toError(e).message, "error");
      } finally {
        setLoading(false);
      }
    },
    [conn.id, schema, loadObjects, toast],
  );

  useEffect(() => {
    if (expanded && !objects && !loading) void load();
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [expanded]);

  const groups = useMemo(() => {
    const g: Record<string, DbObject[]> = { Tables: [], Views: [], Routines: [] };
    for (const o of objects ?? []) g[groupOf(o.kind)].push(o);
    return Object.entries(g).filter(([, v]) => v.length > 0);
  }, [objects]);
  // A single kind (e.g. only tables) is listed directly, without a group row.
  const flat = groups.length === 1 && groups[0][0] !== "Routines";

  return (
    <div>
      <Row
        depth={depth}
        expanded={expanded}
        loading={loading}
        icon={<Database size={13} />}
        label={schemaLabel(info)}
        title={schema}
        meta={objects ? objects.length : undefined}
        onClick={() => setExpanded(!expanded)}
        onContextMenu={(e) => {
          e.preventDefault();
          setMenu({ x: e.clientX, y: e.clientY });
        }}
        actions={
          <>
            <button
              className="icon-btn h-6 w-6"
              title="Copy schema path"
              aria-label="Copy schema path"
              onClick={(e) => {
                e.stopPropagation();
                void copyText(schemaPath(conn.config.kind, schema), "schema path");
              }}
            >
              <ClipboardCopy size={12} />
            </button>
            <button
              className="icon-btn h-6 w-6"
              title="Refresh"
              aria-label="Refresh schema"
              onClick={(e) => {
                e.stopPropagation();
                void load(true);
              }}
            >
              <RefreshCw size={12} />
            </button>
          </>
        }
      />
      {menu && <SchemaMenu conn={conn} schema={info} at={menu} onClose={() => setMenu(null)} onRefresh={() => void load(true)} />}
      {expanded &&
        (flat
          ? groups[0][1].map((o) => <ObjectNode key={`${o.kind}:${o.name}`} obj={o} conn={conn} depth={depth + 1} />)
          : groups.map(([name, items]) => <ObjectGroup key={name} name={name} items={items} conn={conn} schema={schema} depth={depth + 1} />))}
      {expanded && objects?.length === 0 && <div className="py-1 text-[12px] text-muted" style={{ paddingLeft: 26 + (depth + 1) * 14 }}>Empty</div>}
    </div>
  );
}

function ObjectGroup({ name, items, conn, schema, depth }: { name: string; items: DbObject[]; conn: ConnectionView; schema: string; depth: number }) {
  const [open, setOpen] = useTreeOpen(treeKey.group(conn.id, schema, name), name !== "Routines");
  return (
    <div>
      <Row
        depth={depth}
        expanded={open}
        icon={null}
        label={<span className="text-[11.5px] font-medium uppercase tracking-wide text-muted">{name}</span>}
        meta={items.length}
        onClick={() => setOpen(!open)}
      />
      {open && items.map((o) => <ObjectNode key={`${o.kind}:${o.name}`} obj={o} conn={conn} depth={depth + 1} />)}
    </div>
  );
}

function ObjectNode({ obj, conn, depth }: { obj: DbObject; conn: ConnectionView; depth: number }) {
  const columns = useStore((s) => s.columns[`${conn.id}|${obj.schema}|${obj.name}`]);
  const loadColumns = useStore((s) => s.loadColumns);
  const activeTabId = useStore((s) => s.activeTabId);
  const toast = useStore((s) => s.toast);
  const key = treeKey.object(conn.id, obj.schema, obj.name);
  const focused = useStore((s) => s.treeFocus === key);
  const ref = useRef<HTMLDivElement>(null);
  const [expanded, setExpanded] = useState(false);
  const [loading, setLoading] = useState(false);
  const [menu, setMenu] = useState<{ x: number; y: number } | null>(null);
  const [colMenu, setColMenu] = useState<{ x: number; y: number; column: string } | null>(null);
  const isRelation = obj.kind !== "function" && obj.kind !== "procedure";

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
        meta={obj.row_estimate !== undefined && obj.row_estimate > 0 ? formatCount(obj.row_estimate) : undefined}
        onClick={toggle}
        onDoubleClick={() => editorBridge.insert(activeTabId, quoteIdent(conn.config.kind, obj.name))}
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
            onDoubleClick={() => editorBridge.insert(activeTabId, quoteIdent(conn.config.kind, c.name))}
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

// ------------------------------------------------------------------ saved queries

function SavedPanel() {
  const saved = useStore((s) => s.savedQueries);
  const connections = useStore((s) => s.connections);
  const openSaved = useStore((s) => s.openSavedQuery);
  const refresh = useStore((s) => s.refreshSavedQueries);
  const toast = useStore((s) => s.toast);
  const [search, setSearch] = useState("");

  const shown = useMemo(() => {
    const q = search.toLowerCase().trim();
    if (!q) return saved;
    return saved.filter(
      (s) =>
        s.name.toLowerCase().includes(q) ||
        s.sql.toLowerCase().includes(q) ||
        s.tags.some((t) => t.toLowerCase().includes(q)) ||
        (s.description ?? "").toLowerCase().includes(q),
    );
  }, [saved, search]);

  const remove = (q: SavedQuery) =>
    useStore.getState().askConfirm({
      title: `Delete "${q.name}"?`,
      reasons: ["This saved query will be permanently deleted."],
      confirmLabel: "Delete",
      onConfirm: async () => {
        try {
          await api.deleteSavedQuery(q.id);
          await refresh();
          useStore.getState().tabs
            .filter((t) => t.saved_query_id === q.id)
            .forEach((t) => useStore.getState().updateTab(t.id, { saved_query_id: null }));
        } catch (e) {
          toast(toError(e).message, "error");
        }
      },
    });

  const renderQuery = (q: SavedQuery) => {
    const conn = connections.find((c) => c.id === q.connection_id);
    return (
      <div
        {...dragProps("queries", q.id)}
        role="button"
        tabIndex={0}
        onClick={() => openSaved(q)}
        onKeyDown={(e) => e.key === "Enter" && openSaved(q)}
        className="group mb-0.5 cursor-pointer rounded-md px-2 py-1.5 hover:bg-hover"
      >
        <div className="flex items-center gap-1.5">
          <FileCode2 size={13} className="shrink-0 text-muted" />
          <span className="min-w-0 flex-1 truncate font-medium">{q.name}</span>
          {q.ai_example && (
            <span title="Used as an AI example">
              <Sparkles size={11} className="text-accent" />
            </span>
          )}
          <button
            className="icon-btn hidden h-5 w-5 group-hover:flex"
            aria-label={`Delete ${q.name}`}
            onClick={(e) => {
              e.stopPropagation();
              remove(q);
            }}
          >
            <Trash2 size={12} />
          </button>
        </div>
        <div className="mt-0.5 truncate pl-5 font-mono text-[11px] text-muted">{sqlPreview(q.sql, 60)}</div>
        <div className="mt-1 flex flex-wrap items-center gap-1 pl-5">
          {conn && (
            <span className="flex items-center gap-1 text-[10.5px] text-muted">
              <ConnDot color={conn.color} /> {conn.name}
            </span>
          )}
          {q.tags.map((t) => (
            <span key={t} className="rounded bg-panel-2 px-1.5 text-[10.5px] text-muted">
              {t}
            </span>
          ))}
        </div>
      </div>
    );
  };

  return (
    <>
      <PanelHeader title="Saved queries">
        <button className="icon-btn" title="New folder" aria-label="New folder" onClick={() => void createFolder("queries")}>
          <FolderPlus size={14} />
        </button>
      </PanelHeader>
      <SearchBox value={search} onChange={setSearch} placeholder="Search saved queries" />
      <div className="min-h-0 flex-1 overflow-auto px-1.5 pb-3">
        {shown.length === 0 && (
          <div className="px-3 py-8 text-center text-[12.5px] text-muted">
            <Bookmark size={26} className="mx-auto mb-3 opacity-50" />
            {saved.length === 0 ? (
              <>
                No saved queries. Press <span className="kbd">⌘S</span> in an editor tab to save one.
              </>
            ) : (
              "No matches"
            )}
          </div>
        )}
        <FolderTree kind="queries" items={shown} itemId={(q) => q.id} itemFolder={(q) => q.folder_id} filtering={!!search.trim()} renderItem={renderQuery} />
      </div>
    </>
  );
}

// ------------------------------------------------------------------ notebooks

function NotebooksPanel() {
  const notebooks = useStore((s) => s.notebooks);
  const connections = useStore((s) => s.connections);
  const openNotebook = useStore((s) => s.openNotebook);
  const newNotebook = useStore((s) => s.newNotebook);
  const refresh = useStore((s) => s.refreshNotebooks);
  const toast = useStore((s) => s.toast);
  const [search, setSearch] = useState("");
  const shown = notebooks.filter((n) => n.name.toLowerCase().includes(search.toLowerCase().trim()));

  useEffect(() => {
    void refresh().catch(() => {});
  }, [refresh]);

  const remove = (n: NotebookSummary) =>
    useStore.getState().askConfirm({
      title: `Delete notebook "${n.name}"?`,
      reasons: ["All cells of this notebook are deleted."],
      confirmLabel: "Delete",
      onConfirm: async () => {
        try {
          const st = useStore.getState();
          st.tabs.filter((t) => t.notebook_id === n.id).forEach((t) => st.closeTab(t.id));
          await api.deleteNotebook(n.id);
          await refresh();
        } catch (e) {
          toast(toError(e).message, "error");
        }
      },
    });

  return (
    <>
      <PanelHeader title="Notebooks">
        <button className="icon-btn" title="New folder" aria-label="New folder" onClick={() => void createFolder("notebooks")}>
          <FolderPlus size={14} />
        </button>
        <button className="icon-btn" title="New notebook" aria-label="New notebook" onClick={() => void newNotebook()}>
          <Plus size={15} />
        </button>
      </PanelHeader>
      {notebooks.length > 4 && <SearchBox value={search} onChange={setSearch} placeholder="Search notebooks" />}
      <div className="min-h-0 flex-1 overflow-auto px-1.5 pb-3">
        {notebooks.length === 0 && (
          <div className="px-3 py-8 text-center text-[12.5px] text-muted">
            <NotebookPen size={26} className="mx-auto mb-3 opacity-50" />
            Notebooks mix SQL and notes. Cells run on the notebook's connection and share one session.
            <button className="btn-primary mx-auto mt-3" onClick={() => void newNotebook()}>
              <Plus size={14} /> New notebook
            </button>
          </div>
        )}
        <FolderTree
          kind="notebooks"
          items={shown}
          itemId={(n) => n.id}
          itemFolder={(n) => n.folder_id}
          filtering={!!search.trim()}
          renderItem={(n) => {
            const conn = connections.find((c) => c.id === n.connection_id);
            return (
              <div
                {...dragProps("notebooks", n.id)}
                role="button"
                tabIndex={0}
                onClick={() => openNotebook(n.id, n.name)}
                onKeyDown={(e) => e.key === "Enter" && openNotebook(n.id, n.name)}
                className="group mb-0.5 cursor-pointer rounded-md px-2 py-1.5 hover:bg-hover"
              >
                <div className="flex items-center gap-1.5">
                  <NotebookPen size={13} className="shrink-0 text-muted" />
                  <span className="min-w-0 flex-1 truncate font-medium">{n.name}</span>
                  <button
                    className="icon-btn hidden h-5 w-5 group-hover:flex"
                    aria-label={`Delete ${n.name}`}
                    onClick={(e) => {
                      e.stopPropagation();
                      remove(n);
                    }}
                  >
                    <Trash2 size={12} />
                  </button>
                </div>
                <div className="mt-0.5 flex items-center gap-2 pl-5 text-[10.5px] text-muted">
                  {conn && (
                    <span className="flex items-center gap-1">
                      <ConnDot color={conn.color} /> {conn.name}
                    </span>
                  )}
                  <span>{n.cell_count} cells</span>
                  <span>{relativeTime(n.updated_at)}</span>
                </div>
              </div>
            );
          }}
        />
      </div>
    </>
  );
}

// ------------------------------------------------------------------ history

/** Output produced by a history entry: opens it if it is still available. */
function HistoryOutput({ handle }: { handle: string }) {
  const o = useStore((s) => s.outputs.find((x) => x.handle === handle));
  const available = !!o && o.state !== "evicted";
  return (
    <button
      className={`ml-auto shrink-0 rounded border px-1 font-mono text-[10px] ${available ? "border-accent/40 text-accent hover:bg-accent/10" : "border-line text-muted"}`}
      title={available ? `Open output ${handle} (no re-run)` : `Output ${handle} is no longer in memory`}
      disabled={!available}
      onClick={(e) => {
        e.stopPropagation();
        if (o) void openOutput(o);
      }}
    >
      {o?.name ?? handle}
    </button>
  );
}

function HistoryPanel() {
  const version = useStore((s) => s.historyVersion);
  const newTab = useStore((s) => s.newTab);
  const toast = useStore((s) => s.toast);
  const [search, setSearch] = useState("");
  const [items, setItems] = useState<HistoryEntry[]>([]);
  const [loading, setLoading] = useState(false);

  const load = useCallback(async () => {
    setLoading(true);
    try {
      setItems(await api.listHistory({ search: search || null, limit: 300 }));
    } catch (e) {
      toast(toError(e).message, "error");
    } finally {
      setLoading(false);
    }
  }, [search, toast]);

  useEffect(() => {
    const t = setTimeout(() => void load(), 150);
    return () => clearTimeout(t);
  }, [load, version]);

  const clear = () =>
    useStore.getState().askConfirm({
      title: "Clear query history?",
      reasons: ["All history entries will be deleted."],
      confirmLabel: "Clear history",
      onConfirm: async () => {
        await api.clearHistory();
        await load();
      },
    });

  return (
    <>
      <PanelHeader title="History">
        {loading && <Loader2 size={13} className="mr-1 animate-spin text-muted" />}
        <button className="icon-btn" title="Clear history" aria-label="Clear history" onClick={clear} disabled={items.length === 0}>
          <Trash2 size={14} />
        </button>
      </PanelHeader>
      <SearchBox value={search} onChange={setSearch} placeholder="Search history" />
      <div className="min-h-0 flex-1 overflow-auto px-1.5 pb-3">
        {items.length === 0 && !loading && (
          <div className="px-3 py-8 text-center text-[12.5px] text-muted">
            <History size={26} className="mx-auto mb-3 opacity-50" />
            {search ? "No matches" : "Queries you run appear here."}
          </div>
        )}
        {items.map((h) => (
          <div
            key={h.id}
            role="button"
            tabIndex={0}
            title={h.error ?? h.sql}
            onClick={() => newTab({ sql: h.sql, connection_id: h.connection_id ?? undefined })}
            onKeyDown={(e) => e.key === "Enter" && newTab({ sql: h.sql, connection_id: h.connection_id ?? undefined })}
            className="mb-0.5 cursor-pointer rounded-md px-2 py-1.5 hover:bg-hover"
          >
            <div className="flex items-start gap-1.5">
              {h.status === "success" ? (
                <CheckCircle2 size={12} className="mt-0.5 shrink-0 text-success" />
              ) : h.status === "error" ? (
                <AlertCircle size={12} className="mt-0.5 shrink-0 text-danger" />
              ) : (
                <Ban size={12} className="mt-0.5 shrink-0 text-muted" />
              )}
              <span className="line-clamp-2 min-w-0 flex-1 break-all font-mono text-[11.5px]">{sqlPreview(h.sql, 140)}</span>
            </div>
            <div className="mt-1 flex items-center gap-2 pl-[18px] text-[10.5px] text-muted">
              <span>{relativeTime(h.started_at)}</span>
              <span>{formatDuration(h.duration_ms)}</span>
              {h.rows !== null && <span>{formatCount(h.rows)} rows</span>}
              {h.connection_name && (
                <span className="flex min-w-0 items-center gap-1 truncate">
                  <Plug size={10} />
                  {h.connection_name}
                </span>
              )}
              {h.output_handle && <HistoryOutput handle={h.output_handle} />}
            </div>
          </div>
        ))}
      </div>
    </>
  );
}

