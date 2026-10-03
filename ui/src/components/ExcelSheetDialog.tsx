import { useState } from "react";
import { FileSpreadsheet } from "lucide-react";
import { create } from "zustand";
import type { ExcelSheet } from "../lib/types";
import { Modal } from "./ui";

/** Answer of the sheet picker: false = first sheet, true = every sheet, null = cancelled. */
type Pick = boolean | null;

interface State {
  ask: { file: string; sheets: ExcelSheet[]; resolve: (v: Pick) => void } | null;
}

const useSheetAsk = create<State>(() => ({ ask: null }));

/** Ask whether to read the first sheet or every sheet of a workbook. */
export function askExcelSheets(file: string, sheets: ExcelSheet[]): Promise<Pick> {
  return new Promise((resolve) => useSheetAsk.setState({ ask: { file, sheets, resolve } }));
}

export function ExcelSheetDialog() {
  const ask = useSheetAsk((s) => s.ask);
  const [all, setAll] = useState(false);
  if (!ask) return null;
  const used = ask.sheets.filter((s) => s.range);
  const done = (v: Pick) => {
    useSheetAsk.setState({ ask: null });
    setAll(false);
    ask.resolve(v);
  };
  const name = ask.file.split(/[\\/]/).pop();
  return (
    <Modal
      title={
        <span className="flex items-center gap-2">
          <FileSpreadsheet size={16} className="text-success" />
          {name} has {ask.sheets.length} sheets
        </span>
      }
      onClose={() => done(null)}
      width={460}
      footer={
        <>
          <button className="btn-ghost" onClick={() => done(null)}>
            Cancel
          </button>
          <button className="btn-primary" onClick={() => done(all)} autoFocus>
            Open
          </button>
        </>
      }
    >
      <div className="space-y-2 text-[13px]" role="radiogroup" aria-label="Sheets to read">
        <label className="flex cursor-pointer items-start gap-2">
          <input type="radio" name="sheets" className="mt-1" checked={!all} onChange={() => setAll(false)} />
          <span>
            First sheet only <span className="font-mono text-muted">({ask.sheets[0]?.name})</span>
          </span>
        </label>
        <label className="flex cursor-pointer items-start gap-2">
          <input type="radio" name="sheets" className="mt-1" checked={all} onChange={() => setAll(true)} />
          <span>
            All {used.length} sheets with data, one result each
            {used.length < ask.sheets.length && <span className="text-muted"> ({ask.sheets.length - used.length} empty skipped)</span>}
          </span>
        </label>
        <ul className="ml-6 max-h-40 overflow-auto rounded-md border border-line p-1.5 font-mono text-[11.5px]">
          {ask.sheets.map((s, i) => (
            <li key={s.name + i} className={`flex gap-2 ${s.range ? "" : "text-muted"}`}>
              <span className="min-w-0 truncate">{s.name}</span>
              <span className="ml-auto shrink-0 text-muted">
                {s.range ?? "empty"}
                {s.hidden ? " · hidden" : ""}
              </span>
            </li>
          ))}
        </ul>
      </div>
    </Modal>
  );
}
