// One side of "Compare outputs": an output, a table read from any SQL
// connection, or a data file (CSV, Parquet, JSON, Excel) read by DuckDB.
// A table or file becomes an output when read, so the rest of the compare
// works the same for all three.
import { useEffect, useMemo, useState } from "react";
import { open as openDialog } from "@tauri-apps/plugin-dialog";
import { FileUp, Loader2, Table2 } from "lucide-react";
import { api, toError } from "../lib/api";
import { traitsOf } from "../lib/sourceTraits";
import type { CompareSource, DbObject, ExcelSheet, OutputInfo, SchemaInfo } from "../lib/types";
import { formatCount } from "../lib/util";
import { formatAge, outputLabel } from "../outputs";
import { useStore } from "../store";

const DATA_FILES = [{ name: "Data files", extensions: ["csv", "tsv", "txt", "parquet", "json", "ndjson", "jsonl", "xlsx"] }];

/** Where a side was read from, in a few words. */
export function sourceLabel(src: CompareSource | null | undefined, connectionName: (id: string) => string | undefined): string | null {
  if (!src) return null;
  if (src.kind === "table") return `${connectionName(src.connection_id) ?? "?"} · ${src.schema}.${src.table}`;
  if (src.kind === "file") {
    const base = src.path.split(/[\\/]/).pop() ?? src.path;
    return src.sheet ? `${base} · ${src.sheet}` : base;
  }
  return null;
}

