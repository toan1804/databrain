import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import { useFindInResult } from "../useFindInResult";
import { useAi } from "../../aiStore";
import { ChartView } from "../ChartView";
import { tableKey } from "../../lib/gridViews";
import { planFind } from "../../lib/find";
import { api, isTauri, toError } from "../../lib/api";
import type { ChartSpec, EditStatement, EditTarget, ResultInfo, ResultLink, ViewSpec } from "../../lib/types";
import { editCount, emptyEdits, toChanges, type PendingEdits } from "../../lib/gridEdits";
import { emptyView, formatCount } from "../../lib/util";
import { isQueryTab, useStore, type StatementRun } from "../../store";
import { ResultGrid, type GridHandle } from "../ResultGrid";
import { TransposeDialog, type GridRange } from "../SelectionTools";
import { LoadIntoDialog } from "../LoadIntoDialog";
import { ValuePanel, type FocusedCell } from "../ValuePanel";
import { PinChartDialog } from "../DashboardTab";
import type { ChartKind } from "../../lib/charts";
import { ResultToolbar } from "./ResultToolbar";
import { FindBar } from "./FindBar";
import { CheckingEditBanner, EditingBar, EditPreviewModal } from "./EditBars";
import { ResultFooter } from "./ResultFooter";
import { ColumnMenu } from "./ColumnMenu";
import { ExportDialog } from "./ExportDialog";

