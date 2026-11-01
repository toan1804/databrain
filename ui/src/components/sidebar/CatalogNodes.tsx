import { useCallback, useEffect, useMemo, useState } from "react";
import { useEvent } from "../../lib/hooks";
import { traitsOf } from "../../lib/sourceTraits";
import { ClipboardCopy, Database, Library, RefreshCw } from "lucide-react";
import { toError } from "../../lib/api";
import type { ConnectionView, SchemaInfo } from "../../lib/types";
import { GROUP_PAGE, RESULTS_SCHEMA_ID, groupObjects, groupOpenByDefault, schemaLabel, schemaPath, treeKey } from "../../lib/catalog";
import { CatalogMenu, SchemaMenu, copyText } from "../CatalogMenus";
import { useStore } from "../../store";
import { ObjectGroup, ObjectNode } from "./ObjectNodes";
import { Row, useTreeOpen } from "./primitives";

/**
 * Top level of three-level engines: Databricks catalog, Snowflake database,
 * BigQuery project, DuckDB database. Its schemas come with it (`schemas`) or
 * are listed when it opens (cache first).
 */
export function CatalogNode({ conn, name, schemas: given, defaultOpen, filtered = false }: { conn: ConnectionView; name: string; schemas?: SchemaInfo[]; defaultOpen: boolean; filtered?: boolean }) {
  const [expanded, setExpanded] = useTreeOpen(treeKey.catalog(conn.id, name), defaultOpen);
  const listed = useStore((s) => s.catalogSchemas[`${conn.id}|${name}`]);
  const loadCatalogSchemas = useStore((s) => s.loadCatalogSchemas);
  const toast = useStore((s) => s.toast);
  const schemas = given ?? listed;
  const [loading, setLoading] = useState(false);
  const noun = ((n) => n.charAt(0).toUpperCase() + n.slice(1))(traitsOf(conn.config.kind).catalog_noun);
  const [menu, setMenu] = useState<{ x: number; y: number } | null>(null);
  const load = useCallback(
    async (force = false) => {
      setLoading(true);
      try {
        await loadCatalogSchemas(conn.id, name, force);
      } catch (e) {
        toast(`${name}: ${toError(e).message}`, "error");
      } finally {
        setLoading(false);
      }
    },
    [conn.id, name, loadCatalogSchemas, toast],
  );
  const openIfNeeded = useEvent(() => {
    if (!given && !loading) void load();
  });
  useEffect(() => {
    if (expanded) openIfNeeded();
  }, [expanded, openIfNeeded]);
  const sorted = useMemo(
    () => (schemas ? [...schemas].sort((a, b) => Number(b.is_default) - Number(a.is_default) || schemaLabel(a).localeCompare(schemaLabel(b))) : undefined),
    [schemas],
  );
  return (
    <div>
      <Row
        depth={1}
        expanded={expanded}
        loading={loading && !schemas}
        icon={<Library size={13} />}
        label={name}
        title={`${noun} ${name}`}
        meta={schemas?.length}
        onClick={() => setExpanded(!expanded)}
        onContextMenu={(e) => {
          e.preventDefault();
          setMenu({ x: e.clientX, y: e.clientY });
        }}
        actions={
          <>
            <button
              className="icon-btn h-6 w-6"
              title={`Copy ${noun.toLowerCase()} name`}
              aria-label={`Copy ${noun.toLowerCase()} name`}
              onClick={(e) => {
                e.stopPropagation();
                void copyText(name, `${noun.toLowerCase()} name`);
              }}
            >
              <ClipboardCopy size={12} />
            </button>
            {!given && (
              <button
                className="icon-btn h-6 w-6"
                title="Refresh schemas"
                aria-label={`Refresh schemas of ${name}`}
                onClick={(e) => {
                  e.stopPropagation();
                  void load(true);
                }}
              >
                <RefreshCw size={12} />
              </button>
            )}
          </>
        }
      />
      {menu && <CatalogMenu conn={conn} catalog={name} at={menu} onClose={() => setMenu(null)} />}
      {expanded &&
        sorted?.map((s) => <SchemaNode key={s.name} conn={conn} schema={s} depth={2} defaultOpen={!filtered && (s.is_default || sorted.length === 1)} />)}
      {expanded && sorted?.length === 0 && <div className="py-1 text-[12px] text-muted" style={{ paddingLeft: 26 + 2 * 14 }}>No schemas</div>}
    </div>
  );
}

export function SchemaNode({ conn, schema: info, depth, defaultOpen }: { conn: ConnectionView; schema: SchemaInfo; depth: number; defaultOpen: boolean }) {
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

  const openIfNeeded = useEvent(() => {
    if (!objects && !loading) void load();
  });
  useEffect(() => {
    if (expanded) openIfNeeded();
  }, [expanded, openIfNeeded]);

  // Every folder the engine has, also empty ones; DuckDB's attached files and
  // outputs are views only, so they keep the plain list.
  const virtual = conn.config.kind === "duckdb" && (schema.endsWith(".files") || schema === RESULTS_SCHEMA_ID);
  const groups = useMemo(() => (objects ? groupObjects(objects, virtual ? undefined : conn.config.kind) : []), [objects, virtual, conn.config.kind]);
  // Only tables (or only views): no folder row, unless the list needs paging/filtering.
  const flat = groups.length === 1 && groupOpenByDefault(groups[0][0]) && groups[0][1].length <= GROUP_PAGE;

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
      {expanded && objects?.length === 0 && virtual && <div className="py-1 text-[12px] text-muted" style={{ paddingLeft: 26 + (depth + 1) * 14 }}>Empty</div>}
    </div>
  );
}