export function CompareSide({
  label,
  side,
  value,
  source,
  outputs,
  onPick,
}: {
  label: string;
  side: "before" | "after";
  /** Handle of the chosen output. */
  value: string;
  /** Where it was read from (a table or file), or what to read. */
  source: CompareSource | null;
  outputs: OutputInfo[];
  onPick: (handle: string, source: CompareSource | null) => void;
}) {
  const connections = useStore((s) => s.connections);
  const toast = useStore((s) => s.toast);
  const sqlConns = useMemo(() => connections.filter((c) => traitsOf(c.config.kind).language === "sql"), [connections]);
  // A table without a connection yet (from the explorer: the same table elsewhere) opens the table form.
  const [mode, setMode] = useState<"output" | "table" | "file">(source?.kind === "table" && !source.connection_id && !value ? "table" : "output");
  /** A table or file the dialog is reading for this side (from the explorer). */
  const reading = !value && mode === "output" && !!source && source.kind !== "output";
  const [busy, setBusy] = useState(false);
  const [conn, setConn] = useState(source?.kind === "table" ? source.connection_id : "");
  const [schema, setSchema] = useState(source?.kind === "table" ? source.schema : "");
  const [table, setTable] = useState(source?.kind === "table" ? source.table : "");
  const [schemas, setSchemas] = useState<SchemaInfo[] | null>(null);
  const [objects, setObjects] = useState<DbObject[]>([]);
  const [file, setFile] = useState<{ path: string; sheets: ExcelSheet[]; sheet: string } | null>(null);

  useEffect(() => {
    setSchemas(null);
    if (mode !== "table" || !conn) return;
    let alive = true;
    api.listSchemas(conn).then(
      (s) => {
        if (!alive) return;
        setSchemas(s);
        setSchema((cur) => (cur && s.some((x) => x.name === cur) ? cur : (s.find((x) => x.is_default) ?? s[0])?.name ?? ""));
      },
      (e) => alive && toast(toError(e).message, "error"),
    );
    return () => {
      alive = false;
    };
  }, [mode, conn, toast]);
  useEffect(() => {
    setObjects([]);
    if (mode !== "table" || !conn || !schema) return;
    let alive = true;
    api.listObjects(conn, schema).then(
      (o) => alive && setObjects(o.filter((x) => x.kind !== "function" && x.kind !== "procedure" && x.kind !== "sequence")),
      () => {},
    );
    return () => {
      alive = false;
    };
  }, [mode, conn, schema]);

  const read = async (src: CompareSource) => {
    setBusy(true);
    try {
      const o = await api.compareLoadSource(src, "dialog", side);
      await useStore.getState().refreshOutputs();
      onPick(o.handle, src);
      setMode("output");
      setFile(null);
      if (o.truncated) toast(`Read the first ${formatCount(o.rows)} rows only; rows past them show as only on the other side.`, "info");
    } catch (e) {
      toast(toError(e).message, "error");
    } finally {
      setBusy(false);
    }
  };
  const chooseFile = async () => {
    const path = await openDialog({ multiple: false, directory: false, filters: DATA_FILES });
    if (typeof path !== "string") return setMode("output");
    if (/\.xlsx$/i.test(path)) {
      const sheets = (await api.excelSheets(path).catch(() => [] as ExcelSheet[])).filter((s) => !s.hidden && s.range);
      if (sheets.length > 1) return setFile({ path, sheets, sheet: sheets[0].name });
    }
    await read({ kind: "file", path, sheet: null });
  };

  const connName = (id: string) => connections.find((c) => c.id === id)?.name;
  const from = sourceLabel(source, connName);
  const tableOk = conn && schema && table.trim();
  return (
    <div>
      <span className="mb-1 block text-[11.5px] font-medium text-muted">{label}</span>
      <div className="flex items-center gap-1.5">
        <select
          className="field min-w-0 flex-1"
          aria-label={`${label} output`}
          value={mode === "output" ? value : `__${mode}`}
          disabled={busy}
          onChange={(e) => {
            const v = e.target.value;
            if (v === "__table") return setMode("table");
            if (v === "__file") return (setMode("file"), void chooseFile());
            setMode("output");
            onPick(v, null);
          }}
        >
          {!value && mode === "output" && <option value="">{reading ? `Reading ${sourceLabel(source, (id) => connections.find((c) => c.id === id)?.name)}…` : "Choose…"}</option>}
          {outputs.map((o) => (
            <option key={o.handle} value={o.handle}>
              {outputLabel(o)} ({o.handle}) — {o.connection_name}, {formatCount(o.rows)} rows, {formatAge(o.created_at)}
            </option>
          ))}
          <option disabled>──────────</option>
          <option value="__table">From a table…</option>
          <option value="__file">From a file (CSV, Excel, Parquet, JSON)…</option>
        </select>
        {(busy || reading) && <Loader2 size={14} className="shrink-0 animate-spin text-muted" />}
      </div>
      {mode === "output" && from && value && (
        <div className="mt-0.5 flex items-center gap-1 text-[11px] text-muted">
          {source?.kind === "file" ? <FileUp size={11} /> : <Table2 size={11} />} Read from {from}; Compare again reads it again.
        </div>
      )}
      {mode === "table" && (
        <div className="mt-1.5 grid grid-cols-[1fr_1fr_1.4fr_auto] gap-1.5">
          <select className="field h-7 py-0" aria-label={`${label} connection`} value={conn} onChange={(e) => setConn(e.target.value)}>
            <option value="">Connection…</option>
            {sqlConns.map((c) => (
              <option key={c.id} value={c.id}>
                {c.name}
              </option>
            ))}
          </select>
          <select className="field h-7 py-0" aria-label={`${label} schema`} value={schema} disabled={!schemas} onChange={(e) => setSchema(e.target.value)}>
            {!schemas && <option value={schema}>{schema || "Schema"}</option>}
            {schemas?.map((s) => (
              <option key={s.name} value={s.name}>
                {s.name}
              </option>
            ))}
          </select>
          <input
            className="field h-7 py-0 font-mono text-[11.5px]"
            aria-label={`${label} table`}
            placeholder="Table"
            list={`compare-${side}-tables`}
            value={table}
            onChange={(e) => setTable(e.target.value)}
            onKeyDown={(e) => e.key === "Enter" && tableOk && void read({ kind: "table", connection_id: conn, schema, table: table.trim() })}
          />
          <datalist id={`compare-${side}-tables`}>
            {objects.map((o) => (
              <option key={o.name} value={o.name} />
            ))}
          </datalist>
          <button className="btn-primary h-7 py-0" disabled={!tableOk || busy} onClick={() => void read({ kind: "table", connection_id: conn, schema, table: table.trim() })}>
            Read
          </button>
        </div>
      )}
      {mode === "file" && file && (
        <div className="mt-1.5 flex items-center gap-1.5">
          <span className="min-w-0 truncate text-[11.5px]" title={file.path}>
            {file.path.split(/[\\/]/).pop()}
          </span>
          <select className="field h-7 w-40 py-0" aria-label={`${label} sheet`} value={file.sheet} onChange={(e) => setFile({ ...file, sheet: e.target.value })}>
            {file.sheets.map((s) => (
              <option key={s.name} value={s.name}>
                {s.name}
              </option>
            ))}
          </select>
          <button className="btn-primary h-7 py-0" disabled={busy} onClick={() => void read({ kind: "file", path: file.path, sheet: file.sheet })}>
            Read
          </button>
        </div>
      )}
    </div>
  );
}
