import { forwardRef, useCallback, useEffect, useImperativeHandle, useMemo, useRef, useState } from "react";
import {
  CompactSelection,
  DataEditor,
  GridCellKind,
  GridColumnIcon,
  type DataEditorRef,
  type GridCell,
  type GridColumn,
  type GridSelection,
  type Highlight,
  type Item,
  type Rectangle,
  type Theme,
} from "@glideapps/glide-data-grid";
import { writeText } from "@tauri-apps/plugin-clipboard-manager";
import { ArrowDown, ArrowDownLeft, ArrowUp, ArrowUpRight, Ban, Copy, Filter, FilterX, Rows3, Trash2, Undo2, X } from "lucide-react";
import { api, toError } from "../lib/api";
import type { ColumnMeta, EditTarget, ExportFormat, ResultInfo, ResultLink, ViewSpec } from "../lib/types";
import { addRow, removeNewRow, revertRows, setCell, setNewCell, toggleDelete, type PendingEdits } from "../lib/gridEdits";
import { pageRange, toggleSort, upsertFilter } from "../lib/util";
import { columnTypeInfo, engineName } from "../lib/columnType";
import { useStore } from "../store";
import { MenuItem, MenuSeparator, Popover } from "./ui";

const PAGE = 200;

function cssVar(name: string): string {
  return getComputedStyle(document.documentElement).getPropertyValue(name).trim();
}

function useGridTheme(): Partial<Theme> {
  const theme = useStore((s) => s.theme);
  const appearance = useStore((s) => s.appearance);
  return useMemo(() => {
    void theme; // recompute after the theme class or the appearance variables change
    void appearance;
    const size = cssVar("--grid-font-size") || "12.5px";
    const accent = cssVar("--accent");
    return {
      accentColor: accent,
      accentFg: cssVar("--accent-fg"),
      accentLight: cssVar("--selection"),
      textDark: cssVar("--text"),
      textMedium: cssVar("--muted"),
      textLight: cssVar("--muted"),
      textHeader: cssVar("--muted"),
      textHeaderSelected: cssVar("--accent-fg"),
      bgIconHeader: cssVar("--muted"),
      fgIconHeader: cssVar("--panel"),
      bgCell: cssVar("--panel"),
      bgCellMedium: cssVar("--panel-2"),
      bgHeader: cssVar("--panel-2"),
      bgHeaderHasFocus: cssVar("--hover"),
      bgHeaderHovered: cssVar("--hover"),
      bgBubble: cssVar("--panel-2"),
      bgBubbleSelected: cssVar("--hover"),
      bgSearchResult: "rgba(250, 204, 21, 0.25)",
      borderColor: cssVar("--border"),
      horizontalBorderColor: cssVar("--border"),
      headerBottomBorderColor: cssVar("--border"),
      drilldownBorder: cssVar("--border"),
      linkColor: accent,
      fontFamily: cssVar("--font-mono") || "ui-monospace, Menlo, monospace",
      baseFontStyle: size,
      headerFontStyle: "600 12px",
      markerFontStyle: "11px",
      editorFontSize: size,
      cellHorizontalPadding: 10,
      cellVerticalPadding: 4,
    };
  }, [theme, appearance]);
}

function iconFor(c: ColumnMeta): GridColumnIcon {
  switch (c.family) {
    case "number":
      return GridColumnIcon.HeaderNumber;
    case "bool":
      return GridColumnIcon.HeaderBoolean;
    case "date":
    case "time":
      return GridColumnIcon.HeaderDate;
    case "binary":
      return GridColumnIcon.HeaderCode;
    default:
      return GridColumnIcon.HeaderString;
  }
}

/** Longest text drawn in a cell (the cell is one line, at most 1200 px wide). */
const PREVIEW_CHARS = 1000;

