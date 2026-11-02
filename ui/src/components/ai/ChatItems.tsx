import { memo, useMemo, useState } from "react";
import { AlertCircle, Check, ChevronDown, ChevronRight, Copy, FileCode2, Loader2, Play, RefreshCw, Settings2, ShieldAlert } from "lucide-react";
import { writeText } from "@tauri-apps/plugin-clipboard-manager";
import type { ResultInfo } from "../../lib/types";
import { emptyView, lineDiff, sqlPreview } from "../../lib/util";
import { useStore } from "../../store";
import { useAi, type ChatItem } from "../../aiStore";
import { editorBridge } from "../../editorBridge";
import { Markdown, type CodeRenderer } from "../Markdown";
import { ResultGrid } from "../ResultGrid";

/** SQL code block with copy / insert / run actions. */
export function SqlBlock({ code, lang }: { code: string; lang: string }) {
  // No store subscription: a chat has many blocks, and they read the store only when clicked.
  const st = useStore.getState;
  const isSql = !lang || lang === "sql";
  return (
    <div className="overflow-hidden rounded-md border border-line bg-panel-2">
      <pre className="max-h-72 overflow-auto p-2 font-mono text-[12px] leading-snug select-text">{code}</pre>
      <div className="flex gap-1 border-t border-line px-1.5 py-1">
        <button className="btn-ghost h-6 px-1.5 py-0 text-[11px]" onClick={() => void writeText(code).then(() => st().toast("Copied", "success"))}>
          <Copy size={11} /> Copy
        </button>
        {isSql && (
          <>
            <button className="btn-ghost h-6 px-1.5 py-0 text-[11px]" onClick={() => editorBridge.insert(editorBridge.focused() ?? st().activeTabId, code)}>
              <FileCode2 size={11} /> Insert
            </button>
            <button
              className="btn-ghost h-6 px-1.5 py-0 text-[11px]"
              onClick={() => st().newTab({ title: "AI query", sql: code, connection_id: useAi.getState().connectionId })}
            >
              <Play size={11} /> Open in tab
            </button>
          </>
        )}
      </div>
    </div>
  );
}

/** SQL blocks in answers (module-level, so Markdown's memo holds). */
const sqlCode: CodeRenderer = (c, l, k) => <SqlBlock key={k} code={c} lang={l} />;

