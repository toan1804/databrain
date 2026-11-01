import type { Dispatch, RefObject, SetStateAction } from "react";
import { ChevronDown, ChevronUp, Search, X } from "lucide-react";
import type { FindPlan } from "../../lib/find";
import { formatCount } from "../../lib/util";

/** Find in the result: a term (or `column: term`), an optional column scope, whole words, matches to step through. */
export function FindBar({
  findInput,
  findQuery,
  setFindQuery,
  findScope,
  setFindScope,
  wholeWord,
  setWholeWord,
  setFindOpen,
  columnNames,
  plan,
  find,
  findIdx,
  step,
  jumpToColumn,
}: {
  findInput: RefObject<HTMLInputElement>;
  findQuery: string;
  setFindQuery: Dispatch<SetStateAction<string>>;
  findScope: number | null;
  setFindScope: Dispatch<SetStateAction<number | null>>;
  wholeWord: boolean;
  setWholeWord: Dispatch<SetStateAction<boolean>>;
  setFindOpen: Dispatch<SetStateAction<boolean>>;
  columnNames: string[];
  plan: FindPlan;
  find: { matches: { row: number; col: number }[]; truncated: boolean };
  findIdx: number;
  step: (d: number) => void;
  jumpToColumn: (col: number) => void;
}) {
  return (
    <div className="flex h-9 shrink-0 items-center gap-2 border-b border-line bg-panel-2 px-2">
      <Search size={13} className="text-muted" />
      <input
        ref={findInput}
        className="field w-64 py-0.5"
        placeholder={findScope === null ? "Find values or columns (column: value)" : `Find in ${columnNames[findScope]}`}
        aria-label="Find in results"
        title="Type text to find in cells. “country: viet” searches only the country column; matching column names are listed to jump to."
        value={findQuery}
        onChange={(e) => setFindQuery(e.target.value)}
        onKeyDown={(e) => {
          if (e.key === "Enter") step(e.shiftKey ? -1 : 1);
          if (e.key === "Escape") setFindOpen(false);
        }}
      />
      <select
        className="field h-6 w-40 py-0 text-[11.5px]"
        aria-label="Search in column"
        title="Search in one column"
        value={findScope ?? ""}
        onChange={(e) => {
          const v = e.target.value === "" ? null : Number(e.target.value);
          setFindScope(v);
          if (v !== null) jumpToColumn(v);
          findInput.current?.focus();
        }}
      >
        <option value="">All columns</option>
        {columnNames.map((c, i) => (
          <option key={i} value={i}>
            {c}
          </option>
        ))}
      </select>
      <button
        className={`icon-btn h-6 w-6 font-mono text-[11px] ${wholeWord ? "bg-accent/15 text-accent" : ""}`}
        aria-label="Whole words only"
        aria-pressed={wholeWord}
        title="Whole words only (id finds “id” and “a id”, not “paid”)"
        onClick={() => (setWholeWord((w) => !w), findInput.current?.focus())}
      >
        ab
      </button>
      <span className="min-w-[80px] text-[11.5px] text-muted" aria-live="polite">
        {plan.term
          ? find.matches.length
            ? `${findIdx + 1} / ${formatCount(find.matches.length)}${find.truncated ? "+" : ""}`
            : "No matches"
          : ""}
      </span>
      <button className="icon-btn h-6 w-6" aria-label="Previous match" onClick={() => step(-1)}>
        <ChevronUp size={14} />
      </button>
      <button className="icon-btn h-6 w-6" aria-label="Next match" onClick={() => step(1)}>
        <ChevronDown size={14} />
      </button>
      {plan.nameMatches.length > 0 && (
        <div className="flex min-w-0 items-center gap-1 overflow-hidden text-[11.5px]">
          <span className="shrink-0 text-muted">Columns:</span>
          {plan.nameMatches.slice(0, 6).map((i) => (
            <button
              key={i}
              className="shrink-0 rounded border border-line px-1.5 font-mono text-[11px] hover:border-accent hover:text-accent"
              title={`Go to column ${columnNames[i]} and search only in it`}
              onClick={() => {
                jumpToColumn(i);
                setFindScope(i);
                setFindQuery("");
                findInput.current?.focus();
              }}
            >
              {columnNames[i]}
            </button>
          ))}
          {plan.nameMatches.length > 6 && <span className="text-muted">+{plan.nameMatches.length - 6}</span>}
        </div>
      )}
      <button className="icon-btn ml-auto h-6 w-6" aria-label="Close find" onClick={() => setFindOpen(false)}>
        <X size={14} />
      </button>
    </div>
  );
}