export function ResultView({
  stmt,
  info,
  tabId,
  connectionId,
  title,
}: {
  stmt: Pick<StatementRun, "durationMs" | "output">;
  info: ResultInfo;
  tabId: string;
  connectionId?: string | null;
  title?: string;
}) {
  const [mode, setMode] = useState<"grid" | "chart">("grid");
  const tab = useStore((s) => s.tabs.find((t) => t.id === tabId));
  const connId = connectionId !== undefined ? connectionId : tab?.connection_id;
  const conn = useStore((s) => s.connections.find((c) => c.id === connId));
  const send = useAi((s) => s.send);
  const [baseView, setView] = useState<ViewSpec>(emptyView);
  // A sort or filter of a result on disk applies to the rows the grid has
  // loaded (the furthest it was scrolled), counted when the sort or filter starts.
  const [loaded, setLoaded] = useState({ id: info.id, n: 0 });
  const onLoadedRows = useCallback((n: number) => setLoaded((l) => (l.id !== info.id ? { id: info.id, n } : n > l.n ? { id: l.id, n } : l)), [info.id]);
  const narrowed = info.on_disk && (baseView.sort.length > 0 || baseView.filters.length > 0 || !!baseView.quick_filter);
  const [firstRows, setFirstRows] = useState<number | null>(null);
  if (narrowed && firstRows === null) setFirstRows(Math.max(1, loaded.id === info.id ? loaded.n : 0));
  if (!narrowed && firstRows !== null) setFirstRows(null);
  const view = useMemo<ViewSpec>(() => (narrowed && firstRows !== null ? { ...baseView, first_rows: firstRows } : baseView), [baseView, narrowed, firstRows]);
  const [viewRows, setViewRows] = useState(info.total_rows);
  const [windowRows, setWindowRows] = useState<number | null>(null);
  const onViewRows = useCallback((n: number, w: number | null) => {
    setViewRows(n);
    setWindowRows(w);
  }, []);
  const [quick, setQuick] = useState("");
  const quickRef = useRef(quick);
  quickRef.current = quick;
  /** The table the result's query reads (`tableKey`): its quick filter is remembered. */
  const [sourceKey, setSourceKey] = useState<string | null>(null);
  const sourceRef = useRef<string | null>(null);
  const keptQuick = useStore((s) => (sourceKey ? s.quickFilters[sourceKey]?.q : undefined));
  const [findOpen, setFindOpen] = useState(false);
  const [findQuery, setFindQuery] = useState("");
  const [find, setFind] = useState<{ matches: { row: number; col: number }[]; truncated: boolean }>({ matches: [], truncated: false });
  const [findIdx, setFindIdx] = useState(0);
  /** Column the find bar is limited to (null = all columns). */
  const [findScope, setFindScope] = useState<number | null>(null);
  const [wholeWord, setWholeWord] = useState(false);
  const [selRange, setSelRange] = useState<GridRange | null>(null);
  const [transpose, setTranspose] = useState<GridRange | null>(null);
  const columnNames = useMemo(() => info.columns.map((c) => c.name), [info.columns]);
  const plan = useMemo(() => planFind(findQuery, columnNames, findScope), [findQuery, columnNames, findScope]);
  const [headerMenu, setHeaderMenu] = useState<{ col: number; x: number; y: number } | null>(null);
  const [exportOpen, setExportOpen] = useState(false);
  const [loadOpen, setLoadOpen] = useState(false);
  // Importing a file: its result opens "Load into…" once.
  const loadPending = useStore((s) => !!s.loadIntoOnResult[tabId]);
  useEffect(() => {
    if (!loadPending) return;
    useStore.getState().setLoadIntoOnResult(tabId, false);
    setLoadOpen(true);
  }, [loadPending, tabId]);
  const grid = useRef<GridHandle>(null);
  const findInput = useRef<HTMLInputElement>(null);
  const toast = useStore((s) => s.toast);
  // Value panel: the focused cell's whole value (open state kept across results).
  const [valueOpen, setValueOpen] = useState(() => localStorage.getItem("db.valuePanel") === "1");
  useEffect(() => localStorage.setItem("db.valuePanel", valueOpen ? "1" : "0"), [valueOpen]);
  const [focused, setFocused] = useState<FocusedCell | null>(null);
  const [pinning, setPinning] = useState<{ spec: ChartSpec; kind: ChartKind } | null>(null);

  // Editing the result's table: pending changes, previewed and saved as SQL.
  const [target, setTarget] = useState<EditTarget | null>(null);
  const [edits, setEdits] = useState<PendingEdits>(emptyEdits);
  const [preview, setPreview] = useState<EditStatement[] | null>(null);
  const [saving, setSaving] = useState(false);
  useEffect(() => {
    setTarget(null);
    setEdits(emptyEdits());
  }, [info.id]);
  const changes = editCount(edits);

  /** Edit is checking the table (schema, columns, primary key): shown, and Edit waits. */
  const [checkingEdit, setCheckingEdit] = useState(false);
  const startEditing = async () => {
    if (checkingEdit) return;
    setCheckingEdit(true);
    try {
      setTarget(await api.editTarget(info.id));
      setMode("grid");
    } catch (e) {
      toast(toError(e).message, "error");
    } finally {
      setCheckingEdit(false);
    }
  };
  const stopEditing = () => {
    if (changes && !window.confirm(`Discard ${changes} unsaved change${changes === 1 ? "" : "s"}?`)) return;
    setTarget(null);
    setEdits(emptyEdits());
  };
  const showPreview = async () => {
    if (!target) return;
    try {
      setPreview(await api.editPreview(info.id, target, toChanges(edits)));
    } catch (e) {
      toast(toError(e).message, "error");
    }
  };
  /** Follow a foreign key: its rows open in a new query tab and run. */
  const openRelated = async (link: ResultLink, rows: (string | null)[][]) => {
    if (!connId) return;
    try {
      const sql = await api.relatedRowsSql(info.id, link, rows);
      const s = useStore.getState();
      const id = s.newTab({ title: link.table, sql, connection_id: connId });
      void s.runTab(id, "all", { doc: sql, selFrom: 0, selTo: 0, cursor: 0 });
    } catch (e) {
      toast(toError(e).message, "error");
    }
  };
  const saveEdits = async () => {
    if (!target || !changes) return;
    setSaving(true);
    try {
      const r = await api.applyEdits(info.id, toChanges(edits));
      setPreview(null);
      setEdits(emptyEdits());
      toast(
        `${formatCount(r.rows)} row${r.rows === 1 ? "" : "s"} changed${r.in_open_transaction ? " in the open transaction — COMMIT to keep" : ""}`,
        "success",
      );
      // Refresh the result from the database (same run key and session).
      const sql = stmt.output?.sql;
      if (connId && isQueryTab(tab)) void useStore.getState().refreshTxState(tabId, connId);
      if (sql && connId && isQueryTab(tab)) void useStore.getState().runSql(tabId, connId, sql);
      else if (sql && connId && tabId.startsWith("nb:"))
        void useStore.getState().runSql(tabId, connId, sql, 0, tabId.split(":").slice(0, 2).join(":"), stmt.output?.name);
      else toast("Re-run the query to see the saved rows", "info");
    } catch (e) {
      toast(toError(e).message, "error");
    } finally {
      setSaving(false);
    }
  };

  // The result's table, and its remembered quick filter (unless one was typed meanwhile).
  useEffect(() => {
    if (!isTauri()) return;
    let alive = true;
    api
      .resultSource(info.id)
      .then((src) => {
        if (!alive || !src) return;
        const st = useStore.getState();
        const key = tableKey(src, st.schemas[src.connection_id]?.find((s) => s.is_default)?.name);
        sourceRef.current = key;
        setSourceKey(key);
        const kept = st.quickFilters[key]?.q;
        if (kept && quickRef.current === "") {
          setQuick(kept);
          setView((v) => ({ ...v, quick_filter: kept }));
        }
      })
      .catch(() => {});
    return () => {
      alive = false;
    };
  }, [info.id]);

  // Debounced quick filter (remembered for the result's table).
  useEffect(() => {
    const t = setTimeout(() => {
      setView((v) => ({ ...v, quick_filter: quick.trim() || null }));
      if (sourceRef.current) useStore.getState().setQuickFilter(sourceRef.current, quick);
    }, 250);
    return () => clearTimeout(t);
  }, [quick]);

  // Find within the current view (the latest search only).
  useFindInResult({
    open: findOpen,
    plan,
    wholeWord,
    view,
    resultId: info.id,
    onResult: (r) => {
      setFind(r);
      setFindIdx(0);
      if (r.matches[0]) grid.current?.scrollToCell(r.matches[0].col, r.matches[0].row);
    },
    onError: (e) => toast(toError(e).message, "error"),
  });

  /** Scroll to a column (keeps the current match row when there is one). */
  const jumpToColumn = (col: number) => grid.current?.scrollToCell(col, find.matches[findIdx]?.row ?? 0);

  const step = (d: number) => {
    const n = find.matches.length;
    if (n === 0) return;
    const i = (findIdx + d + n) % n;
    setFindIdx(i);
    grid.current?.scrollToCell(find.matches[i].col, find.matches[i].row);
  };

  const openFind = useCallback(() => {
    setFindOpen(true);
    setTimeout(() => findInput.current?.select(), 0);
  }, []);

  const filtered = view.filters.length > 0 || !!view.quick_filter;

  return (
    <div
      className="@container flex h-full min-h-0 flex-col"
      onKeyDownCapture={(e) => {
        if ((e.metaKey || e.ctrlKey) && e.key.toLowerCase() === "f") {
          e.preventDefault();
          e.stopPropagation();
          openFind();
        }
      }}
    >
      <ResultToolbar
        stmt={stmt}
        info={info}
        tabId={tabId}
        connId={connId}
        mode={mode}
        setMode={setMode}
        quick={quick}
        setQuick={setQuick}
        sourceKey={sourceKey}
        keptQuick={keptQuick}
        view={view}
        setView={setView}
        openFind={openFind}
        valueOpen={valueOpen}
        setValueOpen={setValueOpen}
        send={send}
        target={target}
        checkingEdit={checkingEdit}
        startEditing={startEditing}
        stopEditing={stopEditing}
        setLoadOpen={setLoadOpen}
        setExportOpen={setExportOpen}
      />

      {findOpen && (
        <FindBar
          findInput={findInput}
          findQuery={findQuery}
          setFindQuery={setFindQuery}
          findScope={findScope}
          setFindScope={setFindScope}
          wholeWord={wholeWord}
          setWholeWord={setWholeWord}
          setFindOpen={setFindOpen}
          columnNames={columnNames}
          plan={plan}
          find={find}
          findIdx={findIdx}
          step={step}
          jumpToColumn={jumpToColumn}
        />
      )}

      {checkingEdit && <CheckingEditBanner conn={conn} />}

      {target && (
        <EditingBar
          target={target}
          changes={changes}
          saving={saving}
          setEdits={setEdits}
          showPreview={showPreview}
          saveEdits={saveEdits}
          stopEditing={stopEditing}
        />
      )}

      <div className="flex min-h-0 flex-1">
        <div className="min-h-0 min-w-0 flex-1">
        {mode === "chart" ? (
          <ChartView info={info} view={view} onPin={stmt.output && connId ? (spec, kind) => setPinning({ spec, kind }) : undefined} />
        ) : (
        <ResultGrid
          ref={grid}
          info={info}
          view={view}
          onViewChange={setView}
          onViewRows={onViewRows}
          onLoadedRows={onLoadedRows}
          onHeaderMenu={(col, x, y) => setHeaderMenu({ col, x, y })}
          findMatches={find.matches}
          findCurrent={findIdx}
          dialect={conn?.config.kind}
          onSelectionRange={setSelRange}
          onTranspose={setTranspose}
          edit={target ? { target, edits, onEdits: setEdits } : null}
          onOpenRelated={connId ? (link, rows) => void openRelated(link, rows) : undefined}
          onFocusCell={setFocused}
          onViewValue={() => setValueOpen(true)}
        />
        )}
        </div>
        {valueOpen && mode === "grid" && (
          <ValuePanel cell={focused} onClose={() => setValueOpen(false)} onEdit={target && focused ? (v) => grid.current?.setCellValue(focused.col, focused.row, v) : undefined} />
        )}
      </div>

      <ResultFooter stmt={stmt} info={info} view={view} mode={mode} filtered={filtered} viewRows={viewRows} windowRows={windowRows} selRange={selRange} />
      {transpose && <TransposeDialog info={info} view={view} range={transpose} onClose={() => setTranspose(null)} />}
      {pinning && stmt.output && connId && (
        <PinChartDialog info={info} spec={pinning.spec} kind={pinning.kind} sql={stmt.output.sql} connectionId={stmt.output.connection_id || connId} defaultTitle={title ?? tab?.title ?? "Chart"} onClose={() => setPinning(null)} />
      )}
      {loadOpen && <LoadIntoDialog info={info} sql={stmt.output?.sql ?? ""} outputName={stmt.output?.name} connectionId={stmt.output?.connection_id} onClose={() => setLoadOpen(false)} />}
      {preview && <EditPreviewModal preview={preview} setPreview={setPreview} target={target} saving={saving} saveEdits={saveEdits} />}

      {headerMenu && (
        <ColumnMenu
          info={info}
          col={headerMenu.col}
          x={headerMenu.x}
          y={headerMenu.y}
          view={view}
          onView={setView}
          onClose={() => setHeaderMenu(null)}
        />
      )}
      {exportOpen && (
        <ExportDialog
          info={info}
          view={view}
          viewRows={viewRows}
          dialect={conn?.config.kind}
          defaultName={title ?? tab?.title ?? "result"}
          onClose={() => setExportOpen(false)}
        />
      )}
    </div>
  );
}