/** Memoized: a streamed token replaces only the last item, the others keep their identity. */
export const ChatItemView = memo(function ChatItemView({ item }: { item: ChatItem }) {
  const respondApproval = useAi((s) => s.respondApproval);
  const respondEdit = useAi((s) => s.respondEdit);
  switch (item.kind) {
    case "user":
      return (
        <div className="ml-6 rounded-lg bg-accent/12 px-3 py-2 text-[13px] whitespace-pre-wrap select-text">
          {item.text.split(/((?:^|(?<=[\s(,]))@[A-Za-z_]\w*)/).map((part, i) =>
            part.startsWith("@") ? (
              <span key={i} className="rounded bg-accent/20 px-0.5 font-mono text-[12px] text-accent">
                {part}
              </span>
            ) : (
              part
            ),
          )}
        </div>
      );
    case "assistant":
      return (
        <div className="select-text">
          <Markdown text={item.text} code={sqlCode} />
          {item.streaming && <span className="ml-0.5 inline-block h-3 w-1.5 animate-pulse bg-accent align-middle" />}
        </div>
      );
    case "error":
      return (
        <div className="rounded-lg border border-danger/40 bg-danger/10 p-2 text-[12.5px]" role="alert">
          <div className="flex items-start gap-2">
            <AlertCircle size={14} className="mt-px shrink-0 text-danger" />
            <span className="break-words">{item.text}</span>
          </div>
          {item.reauthed && <div className="mt-1.5 text-[11.5px] text-success">Signed in again. Send the question again?</div>}
          {(item.providerId !== undefined || item.retry) && (
            <div className="mt-1.5 flex flex-wrap gap-1.5">
              {item.retry && (
                <button
                  className={`${item.reauthed ? "btn-primary" : "btn-ghost border border-line"} py-1 text-[12px]`}
                  onClick={() => useAi.getState().resend(item.id)}
                  title={item.retry.message}
                >
                  <RefreshCw size={12} /> Resend question
                </button>
              )}
              {item.providerId !== undefined && !item.reauthed && (
                <button className="btn-ghost border border-line py-1 text-[12px]" onClick={() => useAi.getState().openProviderSettings(item.providerId)}>
                  <Settings2 size={12} /> Open provider settings to sign in again
                </button>
              )}
            </div>
          )}
        </div>
      );
    case "tool":
      return <ToolCard item={item} />;
    case "approval":
      return <ApprovalCard item={item} onRespond={respondApproval} />;
    case "edit":
      return <EditCard item={item} onRespond={respondEdit} />;
  }
});

const TOOL_LABELS: Record<string, string> = {
  search_schema: "Searched schema",
  describe_table: "Described table",
  index_tables: "Indexed tables",
  list_tables: "Listed tables",
  get_sample_rows: "Sampled rows",
  run_query: "Ran query",
  query_result: "Queried result",
  result_summary: "Summarized result",
  add_knowledge_note: "Added knowledge note",
  update_knowledge_note: "Updated knowledge note",
  list_knowledge_notes: "Read notes & glossary",
  save_query: "Saved query",
  get_editor: "Read editor",
  write_editor: "Proposed editor change",
  list_outputs: "Listed outputs",
  query_outputs: "Queried outputs (DuckDB)",
  list_connections: "Listed connections",
  list_jobs: "Listed jobs",
  get_job: "Read job",
  create_job: "Created job",
  update_job: "Updated job",
  run_job: "Ran job",
};

function ToolCard({ item }: { item: Extract<ChatItem, { kind: "tool" }> }) {
  const [open, setOpen] = useState(false);
  const arg = (k: string) => (typeof item.args[k] === "string" ? (item.args[k] as string) : undefined);
  const subject = arg("sql") ?? arg("table") ?? arg("query") ?? arg("note") ?? arg("name") ?? "";
  const results = useMemo(() => {
    const d = item.display as { statements?: { result?: ResultInfo }[] } | undefined;
    return (d?.statements ?? []).map((s) => s.result).filter((r): r is ResultInfo => !!r);
  }, [item.display]);
  return (
    <div className="rounded-lg border border-line text-[12px]">
      <button className="flex w-full items-center gap-1.5 px-2 py-1.5 text-left" onClick={() => setOpen(!open)} aria-expanded={open}>
        {item.done ? <Check size={12} className="shrink-0 text-success" /> : <Loader2 size={12} className="shrink-0 animate-spin text-accent" />}
        <span className="shrink-0 font-medium">{TOOL_LABELS[item.tool] ?? item.tool}</span>
        <span className="min-w-0 flex-1 truncate font-mono text-[11px] text-muted">{sqlPreview(subject, 80)}</span>
        {open ? <ChevronDown size={12} /> : <ChevronRight size={12} />}
      </button>
      {results[0] && <MiniGrid info={results[0]} />}
      {open && (
        <div className="space-y-1.5 border-t border-line p-2">
          {arg("sql") && <pre className="overflow-x-auto whitespace-pre-wrap font-mono text-[11.5px] select-text">{arg("sql")}</pre>}
          {item.content && (
            <pre className="max-h-48 overflow-auto whitespace-pre-wrap font-mono text-[11px] text-muted select-text">{item.content}</pre>
          )}
        </div>
      )}
    </div>
  );
}

function MiniGrid({ info }: { info: ResultInfo }) {
  const [view, setView] = useState(emptyView);
  return (
    <div className="border-t border-line" style={{ height: Math.min(240, 36 + Math.max(1, info.total_rows) * 30) }}>
      <ResultGrid
        info={info}
        view={view}
        onViewChange={setView}
        onViewRows={() => {}}
        onHeaderMenu={() => {}}
        findMatches={[]}
        findCurrent={0}
        dialect={undefined}
      />
    </div>
  );
}

function ApprovalCard({
  item,
  onRespond,
}: {
  item: Extract<ChatItem, { kind: "approval" }>;
  onRespond: (id: string, approved: boolean, sql?: string) => void;
}) {
  const initialSql = typeof item.detail.sql === "string" ? item.detail.sql : undefined;
  const [sql, setSql] = useState(initialSql ?? "");
  const pending = item.state === "pending";
  return (
    <div className={`rounded-lg border p-2 text-[12.5px] ${pending ? "border-warning/50 bg-warning/5" : "border-line"}`}>
      <div className="mb-1 flex items-center gap-1.5 font-medium">
        <ShieldAlert size={13} className="text-warning" />
        {item.tool === "run_query" ? "Run this query?" : item.tool === "create_job" ? "Create this job?" : item.tool === "update_job" ? "Change this job?" : item.tool === "run_job" ? "Run this job?" : item.tool === "index_tables" ? "Index these tables?" : `Allow ${item.tool}?`}
        {!pending && (
          <span className={`ml-auto text-[11px] ${item.state === "approved" ? "text-success" : "text-muted"}`}>
            {item.state === "approved" ? (item.auto ? "Auto-approved" : "Approved") : item.state === "expired" ? "No longer waiting (stopped or timed out)" : "Declined"}
          </span>
        )}
      </div>
      <div className="mb-1.5 text-[11.5px] text-muted">{item.summary}</div>
      {initialSql !== undefined &&
        (pending ? (
          <textarea
            className="field min-h-[70px] font-mono text-[11.5px]"
            value={sql}
            aria-label="SQL to run (editable)"
            onChange={(e) => setSql(e.target.value)}
          />
        ) : (
          <pre className="whitespace-pre-wrap font-mono text-[11.5px] select-text">{sql}</pre>
        ))}
      {initialSql === undefined && typeof item.detail.preview === "string" && (
        <pre className="max-h-64 overflow-auto whitespace-pre-wrap rounded border border-line bg-panel-2 p-1.5 font-mono text-[11px] select-text">{item.detail.preview}</pre>
      )}
      {initialSql === undefined && typeof item.detail.preview !== "string" && (
        <pre className="max-h-32 overflow-auto whitespace-pre-wrap font-mono text-[11px] text-muted">{JSON.stringify(item.detail, null, 2)}</pre>
      )}
      {pending && (
        <div className="mt-2 flex justify-end gap-1.5">
          <button className="btn-ghost py-1" onClick={() => onRespond(item.id, false)} disabled={item.sending}>
            Deny
          </button>
          <button className="btn-primary py-1" onClick={() => onRespond(item.id, true, initialSql !== undefined ? sql : undefined)} disabled={item.sending}>
            <Play size={12} /> {item.sending ? "Sending…" : item.tool === "run_query" ? "Run" : "Allow"}
          </button>
        </div>
      )}
    </div>
  );
}

function EditCard({ item, onRespond }: { item: Extract<ChatItem, { kind: "edit" }>; onRespond: (id: string, ok: boolean) => void }) {
  const diff = useMemo(() => lineDiff(item.before, item.proposal.sql), [item.before, item.proposal.sql]);
  const pending = item.state === "pending";
  const target =
    item.proposal.mode === "new_tab" || !item.targetKey
      ? "a new tab"
      : item.proposal.mode === "replace_all"
        ? "the whole editor"
        : item.proposal.mode === "insert_at_cursor"
          ? "the cursor position"
          : "the selection";
  return (
    <div className={`overflow-hidden rounded-lg border text-[12px] ${pending ? "border-accent/60" : "border-line"}`}>
      <div className="flex items-center gap-1.5 px-2 py-1.5 font-medium">
        <FileCode2 size={13} className="text-accent" />
        {item.proposal.title || "Proposed SQL"}
        <span className="text-[11px] font-normal text-muted">→ {target}</span>
        {!pending && (
          <span className={`ml-auto text-[11px] ${item.state === "accepted" ? "text-success" : "text-muted"}`}>
            {item.state === "accepted" ? (item.auto ? "Auto-applied" : "Applied") : item.state === "expired" ? "No longer waiting" : "Rejected"}
          </span>
        )}
      </div>
      <div className="max-h-72 overflow-auto border-t border-line font-mono text-[11.5px] leading-snug select-text">
        {(item.before && item.proposal.mode !== "insert_at_cursor" ? diff : diff.filter((d) => d.op !== "del")).map((d, i) => (
          <div
            key={i}
            className={`whitespace-pre-wrap px-2 ${d.op === "add" ? "bg-success/12" : d.op === "del" ? "bg-danger/12 text-muted line-through" : ""}`}
          >
            <span className="mr-1.5 inline-block w-2 select-none text-muted">{d.op === "add" ? "+" : d.op === "del" ? "−" : " "}</span>
            {d.text || " "}
          </div>
        ))}
      </div>
      {pending && (
        <div className="flex justify-end gap-1.5 border-t border-line p-1.5">
          <button className="btn-ghost py-1" onClick={() => onRespond(item.id, false)}>
            Reject
          </button>
          <button className="btn-primary py-1" onClick={() => onRespond(item.id, true)}>
            <Check size={12} /> Accept
          </button>
        </div>
      )}
    </div>
  );
}
