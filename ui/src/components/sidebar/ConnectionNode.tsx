import { useCallback, useEffect, useMemo, useState } from "react";
import { useEvent } from "../../lib/hooks";
import {
  AlertCircle,
  Eye,
  FileCode2,
  LogIn,
  NotebookPen,
  Sparkles,
  ListChecks,
  MoreHorizontal,
  Pencil,
  RefreshCw,
  Search,
  Trash2,
  Unplug,
  WifiOff,
} from "lucide-react";
import { toError } from "../../lib/api";
import type { ConnectionView } from "../../lib/types";
import { groupSchemas, topLevelNoun, treeKey, visibleSchemas, visibleCatalogs, cacheAge, isThreeLevel } from "../../lib/catalog";
import { useAi } from "../../aiStore";
import { MoveToFolderItems, dragProps } from "../FolderTree";
import { useStore } from "../../store";
import { ConnDot, EnvBadge, MenuItem, MenuSeparator, Popover } from "../ui";
import { kbd } from "../../lib/platform";
import { CatalogNode, SchemaNode } from "./CatalogNodes";
import { Row, useTreeOpen } from "./primitives";

/** Connections whose explorer was opened this session (cache shown, then checked). */
const explorerOpened = new Set<string>();

export function ConnectionNode({ conn }: { conn: ConnectionView }) {
  const schemas = useStore((s) => s.schemas[conn.id]);
  const catalogList = useStore((s) => (isThreeLevel(conn.config) ? s.catalogs[conn.id] : undefined));
  const status = useStore((s) => s.explorer[conn.id]);
  const openExplorer = useStore((s) => s.openExplorer);
  const disconnect = useStore((s) => s.disconnect);
  const openDialog = useStore((s) => s.openConnectionDialog);
  const newTab = useStore((s) => s.newTab);
  const toast = useStore((s) => s.toast);
  const activeConn = useStore((s) => s.tabs.find((t) => t.id === s.activeTabId)?.connection_id);
  const [expanded, setExpanded] = useTreeOpen(treeKey.conn(conn.id), false);
  const [loading, setLoading] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [menu, setMenu] = useState<{ x: number; y: number } | null>(null);

  const loaded = !!(catalogList ?? schemas);
  const load = useCallback(
    async (force = false) => {
      explorerOpened.add(conn.id);
      setLoading(true);
      setError(null);
      try {
        await openExplorer(conn.id, force);
      } catch (e) {
        explorerOpened.delete(conn.id);
        setError(toError(e).message);
        setExpanded(false);
        toast(`${conn.name}: ${toError(e).message}`, "error");
      } finally {
        setLoading(false);
      }
    },
    [conn.id, conn.name, openExplorer, toast, setExpanded],
  );

  // Open on expand (by click or by a search reveal): the cache shows at once, the server is asked meanwhile.
  // Only on expand: a failed load isn't retried until the user expands again.
  const openIfNeeded = useEvent(() => {
    if ((!explorerOpened.has(conn.id) || !loaded) && !loading && !error) void load();
  });
  useEffect(() => {
    if (expanded) openIfNeeded();
  }, [expanded, loaded, openIfNeeded]);

  const toggle = () => {
    if (!expanded) setError(null);
    setExpanded(!expanded);
  };

  const openMenu = (e: React.MouseEvent) => {
    e.preventDefault();
    e.stopPropagation();
    setMenu({ x: e.clientX, y: e.clientY });
  };

  const chosen = useStore((s) => s.schemaFilter[conn.id]);
  // Schema of an object revealed by ⌘P search (`o|conn|schema|name`).
  const revealed = useStore((s) => (chosen && s.treeFocus?.startsWith(`o|${conn.id}|`) ? s.treeFocus.slice(conn.id.length + 3, s.treeFocus.lastIndexOf("|")) : null));
  // Stays listed after the highlight fades (until the schema choice changes).
  const [sticky, setSticky] = useState<string | null>(null);
  useEffect(() => {
    if (revealed) setSticky(revealed);
  }, [revealed]);
  useEffect(() => setSticky(null), [chosen]);
  const extra = revealed ?? sticky;
  // Catalog-first (three-level engines): catalogs are listed before any schema.
  const shownCatalogs = useMemo(() => (catalogList ? visibleCatalogs(catalogList, chosen, extra) : null), [catalogList, chosen, extra]);
  const shownSchemas = useMemo(() => (!catalogList && schemas ? visibleSchemas(schemas, chosen, extra) : undefined), [catalogList, schemas, chosen, extra]);
  const groups = useMemo(() => (shownSchemas ? groupSchemas(shownSchemas) : null), [shownSchemas]);
  const filtered = catalogList
    ? !!shownCatalogs && shownCatalogs.length < catalogList.length
    : !!schemas && !!shownSchemas && shownSchemas.length < schemas.length;
  const topNoun = topLevelNoun(conn.config, true);
  const pickSchemas = () => {
    setMenu(null);
    useStore.setState({ schemaPicker: conn.id });
  };
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
            {!error && status?.offline && expanded && <WifiOff size={12} className="text-warning" aria-label="Offline: showing cached explorer" />}
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
          <MenuItem icon={<ListChecks size={13} />} label={`Choose ${topNoun}…`} hint={chosen ? `${chosen.length} shown` : undefined} onClick={pickSchemas} />
          {chosen && (
            <MenuItem icon={<Eye size={13} />} label={`Show all ${topNoun}`} onClick={() => { setMenu(null); useStore.getState().setSchemaFilter(conn.id, null); }} />
          )}
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
          <MenuItem icon={<Trash2 size={13} />} label="Delete connection…" hint={kbd("⌘⌫")} danger onClick={() => { setMenu(null); void useStore.getState().deleteConnection(conn.id); }} />
        </Popover>
      )}
      {expanded && status?.offline && (
        <Row
          depth={1}
          icon={<WifiOff size={12} className="text-warning" />}
          label={
            <span className="text-[11.5px] text-muted">
              Offline · cached {cacheAge(status.checkedAt)} · <span className="text-accent">retry</span>
            </span>
          }
          title={`Cannot reach the server, showing what was cached (tables, columns; completion works too).\n${status.offline}`}
          onClick={() => void load()}
        />
      )}
      {expanded && filtered && (
        <Row
          depth={1}
          icon={<ListChecks size={12} className="text-accent" />}
          label={
            <span className="text-[11.5px] text-muted">
              {catalogList
                ? `${shownCatalogs!.length} of ${catalogList.length}`
                : groups
                  ? `${groups.length} of ${groupSchemas(schemas!)?.length ?? 0}`
                  : `${shownSchemas!.length} of ${schemas!.length}`}{" "}
              {topNoun} · <span className="text-accent">change</span>
            </span>
          }
          title={`Choose which ${topNoun} are listed`}
          onClick={() => useStore.setState({ schemaPicker: conn.id })}
        />
      )}
      {expanded &&
        shownCatalogs &&
        shownCatalogs.map((c) => (
          <CatalogNode
            key={c.name}
            conn={conn}
            name={c.name}
            filtered={!!chosen}
            // The Results connection exists to query outputs: show them.
            defaultOpen={!chosen && (c.is_default || shownCatalogs.length === 1 || (c.name === "results" && conn.config.options?.databrain_results === "1"))}
          />
        ))}
      {expanded &&
        shownSchemas &&
        (groups
          ? groups.map((g) => (
              <CatalogNode
                key={g.name}
                conn={conn}
                name={g.name}
                schemas={g.schemas}
                filtered={!!chosen}
                defaultOpen={!chosen && (g.isDefault || groups.length === 1 || (g.name === "results" && conn.config.options?.databrain_results === "1"))}
              />
            ))
          : shownSchemas.map((s) => (
              <SchemaNode key={s.name} conn={conn} schema={s} depth={1} defaultOpen={!chosen && (s.is_default || shownSchemas.length === 1)} />
            )))}
      {expanded && !catalogList && schemas?.length === 0 && <div className="py-1 pl-10 text-[12px] text-muted">No schemas</div>}
      {expanded && catalogList?.length === 0 && <div className="py-1 pl-10 text-[12px] text-muted">No {topNoun}</div>}
    </div>
  );
}