/**
 * What a cell draws: one line. The grid draws only the first line of a
 * multi-line value, so line breaks become ↵ (also in long values such as
 * CLOBs, which are cut first so huge texts stay cheap to draw). The full
 * value stays in the cell's data: double-click / Enter or copy shows it.
 */
function cellPreview(v: string): string {
  const head = v.length > PREVIEW_CHARS ? v.slice(0, PREVIEW_CHARS) + "…" : v;
  return head.replace(/\r\n|\r|\n/g, "↵");
}

function estimateWidth(name: string, sample: (string | null)[]): number {
  let chars = name.length + 3;
  for (const v of sample) if (v) chars = Math.max(chars, Math.min(v.length, 60));
  return Math.max(70, Math.min(420, Math.round(chars * 7.6 + 24)));
}

export interface GridHandle {
  scrollToCell: (col: number, row: number) => void;
  copySelection: (format: ExportFormat, header: boolean) => Promise<void>;
}

interface Props {
  info: ResultInfo;
  view: ViewSpec;
  onViewChange: (v: ViewSpec) => void;
  onHeaderMenu: (col: number, x: number, y: number) => void;
  onViewRows: (n: number) => void;
  findMatches: { row: number; col: number }[];
  findCurrent: number;
  dialect: string | undefined;
  /** The selected rectangle changed (null: nothing selected). */
  onSelectionRange?: (range: { cols: number[]; offset: number; limit: number } | null) => void;
  /** "Transpose selected rows" from the cell menu. */
  onTranspose?: (range: { cols: number[]; offset: number; limit: number }) => void;
  /** Editing: cells of the target table's columns can be changed. */
  edit?: { target: EditTarget; edits: PendingEdits; onEdits: (e: PendingEdits) => void } | null;
  /** Follow a foreign key from the selected rows (values in `link.columns` order). */
  onOpenRelated?: (link: ResultLink, rows: (string | null)[][]) => void;
}

const CHANGED_BG = "rgba(250, 204, 21, 0.20)";
const DELETED_BG = "rgba(239, 68, 68, 0.14)";
const NEW_BG = "rgba(34, 197, 94, 0.12)";

