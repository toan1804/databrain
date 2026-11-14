import { useEffect, useMemo, useRef, useState } from "react";
import { save as saveDialog } from "@tauri-apps/plugin-dialog";
import { Download, Loader2, Square } from "lucide-react";
import { useExports } from "../../exportsStore";
import { ExportCard } from "../ExportsTray";
import { api, toError } from "../../lib/api";
import type { ExportFormat, ResultInfo, ViewSpec } from "../../lib/types";
import { emptyView, formatCount } from "../../lib/util";
import { useStore } from "../../store";
import { Modal } from "../ui";

const FORMATS: { value: ExportFormat; label: string; ext: string }[] = [
  { value: "csv", label: "CSV", ext: "csv" },
  { value: "tsv", label: "TSV", ext: "tsv" },
  { value: "json", label: "JSON (array)", ext: "json" },
  { value: "ndjson", label: "JSON Lines", ext: "ndjson" },
  { value: "markdown", label: "Markdown table", ext: "md" },
  { value: "sql_insert", label: "SQL INSERT statements", ext: "sql" },
  { value: "parquet", label: "Parquet (zstd)", ext: "parquet" },
  { value: "xlsx", label: "Excel workbook", ext: "xlsx" },
];

export function ExportDialog({
  info,
  view,
  viewRows,
  dialect,
  defaultName,
  onClose,
}: {
  info: ResultInfo;
  view: ViewSpec;
  viewRows: number;
  dialect: string | undefined;
  defaultName: string;
  onClose: () => void;
}) {
  const toast = useStore((s) => s.toast);
  const [format, setFormat] = useState<ExportFormat>("csv");
  const [header, setHeader] = useState(true);
  const [excelSafe, setExcelSafe] = useState(false);
  const [scope, setScope] = useState<"view" | "fetched" | "all">("view");
  const useView = scope === "view";
  // A full export runs in the background (exports store); the dialog shows it
  // until it ends or the user sends it to the background.
  const [bgId, setBgId] = useState<string | null>(null);
  const bg = useExports((s) => (bgId ? s.items.find((x) => x.id === bgId) : undefined));
  const [now, setNow] = useState(Date.now());
  useEffect(() => {
    if (!bgId) return;
    useExports.getState().setInDialog(bgId);
    const t = setInterval(() => setNow(Date.now()), 1000);
    return () => {
      clearInterval(t);
      useExports.getState().setInDialog(null);
    };
  }, [bgId]);
  // Finished while the dialog is open: say so and close; stopped: close.
  const ended = useRef(false);
  useEffect(() => {
    if (!bgId || ended.current) return;
    if (!bg || bg.status === "done") {
      ended.current = true;
      if (bg) toast(`Exported ${formatCount(bg.rows)} rows to ${bg.fileName}`, "success");
      onClose();
    }
  }, [bgId, bg, onClose, toast]);
  const [table, setTable] = useState("my_table");
  const [busy, setBusy] = useState(false);
  const fmt = FORMATS.find((f) => f.value === format)!;
  const filtered = view.filters.length > 0 || !!view.quick_filter || view.sort.length > 0;
  const fileBase = useMemo(() => defaultName.replace(/[^\w.-]+/g, "_") || "result", [defaultName]);

  const run = async () => {
    const path = await saveDialog({
      defaultPath: `${fileBase}.${fmt.ext}`,
      filters: [{ name: fmt.label, extensions: [fmt.ext] }],
    });
    if (typeof path !== "string") return;
    const options = { format, header, table_name: table || "my_table", dialect: (dialect as never) ?? null, excel_safe: excelSafe };
    if (scope === "all") {
      setBgId(await useExports.getState().start({ resultId: info.id, options, path, source: defaultName }));
      return;
    }
    setBusy(true);
    try {
      const n = await api.exportResult(info.id, useView ? view : emptyView(), options, path);
      toast(`Exported ${formatCount(n)} rows to ${path.split(/[\\/]/).pop()}`, "success");
      onClose();
    } catch (e) {
      toast(toError(e).message, "error");
    } finally {
      setBusy(false);
    }
  };
  const running = bg?.status === "running";
  const failed = bg?.status === "failed";
  // Closing the dialog keeps a running export going (shown in the tray).
  const close = () => {
    if (failed) useExports.getState().dismiss(bg.id);
    onClose();
  };

  if (bg) {
    return (
      <Modal
        title="Export results"
        onClose={close}
        width={440}
        footer={
          running ? (
            <>
              <button className="btn-ghost mr-auto" onClick={() => useExports.getState().cancel(bg.id)} disabled={bg.stopping}>
                <Square size={12} /> Stop
              </button>
              <button className="btn-primary" onClick={onClose} title="Keep exporting while you work; progress shows at the bottom right">
                Run in background
              </button>
            </>
          ) : (
            <>
              <button className="btn-ghost" onClick={() => (setBgId(null), useExports.getState().dismiss(bg.id))}>
                Back
              </button>
              <button className="btn-primary" onClick={close}>
                Close
              </button>
            </>
          )
        }
      >
        <div className="space-y-3">
          <ExportCard x={bg} now={now} bare />
          {running && (
            <p className="text-[11.5px] text-muted">
              The query runs again (on its own session, or on the tab's when a transaction is open there) and every row goes straight to the file. You can keep working: choose "Run in background" and the
              progress stays at the bottom right, with Stop and Show file.
            </p>
          )}
        </div>
      </Modal>
    );
  }

  return (
    <Modal
      title="Export results"
      onClose={onClose}
      width={440}
      footer={
        <>
          <button className="btn-ghost" onClick={onClose}>
            Cancel
          </button>
          <button className="btn-primary" onClick={run} disabled={busy}>
            {busy ? <Loader2 size={14} className="animate-spin" /> : <Download size={14} />} Export…
          </button>
        </>
      }
    >
      <div className="space-y-4">
        <div>
          <div className="mb-1.5 text-[11.5px] font-medium text-muted">Format</div>
          <div className="grid grid-cols-2 gap-1.5">
            {FORMATS.map((f) => (
              <button
                key={f.value}
                onClick={() => setFormat(f.value)}
                aria-pressed={format === f.value}
                className={`rounded-lg border px-3 py-2 text-left text-[12.5px] ${
                  format === f.value ? "border-accent bg-accent/10" : "border-line hover:bg-hover"
                }`}
              >
                {f.label}
              </button>
            ))}
          </div>
        </div>
        {(format === "csv" || format === "tsv") && (
          <label className="flex items-center gap-2 text-[13px]">
            <input type="checkbox" checked={header} onChange={(e) => setHeader(e.target.checked)} /> Include header row
          </label>
        )}
        {(format === "csv" || format === "tsv") && (
          <label className="flex items-center gap-2 text-[13px]" title="Text starting with = + - @ gets a leading ' so Excel doesn't run it as a formula. Numbers, also negative ones, stay as they are.">
            <input type="checkbox" checked={excelSafe} onChange={(e) => setExcelSafe(e.target.checked)} /> Safe to open in Excel (text is never run as a formula)
          </label>
        )}
        {format === "sql_insert" && (
          <div>
            <div className="mb-1 text-[11.5px] font-medium text-muted">Table name</div>
            <input className="field font-mono" value={table} onChange={(e) => setTable(e.target.value)} />
          </div>
        )}
        <div className="space-y-1.5 text-[13px]">
          <label className="flex items-center gap-2">
            <input type="radio" checked={scope === "view"} disabled={busy} onChange={() => setScope("view")} />
            Current view {filtered ? "(filtered/sorted)" : ""} — {formatCount(viewRows)} rows
            {filtered && info.on_disk ? " (of the rows loaded in the grid)" : ""}
          </label>
          <label className="flex items-center gap-2">
            <input type="radio" checked={scope === "fetched"} disabled={busy} onChange={() => setScope("fetched")} />
            All fetched rows — {formatCount(info.total_rows)} rows
          </label>
          <label className="flex items-start gap-2">
            <input type="radio" className="mt-1" checked={scope === "all"} disabled={busy} onChange={() => setScope("all")} />
            <span>
              All rows of the query, ignoring the row limit
              <span className="block text-[11.5px] text-muted">Runs the query again and writes every row straight to the file (filters and sorting of the grid don't apply).</span>
            </span>
          </label>
          {info.truncated && scope !== "all" && (
            <p className="text-[11.5px] text-warning">
              This result was limited to {formatCount(info.total_rows)} rows. Choose "All rows of the query" to export everything.
            </p>
          )}
        </div>
      </div>
    </Modal>
  );
}
