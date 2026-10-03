// Copy / insert actions for catalog objects (explorer tree and catalog search).
import { writeText } from "@tauri-apps/plugin-clipboard-manager";
import { ClipboardCopy, Columns3, Play, RefreshCw, TextCursorInput } from "lucide-react";
import { toError } from "../lib/api";
import { isRelationKind, columnList, schemaLabel, schemaPath, splitSchema, tablePath } from "../lib/catalog";
import type { ConnectionView, DbObject, SchemaInfo } from "../lib/types";
import { quoteIdent } from "../lib/util";
import { editorBridge } from "../editorBridge";
import { useStore } from "../store";
import { MenuItem, MenuSeparator, Popover } from "./ui";

export async function copyText(text: string, what: string) {
  try {
    await writeText(text);
    useStore.getState().toast(`Copied ${what}: ${text.length > 60 ? text.slice(0, 59) + "…" : text}`, "success");
  } catch (e) {
    useStore.getState().toast(toError(e).message, "error");
  }
}

/** Insert text at the cursor of the active editor (query tab or notebook cell). */
export function insertText(text: string) {
  const st = useStore.getState();
  const tab = st.tabs.find((t) => t.id === st.activeTabId);
  if (!tab || tab.output_ref) {
    st.toast("Open a query tab to insert into the editor", "info");
    return;
  }
  editorBridge.insert(st.activeTabId, text);
}

/** Load (if needed) and insert/copy a table's column list. */
export async function columnsOf(conn: ConnectionView, obj: DbObject): Promise<string[] | null> {
  try {
    const cols = await useStore.getState().loadColumns(conn.id, obj.schema, obj.name);
    return cols.map((c) => c.name);
  } catch (e) {
    useStore.getState().toast(toError(e).message, "error");
    return null;
  }
}

export async function insertColumns(conn: ConnectionView, obj: DbObject) {
  const cols = await columnsOf(conn, obj);
  if (cols?.length) insertText(columnList(conn.config.kind, cols));
}

type At = { x: number; y: number };

const nounOf = (conn: ConnectionView) => (conn.config.kind === "bigquery" ? "project" : conn.config.kind === "databricks" ? "catalog" : "database");

export function CatalogMenu({ conn, catalog, at, onClose }: { conn: ConnectionView; catalog: string; at: At; onClose: () => void }) {
  const kind = conn.config.kind;
  const noun = nounOf(conn);
  const run = (f: () => void) => () => (onClose(), f());
  return (
    <Popover x={at.x} y={at.y} onClose={onClose} className="w-60">
      <MenuItem icon={<ClipboardCopy size={13} />} label={`Copy ${noun} name`} onClick={run(() => void copyText(catalog, `${noun} name`))} />
      <MenuItem icon={<ClipboardCopy size={13} />} label={`Copy quoted ${noun}`} onClick={run(() => void copyText(quoteIdent(kind, catalog), noun))} />
      <MenuSeparator />
      <MenuItem icon={<TextCursorInput size={13} />} label="Insert into editor" onClick={run(() => insertText(quoteIdent(kind, catalog)))} />
    </Popover>
  );
}

export function SchemaMenu({
  conn,
  schema,
  at,
  onClose,
  onRefresh,
}: {
  conn: ConnectionView;
  schema: SchemaInfo;
  at: At;
  onClose: () => void;
  onRefresh?: () => void;
}) {
  const kind = conn.config.kind;
  const name = schemaLabel(schema);
  const path = schemaPath(kind, schema.name);
  const { catalog } = splitSchema(kind, schema.name, [schema]);
  const run = (f: () => void) => () => (onClose(), f());
  return (
    <Popover x={at.x} y={at.y} onClose={onClose} className="w-64">
      <MenuItem icon={<ClipboardCopy size={13} />} label="Copy schema name" hint={name} onClick={run(() => void copyText(name, "schema name"))} />
      {path !== name && <MenuItem icon={<ClipboardCopy size={13} />} label="Copy schema path" hint={path} onClick={run(() => void copyText(path, "schema path"))} />}
      {catalog && <MenuItem icon={<ClipboardCopy size={13} />} label={`Copy ${nounOf(conn)} name`} hint={catalog} onClick={run(() => void copyText(catalog, `${nounOf(conn)} name`))} />}
      <MenuSeparator />
      <MenuItem icon={<TextCursorInput size={13} />} label="Insert schema path" onClick={run(() => insertText(path))} />
      {onRefresh && (
        <>
          <MenuSeparator />
          <MenuItem icon={<RefreshCw size={13} />} label="Refresh" onClick={run(onRefresh)} />
        </>
      )}
    </Popover>
  );
}

