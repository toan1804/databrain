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
  readOnly = false,
}: {
  stmt: Pick<StatementRun, "durationMs" | "output">;
  info: ResultInfo;
  tabId: string;
  connectionId?: string | null;
  title?: string;
  /** Another tab's result shown here: no editing, and not kept by this tab. */
  readOnly?: boolean;
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
  // Charts and column statistics of a result on disk: of the rows loaded in the grid too.
  const loadedRows = loaded.id === info.id ? loaded.n : 0;
  const loadedView = useMemo<ViewSpec>(() => (info.on_disk ? { ...view, first_rows: view.first_rows ?? Math.max(1, loadedRows) } : view), [info.on_disk, view, loadedRows]);
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
  /** A save is on its way: the grid takes no edits meanwhile (they would be cleared with the saved ones, unsent). */
  const savingRef = useRef(false);
  const onEdits = useCallback((next: PendingEdits) => {
    if (!savingRef.current) setEdits(next);
  }, []);
  // The same object while nothing changed: the grid's effects depend on it.
  const gridEdit = useMemo(() => (target ? { target, edits, onEdits } : null), [target, edits, onEdits]);
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
    if (savingRef.current) return;
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
    if (!target || !changes || savingRef.current) return;
    // From the first moment: edits made while the statements are previewed
    // or the confirmation is open would otherwise be cleared unsent.
    savingRef.current = true;
    setSaving(true);
    const sent = toChanges(edits);
    // The connection the rows are saved on is the result's (the tab may be
    // on another one since), as the server checks it.
    const resultConn = useStore.getState().connections.find((c) => c.id === (stmt.output?.connection_id ?? connId));
    /** Shows the statements and asks; false when the user says no. */
    const confirmProd = async (why?: string): Promise<boolean> => {
      let stmts: EditStatement[];
      try {
        stmts = await api.editPreview(info.id, target, sent);
      } catch (e) {
        toast(toError(e).message, "error");
        return false;
      }
      const shown = stmts.slice(0, 8).map((s) => (s.sql.length > 300 ? `${s.sql.slice(0, 300)}…` : s.sql));
      return new Promise<boolean>((resolve) =>
        useStore.getState().askConfirm({
          title: `Save ${stmts.length} change${stmts.length === 1 ? "" : "s"} on production?`,
          reasons: [
            why ?? `"${resultConn?.name ?? "This connection"}" is a production connection: these statements run on it.`,
            ...shown,
            ...(stmts.length > shown.length ? [`… and ${stmts.length - shown.length} more (Preview SQL shows them all).`] : []),
          ],
          confirmLabel: "Save changes",
          onConfirm: () => resolve(true),
          onCancel: () => resolve(false),
        }),
      );
    };
    try {
      let confirmed = false;
      if (resultConn?.env === "prod") {
        if (!(await confirmProd())) return;
        confirmed = true;
      }
      let r;
      try {
        r = await api.applyEdits(info.id, sent, confirmed);
      } catch (e) {
        // The server asks (the connection became production meanwhile): asked here too.
        const err = toError(e);
        if (confirmed || err.code !== "needs_confirmation" || !(await confirmProd(err.message))) throw e;
        r = await api.applyEdits(info.id, sent, true);
      }
      setPreview(null);
      setEdits(emptyEdits());
      toast(
        `${formatCount(r.rows)} row${r.rows === 1 ? "" : "s"} changed${r.in_open_transaction ? " in the open transaction — COMMIT to keep" : ""}`,
        "success",
      );
      // This result again from the database (same run key and session); the
      // run's other results stay as they are.
      const st = useStore.getState();
      if (connId && isQueryTab(tab)) void st.refreshTxState(tabId, connId);
      const handle = stmt.output?.handle;
      const at = handle ? (st.runs[tabId]?.statements.findIndex((x) => x.output?.handle === handle) ?? -1) : -1;
      if (at < 0 || !(await st.refreshResult(tabId, at))) toast("Re-run the query to see the saved rows", "info");
    } catch (e) {
      toast(toError(e).message, "error");
    } finally {
      savingRef.current = false;
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
      // The same view when the filter didn't change: a new one reads the
      // first page again and clears the grid's selection.
      const q = quick.trim() || null;
      setView((v) => (v.quick_filter === q ? v : { ...v, quick_filter: q }));
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
        readOnly={readOnly}
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
          <ChartView info={info} view={loadedView} onPin={stmt.output && connId ? (spec, kind) => setPinning({ spec, kind }) : undefined} />
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
          edit={gridEdit}
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
      {loadOpen && (
        <LoadIntoDialog
          info={info}
          sql={stmt.output?.sql ?? ""}
          outputName={stmt.output?.name}
          connectionId={stmt.output?.connection_id}
          tabId={stmt.output?.tab_id ?? tabId}
          onClose={() => setLoadOpen(false)}
        />
      )}
      {preview && <EditPreviewModal preview={preview} setPreview={setPreview} target={target} saving={saving} saveEdits={saveEdits} />}

      {headerMenu && (
        <ColumnMenu
          info={info}
          col={headerMenu.col}
          x={headerMenu.x}
          y={headerMenu.y}
          view={view}
          statsView={loadedView}
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
          tabId={stmt.output?.tab_id ?? tabId}
          onClose={() => setExportOpen(false)}
        />
      )}
    </div>
  );
}