export const ResultGrid = forwardRef<GridHandle, Props>(function ResultGrid(
  { info, view, onViewChange, onHeaderMenu, onViewRows, findMatches, findCurrent, dialect, onSelectionRange, onTranspose, edit, onOpenRelated },
  ref,
) {
  const theme = useGridTheme();
  const toast = useStore((s) => s.toast);
  const gridRef = useRef<DataEditorRef>(null);
  const pages = useRef(new Map<number, (string | null)[][]>());
  /** Original row ids of each page (editing finds rows by them). */
  const pageIds = useRef(new Map<number, number[]>());
  const inflight = useRef(new Set<number>());
  const generation = useRef(0);
  const visible = useRef<Rectangle>({ x: 0, y: 0, width: 10, height: 40 });
  const [version, setVersion] = useState(0);
  const [rows, setRows] = useState(0);
  const [widths, setWidths] = useState<Record<number, number>>({});
  const [selection, setSelection] = useState<GridSelection>({
    columns: CompactSelection.empty(),
    rows: CompactSelection.empty(),
  });
  const [menu, setMenu] = useState<{ x: number; y: number; cell: Item } | null>(null);
  /** Column header under the pointer: its type is shown in a tooltip. */
  const [hoverHead, setHoverHead] = useState<{ col: number; x: number; y: number } | null>(null);
  const outputs = useStore((s) => s.outputs);
  const tip = useMemo(() => {
    const c = hoverHead && info.columns[hoverHead.col];
    return c ? columnTypeInfo(c, info.id, outputs) : null;
  }, [hoverHead, info.columns, info.id, outputs]);
  const autoSized = useRef(false);
  /** Foreign keys of the result's table (loaded on the first cell menu). */
  const [links, setLinks] = useState<ResultLink[] | null>(null);
  useEffect(() => setLinks(null), [info.id]);
  const loadLinks = () => {
    if (links !== null || !onOpenRelated) return;
    setLinks([]);
    api.resultLinks(info.id).then(setLinks, () => setLinks([]));
  };

  const fetchPage = useCallback(
    async (p: number) => {
      if (!Number.isInteger(p) || p < 0 || pages.current.has(p) || inflight.current.has(p)) return;
      inflight.current.add(p);
      const gen = generation.current;
      try {
        const page = await api.fetchPage(info.id, view, p * PAGE, PAGE);
        if (gen !== generation.current) return;
        pages.current.set(p, page.rows);
        pageIds.current.set(p, page.row_ids);
        setRows(page.view_rows);
        onViewRows(page.view_rows);
        if (!autoSized.current && p === 0) {
          autoSized.current = true;
          const w: Record<number, number> = {};
          info.columns.forEach((c, i) => {
            w[i] = estimateWidth(c.name, page.rows.slice(0, 50).map((r) => r[i]));
          });
          setWidths((old) => ({ ...w, ...old }));
        }
        setVersion((v) => v + 1);
      } catch (e) {
        if (gen === generation.current) toast(toError(e).message, "error");
      } finally {
        inflight.current.delete(p);
      }
    },
    [info.id, info.columns, view, onViewRows, toast],
  );

  // Reset the cache whenever the result or view changes.
  useEffect(() => {
    generation.current++;
    pages.current.clear();
    pageIds.current.clear();
    inflight.current.clear();
    const r = visible.current;
    void fetchPage(0);
    // Row count of the new view is unknown until page 0 arrives; clamp to the stored total.
    for (const p of pageRange(r.y, r.height, PAGE, info.total_rows)) if (p !== 0) void fetchPage(p);
    setSelection({ columns: CompactSelection.empty(), rows: CompactSelection.empty() });
  }, [fetchPage]); // eslint-disable-line react-hooks/exhaustive-deps

  useEffect(() => {
    autoSized.current = false;
    setWidths({});
  }, [info.id]);

  const onVisibleRegionChanged = useCallback(
    (r: Rectangle) => {
      visible.current = r;
      for (const p of pageRange(r.y, r.height, PAGE, rows)) void fetchPage(p);
    },
    [fetchPage, rows],
  );

  const columns = useMemo<GridColumn[]>(
    () =>
      info.columns.map((c, i) => {
        const s = view.sort.findIndex((k) => k.column === i);
        const arrow = s === -1 ? "" : view.sort[s].descending ? " ↓" : " ↑";
        const order = s !== -1 && view.sort.length > 1 ? `${s + 1}` : "";
        const filtered = view.filters.some((f) => f.column === i);
        return {
          id: String(i),
          title: `${c.name}${arrow}${order}${filtered ? " ⏷" : ""}`,
          width: widths[i] ?? 150,
          icon: iconFor(c),
          hasMenu: true,
        };
      }),
    [info.columns, view.sort, view.filters, widths],
  );

  /** Original row id and fetched key values of a view row (null when not fetched). */
  const rowIdentity = useCallback(
    (row: number): { id: number; key: (string | null)[] } | null => {
      const p = Math.floor(row / PAGE);
      const id = pageIds.current.get(p)?.[row % PAGE];
      const r = pages.current.get(p)?.[row % PAGE];
      if (id === undefined || !r || !edit) return null;
      return { id, key: edit.target.key_columns.map((k) => r[k] ?? null) };
    },
    [edit],
  );

  const getCellContent = useCallback(
    ([col, row]: Item): GridCell => {
      const fam = info.columns[col]?.family;
      const align = fam === "number" ? "right" : "left";
      const editable = !!edit && edit.target.columns[col] != null;
      // New rows (editing) come after the fetched ones.
      if (edit && row >= rows) {
        const vals = edit.edits.inserts[row - rows];
        const has = vals?.has(col);
        const v = vals?.get(col);
        return {
          kind: GridCellKind.Text,
          data: v ?? "",
          displayData: !has ? (editable ? "(default)" : "") : v === null ? "NULL" : cellPreview(v ?? ""),
          allowOverlay: editable,
          readonly: !editable,
          style: has && v !== null ? "normal" : "faded",
          themeOverride: { bgCell: NEW_BG },
          contentAlign: align,
        };
      }
      const page = pages.current.get(Math.floor(row / PAGE));
      if (!page) return { kind: GridCellKind.Loading, allowOverlay: false };
      let v = page[row % PAGE]?.[col];
      if (v === undefined) return { kind: GridCellKind.Loading, allowOverlay: false };
      let themeOverride: Partial<Theme> | undefined;
      let deleted = false;
      if (edit) {
        const id = pageIds.current.get(Math.floor(row / PAGE))?.[row % PAGE];
        if (id !== undefined && edit.edits.deletes.has(id)) {
          deleted = true;
          themeOverride = { bgCell: DELETED_BG, textDark: cssVar("--muted") };
        } else {
          const u = id !== undefined ? edit.edits.updates.get(id) : undefined;
          if (u?.values.has(col)) {
            v = u.values.get(col) ?? null;
            themeOverride = { bgCell: CHANGED_BG };
          }
        }
      }
      const canEdit = editable && !deleted;
      if (v === null) {
        return { kind: GridCellKind.Text, data: "", displayData: "NULL", allowOverlay: canEdit, readonly: !canEdit, style: "faded", copyData: "", themeOverride };
      }
      return {
        kind: GridCellKind.Text,
        data: v,
        displayData: cellPreview(v),
        allowOverlay: true,
        readonly: !canEdit,
        contentAlign: align,
        themeOverride,
      };
    },
    // eslint-disable-next-line react-hooks/exhaustive-deps
    [version, info.columns, edit, rows],
  );

  /** A typed value: stored for a fetched row (with its key) or a new row. */
  const editCell = (col: number, row: number, value: string | null) => {
    if (!edit || edit.target.columns[col] == null) return;
    if (row >= rows) return edit.onEdits(setNewCell(edit.edits, row - rows, col, value));
    const who = rowIdentity(row);
    if (!who || edit.edits.deletes.has(who.id)) return;
    const original = pages.current.get(Math.floor(row / PAGE))?.[row % PAGE]?.[col] ?? null;
    edit.onEdits(setCell(edit.edits, who.id, who.key, col, original, value));
  };

  /** View rows of the selection (or the focused cell's row). */
  const selectedRows = (): number[] => {
    const r = selectedRange();
    if (!r) return [];
    return Array.from({ length: Math.min(r.limit, 10_000) }, (_, i) => r.offset + i);
  };

  const highlights = useMemo<Highlight[]>(() => {
    if (findMatches.length === 0) return [];
    const r = visible.current;
    const out: Highlight[] = [];
    findMatches.forEach((m, i) => {
      if (i !== findCurrent && (m.row < r.y - 5 || m.row > r.y + r.height + 5)) return;
      out.push({
        color: i === findCurrent ? "rgba(250, 204, 21, 0.55)" : "rgba(250, 204, 21, 0.22)",
        range: { x: m.col, y: m.row, width: 1, height: 1 },
        style: i === findCurrent ? "solid-outline" : "no-outline",
      });
    });
    return out;
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [findMatches, findCurrent, version]);

  /** Selected rectangle in (columns, rows) terms; falls back to the focused cell. */
  const selectedRange = useCallback((): { cols: number[]; offset: number; limit: number } | null => {
    const cur = selection.current;
    if (selection.rows.length > 0) {
      const first = selection.rows.first()!;
      const last = selection.rows.last()!;
      return { cols: [], offset: first, limit: last - first + 1 };
    }
    if (selection.columns.length > 0) {
      return { cols: selection.columns.toArray(), offset: 0, limit: rows };
    }
    if (cur) {
      const r = cur.range;
      const cols = Array.from({ length: r.width }, (_, i) => r.x + i);
      return { cols, offset: r.y, limit: r.height };
    }
    return null;
  }, [selection, rows]);

  useEffect(() => {
    onSelectionRange?.(selectedRange());
  }, [selection, selectedRange]); // eslint-disable-line react-hooks/exhaustive-deps

  const copy = useCallback(
    async (format: ExportFormat, header: boolean, range = selectedRange()) => {
      if (!range) return;
      try {
        const text = await api.copyRows(info.id, view, range.offset, range.limit, {
          format,
          header,
          columns: range.cols,
          table_name: "result",
          dialect: (dialect as never) ?? null,
        });
        // Single value without header: copy the raw value without a newline.
        await writeText(!header && format === "tsv" ? text.replace(/\n$/, "") : text);
        const n = range.limit;
        toast(`Copied ${n.toLocaleString()} row${n === 1 ? "" : "s"}`, "success");
      } catch (e) {
        toast(toError(e).message, "error");
      }
    },
    [info.id, view, dialect, selectedRange, toast],
  );

  useImperativeHandle(
    ref,
    () => ({
      scrollToCell: (col, row) => {
        gridRef.current?.scrollTo(col, row, "both", 0, 0, { vAlign: "center" });
        setSelection({
          columns: CompactSelection.empty(),
          rows: CompactSelection.empty(),
          current: { cell: [col, row], range: { x: col, y: row, width: 1, height: 1 }, rangeStack: [] },
        });
      },
      copySelection: (format, header) => copy(format, header),
    }),
    [copy],
  );

  const cellValue = (cell: Item): string | null | undefined =>
    pages.current.get(Math.floor(cell[1] / PAGE))?.[cell[1] % PAGE]?.[cell[0]];

  const filterBy = (cell: Item, op: "equals" | "not_equals" | "is_null" | "is_not_null") => {
    const v = cellValue(cell);
    const nullOp = v === null && (op === "equals" || op === "not_equals");
    const realOp = nullOp ? (op === "equals" ? "is_null" : "is_not_null") : op;
    onViewChange({ ...view, filters: upsertFilter(view.filters, { column: cell[0], op: realOp, value: v ?? "" }) });
  };

  return (
    <div
      className="relative h-full w-full"
      onMouseLeave={() => setHoverHead(null)}
      onKeyDown={(e) => {
        if ((e.metaKey || e.ctrlKey) && e.key.toLowerCase() === "c") {
          e.preventDefault();
          e.stopPropagation();
          void copy("tsv", e.shiftKey);
        }
      }}
    >
      <DataEditor
        ref={gridRef}
        width="100%"
        height="100%"
        theme={theme}
        columns={columns}
        rows={rows + (edit ? edit.edits.inserts.length : 0)}
        getCellContent={getCellContent}
        onCellEdited={
          edit
            ? ([col, row], v) => {
                if (v.kind === GridCellKind.Text) editCell(col, row, v.data);
              }
            : undefined
        }
        trailingRowOptions={edit ? { hint: "Add row", sticky: true, tint: true } : undefined}
        onRowAppended={edit ? () => void edit.onEdits(addRow(edit.edits)) : undefined}
        onVisibleRegionChanged={onVisibleRegionChanged}
        rowMarkers="number"
        rowMarkerWidth={Math.max(44, String(rows).length * 8 + 20)}
        smoothScrollX
        smoothScrollY
        rowHeight={28}
        headerHeight={32}
        freezeColumns={0}
        gridSelection={selection}
        onGridSelectionChange={setSelection}
        rangeSelect="multi-rect"
        columnSelect="multi"
        rowSelect="multi"
        keybindings={{ copy: false, paste: false, search: false, selectAll: true }}
        highlightRegions={highlights}
        onColumnResize={(_c, size, idx) => setWidths((w) => ({ ...w, [idx]: size }))}
        minColumnWidth={50}
        maxColumnWidth={1200}
        onHeaderClicked={(col, e) => {
          e.preventDefault();
          onViewChange({ ...view, sort: toggleSort(view.sort, col, e.shiftKey) });
        }}
        onHeaderMenuClick={(col, bounds) => onHeaderMenu(col, bounds.x, bounds.y + bounds.height)}
        onItemHovered={(a) => {
          if (a.kind === "header" && a.location[0] >= 0) {
            const [col] = a.location;
            setHoverHead((h) => (h?.col === col ? h : { col, x: a.bounds.x, y: a.bounds.y + a.bounds.height + 4 }));
          } else setHoverHead(null);
        }}
        onCellContextMenu={(cell, e) => {
          e.preventDefault();
          const inSel =
            selection.current &&
            cell[0] >= selection.current.range.x &&
            cell[0] < selection.current.range.x + selection.current.range.width &&
            cell[1] >= selection.current.range.y &&
            cell[1] < selection.current.range.y + selection.current.range.height;
          if (!inSel && !selection.rows.hasIndex(cell[1])) {
            setSelection({
              columns: CompactSelection.empty(),
              rows: CompactSelection.empty(),
              current: { cell, range: { x: cell[0], y: cell[1], width: 1, height: 1 }, rangeStack: [] },
            });
          }
          setMenu({ x: e.bounds.x + e.localEventX, y: e.bounds.y + e.localEventY, cell });
          loadLinks();
        }}
        getCellsForSelection={undefined}
      />
      {tip && hoverHead && !menu && (
        <div
          role="tooltip"
          className="pointer-events-none fixed z-50 max-w-sm rounded-md border border-line bg-panel px-2 py-1.5 text-[11.5px] shadow-lg"
          style={{ left: Math.max(4, Math.min(hoverHead.x, window.innerWidth - 300)), top: hoverHead.y }}
        >
          <div className="font-medium">{tip.name}</div>
          <div className="font-mono text-accent">{tip.type}</div>
          {tip.engine && <div className="text-[10.5px] text-muted">{engineName(tip.engine)} type</div>}
          {tip.sources.map((src) => (
            <div key={src.ref} className="mt-1 border-t border-line pt-1">
              <span className="font-mono">{src.type}</span>
              <span className="text-muted">
                {" "}
                in {src.ref} ({src.connection}, {engineName(src.engine)})
              </span>
            </div>
          ))}
        </div>
      )}
      {menu && (
        <Popover x={menu.x} y={menu.y} onClose={() => setMenu(null)} className="w-60">
          <MenuItem
            icon={<Copy size={13} />}
            label="Copy"
            hint="⌘C"
            onClick={() => {
              setMenu(null);
              void copy("tsv", false);
            }}
          />
          <MenuItem
            icon={<Copy size={13} />}
            label="Copy with headers"
            hint="⇧⌘C"
            onClick={() => {
              setMenu(null);
              void copy("tsv", true);
            }}
          />
          <MenuItem icon={<Copy size={13} />} label="Copy as CSV" onClick={() => { setMenu(null); void copy("csv", true); }} />
          <MenuItem icon={<Copy size={13} />} label="Copy as JSON" onClick={() => { setMenu(null); void copy("json", false); }} />
          <MenuItem icon={<Copy size={13} />} label="Copy as Markdown" onClick={() => { setMenu(null); void copy("markdown", true); }} />
          <MenuItem icon={<Copy size={13} />} label="Copy as SQL INSERT" onClick={() => { setMenu(null); void copy("sql_insert", false); }} />
          {onTranspose && (
            <MenuItem
              icon={<Rows3 size={13} />}
              label="Transpose selected rows"
              onClick={() => {
                setMenu(null);
                const r = selectedRange();
                if (r) onTranspose({ cols: r.cols, offset: r.offset, limit: r.limit });
              }}
            />
          )}
          {edit && (
            <>
              <MenuSeparator />
              {edit.target.columns[menu.cell[0]] != null && (
                <MenuItem
                  icon={<Ban size={13} />}
                  label="Set to NULL"
                  onClick={() => {
                    setMenu(null);
                    editCell(menu.cell[0], menu.cell[1], null);
                  }}
                />
              )}
              {menu.cell[1] >= rows ? (
                <MenuItem
                  icon={<Trash2 size={13} />}
                  label="Remove new row"
                  onClick={() => {
                    setMenu(null);
                    edit.onEdits(removeNewRow(edit.edits, menu.cell[1] - rows));
                  }}
                />
              ) : (
                <>
                  <MenuItem
                    icon={<Trash2 size={13} />}
                    label="Delete / undelete row(s)"
                    onClick={() => {
                      setMenu(null);
                      const who = selectedRows().filter((r) => r < rows).map(rowIdentity).filter((x): x is { id: number; key: (string | null)[] } => !!x);
                      edit.onEdits(toggleDelete(edit.edits, who));
                    }}
                  />
                  <MenuItem
                    icon={<Undo2 size={13} />}
                    label="Revert row changes"
                    onClick={() => {
                      setMenu(null);
                      const ids = selectedRows().map((r) => rowIdentity(r)?.id).filter((x): x is number => x !== undefined);
                      edit.onEdits(revertRows(edit.edits, ids));
                    }}
                  />
                </>
              )}
            </>
          )}
          {onOpenRelated && links && links.length > 0 && (
            <>
              <MenuSeparator />
              {[...links]
                // Keys through the clicked column first.
                .sort((a, b) => Number(b.columns.includes(menu.cell[0])) - Number(a.columns.includes(menu.cell[0])))
                .slice(0, 10)
                .map((l, i) => (
                  <MenuItem
                    key={i}
                    icon={l.direction === "parent" ? <ArrowUpRight size={13} /> : <ArrowDownLeft size={13} />}
                    label={l.direction === "parent" ? `Open referenced ${l.table}` : `Rows in ${l.table} referencing this`}
                    hint={l.direction === "parent" ? info.columns.filter((_, c) => l.columns.includes(c)).map((c) => c.name).join(", ") : l.target_columns.join(", ")}
                    onClick={() => {
                      setMenu(null);
                      const vals = selectedRows()
                        .filter((r) => r < rows)
                        .map((r) => pages.current.get(Math.floor(r / PAGE))?.[r % PAGE])
                        .filter((r): r is (string | null)[] => !!r)
                        .map((r) => l.columns.map((c) => r[c] ?? null));
                      onOpenRelated(l, vals);
                    }}
                  />
                ))}
            </>
          )}
          <MenuSeparator />
          <MenuItem icon={<Filter size={13} />} label="Filter: equals this value" onClick={() => { setMenu(null); filterBy(menu.cell, "equals"); }} />
          <MenuItem icon={<FilterX size={13} />} label="Filter: not this value" onClick={() => { setMenu(null); filterBy(menu.cell, "not_equals"); }} />
          <MenuItem icon={<Ban size={13} />} label="Filter: is NULL" onClick={() => { setMenu(null); filterBy(menu.cell, "is_null"); }} />
          <MenuSeparator />
          <MenuItem icon={<ArrowUp size={13} />} label="Sort ascending" onClick={() => { setMenu(null); onViewChange({ ...view, sort: [{ column: menu.cell[0], descending: false }] }); }} />
          <MenuItem icon={<ArrowDown size={13} />} label="Sort descending" onClick={() => { setMenu(null); onViewChange({ ...view, sort: [{ column: menu.cell[0], descending: true }] }); }} />
          {(view.sort.length > 0 || view.filters.length > 0) && (
            <MenuItem
              icon={<X size={13} />}
              label="Clear sort & filters"
              onClick={() => {
                setMenu(null);
                onViewChange({ ...view, sort: [], filters: [] });
              }}
            />
          )}
        </Popover>
      )}
    </div>
  );
});
