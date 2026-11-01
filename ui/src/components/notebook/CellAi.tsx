import { useMemo, useState } from "react";
import { useEvent } from "../../lib/hooks";
import { Wand2 } from "lucide-react";
import type { NotebookCell } from "../../lib/types";
import { useStore } from "../../store";
import { useAi } from "../../aiStore";

// ------------------------------------------------------------------ AI per cell

export function CellAi({
  cell,
  editorKey,
  connectionId,
  hasError,
  onClose,
}: {
  cell: NotebookCell;
  editorKey: string;
  connectionId: string | null;
  hasError: boolean;
  onClose: () => void;
}) {
  const send = useAi((s) => s.send);
  const [text, setText] = useState("");
  const isSql = cell.kind === "sql";
  const go = useEvent((message: string, mode: Parameters<typeof send>[0]["mode"]) => {
    if (!connectionId)
      return useStore.getState().toast("Choose a connection first", "error");
    void send({
      message,
      mode,
      targetKey: isSql ? editorKey : null,
      connectionId,
    });
    onClose();
  });
  const quick = useMemo(
    () =>
      isSql
        ? [
            {
              label: "Explain",
              run: () => go("Explain what this cell's query does.", "explain"),
            },
            ...(hasError
              ? [
                  {
                    label: "Fix error",
                    run: () =>
                      go("Fix the error in this cell's query.", "fix_error"),
                  },
                ]
              : []),
            {
              label: "Optimize",
              run: () =>
                go(
                  "Suggest a faster version of this query and put it in the cell.",
                  "edit",
                ),
            },
            {
              label: "Analyze output",
              run: () =>
                go(
                  "Analyze this cell's result and summarize the findings.",
                  "analyze_result",
                ),
            },
          ]
        : [
            {
              label: "Write SQL for this",
              run: () => go(`Write SQL for: ${cell.source}`, "generate"),
            },
          ],
    [isSql, hasError, cell.source, go],
  );
  return (
    <div className="mt-2 rounded-lg border border-accent/40 bg-accent/5 p-2">
      <form
        className="flex items-center gap-1.5"
        onSubmit={(e) => {
          e.preventDefault();
          if (text.trim())
            go(
              text.trim(),
              isSql ? (cell.source.trim() ? "edit" : "generate") : "chat",
            );
        }}
      >
        <Wand2 size={13} className="shrink-0 text-accent" />
        <input
          autoFocus
          className="field h-7 py-0"
          placeholder={
            isSql
              ? cell.source.trim()
                ? "Change this query… e.g. group by month"
                : "Describe the query to write…"
              : "Ask about this note…"
          }
          aria-label="Ask AI about this cell"
          value={text}
          onChange={(e) => setText(e.target.value)}
          onKeyDown={(e) => e.key === "Escape" && onClose()}
        />
        <button
          type="submit"
          className="btn-primary h-7 py-0"
          disabled={!text.trim()}
        >
          Ask
        </button>
      </form>
      <div className="mt-1.5 flex flex-wrap gap-1">
        {quick.map((q) => (
          <button
            key={q.label}
            className="rounded-md border border-line bg-panel px-2 py-0.5 text-[11.5px] hover:bg-hover"
            onClick={q.run}
          >
            {q.label}
          </button>
        ))}
      </div>
    </div>
  );
}