export function ObjectMenu({
  conn,
  obj,
  at,
  onClose,
  onSelectTop,
}: {
  conn: ConnectionView;
  obj: DbObject;
  at: At;
  onClose: () => void;
  onSelectTop?: () => void;
}) {
  const kind = conn.config.kind;
  const known = useStore.getState().schemas[conn.id];
  const { catalog, schema } = splitSchema(kind, obj.schema, known);
  const path = tablePath(kind, obj.schema, obj.name);
  const sPath = schemaPath(kind, obj.schema);
  const isRelation = isRelationKind(obj.kind);
  const run = (f: () => void) => () => (onClose(), f());
  return (
    <Popover x={at.x} y={at.y} onClose={onClose} className="w-72">
      <MenuItem icon={<ClipboardCopy size={13} />} label="Copy table name" hint={obj.name.length > 22 ? undefined : obj.name} onClick={run(() => void copyText(obj.name, "table name"))} />
      <MenuItem icon={<ClipboardCopy size={13} />} label="Copy table path" onClick={run(() => void copyText(path, "table path"))} />
      <MenuItem icon={<ClipboardCopy size={13} />} label="Copy schema name" hint={schema.length > 22 ? undefined : schema} onClick={run(() => void copyText(schema, "schema name"))} />
      {sPath !== schema && <MenuItem icon={<ClipboardCopy size={13} />} label="Copy schema path" onClick={run(() => void copyText(sPath, "schema path"))} />}
      {catalog && (
        <MenuItem
          icon={<ClipboardCopy size={13} />}
          label={`Copy ${nounOf(conn)} name`}
          hint={catalog.length > 22 ? undefined : catalog}
          onClick={run(() => void copyText(catalog, `${nounOf(conn)} name`))}
        />
      )}
      {isRelation && (
        <MenuItem
          icon={<Columns3 size={13} />}
          label="Copy column names"
          onClick={run(async () => {
            const cols = await columnsOf(conn, obj);
            if (cols) void copyText(columnList(kind, cols), `${cols.length} columns`);
          })}
        />
      )}
      <MenuSeparator />
      <MenuItem icon={<TextCursorInput size={13} />} label="Insert table path" onClick={run(() => insertText(path))} />
      {isRelation && <MenuItem icon={<Columns3 size={13} />} label="Insert column names" onClick={run(() => void insertColumns(conn, obj))} />}
      {isRelation && onSelectTop && <MenuItem icon={<Play size={13} />} label="Select top 100 rows" onClick={run(onSelectTop)} />}
    </Popover>
  );
}

export function ColumnMenu({ conn, obj, column, at, onClose }: { conn: ConnectionView; obj: DbObject; column: string; at: At; onClose: () => void }) {
  const kind = conn.config.kind;
  const quoted = quoteIdent(kind, column);
  const qualified = `${quoteIdent(kind, obj.name)}.${quoted}`;
  const run = (f: () => void) => () => (onClose(), f());
  return (
    <Popover x={at.x} y={at.y} onClose={onClose} className="w-60">
      <MenuItem icon={<ClipboardCopy size={13} />} label="Copy column name" onClick={run(() => void copyText(column, "column name"))} />
      <MenuItem icon={<ClipboardCopy size={13} />} label="Copy table.column" onClick={run(() => void copyText(qualified, "column"))} />
      <MenuSeparator />
      <MenuItem icon={<TextCursorInput size={13} />} label="Insert into editor" onClick={run(() => insertText(quoted))} />
      <MenuItem icon={<TextCursorInput size={13} />} label="Insert table.column" onClick={run(() => insertText(qualified))} />
    </Popover>
  );
}
