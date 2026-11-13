import { open as openDialog } from "@tauri-apps/plugin-dialog";
import { api, toError } from "../../lib/api";
import { DEFAULT_POLICY } from "../connection/parts";
import { askExcelSheets } from "../ExcelSheetDialog";
import { useStore } from "../../store";

// ------------------------------------------------------------------ connections

/**
 * Open a local file with DuckDB: reuse/create a "Local files" connection and
 * run a scan query. `importIt`: the query reads every row (no `LIMIT`), the
 * tab shows the first ones, and "Load into…" opens on the result to copy the
 * whole file into a table (it streams every row when the result was cut).
 */
let reading = false;

export async function queryLocalFile(importIt = false) {
  // Reading a big workbook takes a moment: a second click must not start another.
  if (reading) return;
  reading = true;
  try {
    await openLocalFile(importIt);
  } finally {
    reading = false;
  }
}

async function openLocalFile(importIt: boolean) {
  const st = useStore.getState();
  const picked = await openDialog({
    multiple: false,
    directory: false,
    filters: [{ name: "Data files", extensions: ["csv", "tsv", "txt", "parquet", "json", "ndjson", "jsonl", "xlsx", "gz"] }],
  });
  if (typeof picked !== "string") return;
  try {
    // Workbooks with several sheets: first sheet with data (default) or all of them.
    let allSheets = false;
    if (/\.xlsx$/i.test(picked)) {
      const sheets = await api.excelSheets(picked);
      const used = sheets.filter((s) => s.range).length;
      if (used === 0) {
        st.toast("Every sheet in this workbook is empty", "info");
        return;
      }
      if (used > 1) {
        const pick = await askExcelSheets(picked, sheets);
        if (pick === null) return;
        allSheets = pick;
        if (allSheets) st.toast(`Reading ${used} sheets…`, "info");
      }
    }
    // An import reads every row: no LIMIT. The tab shows the first rows (the
    // row limit); "Load into…" then streams every row of the file.
    const sql = await api.fileScanSql(picked, allSheets, !importIt);
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
    if (importIt) {
      useStore.getState().setLoadIntoOnResult(id, true);
      void useStore.getState().runSql(id, conn.id, sql, 0);
    } else void useStore.getState().runTab(id, "all", { doc: sql, selFrom: 0, selTo: 0, cursor: 0 });
  } catch (e) {
    st.toast(toError(e).message, "error");
  }
}
