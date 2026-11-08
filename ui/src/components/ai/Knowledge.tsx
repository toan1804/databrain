import { useCallback, useEffect, useState } from "react";
import { Check, Database, FileDown, FileUp, ListChecks, Loader2, RefreshCw, Trash2, X } from "lucide-react";
import { api, toError } from "../../lib/api";
import type { KnNote, KnObject, KnowledgeView, LearnedView, NotesImportPreview } from "../../lib/types";
import { formatCount, relativeTime } from "../../lib/util";
import { useStore } from "../../store";
import { useAi } from "../../aiStore";
import { open as openDialog, save as saveDialog } from "@tauri-apps/plugin-dialog";
import { exportFileName } from "../../lib/notesImport";
import { ImportNotesDialog } from "../ImportNotesDialog";
import { NoteTargetInput, type TargetState } from "../NoteTargetInput";
import { LearnedSection } from "./LearnedSection";

/** Indexed tables shown at first and added by each "Show more". */
const OBJECT_PAGE = 25;

type Part = "index" | "notes" | "learned";
const PART_KEY = "databrain.knowledge.part";

function savedPart(): Part {
  try {
    const v = localStorage.getItem(PART_KEY);
    return v === "notes" || v === "learned" ? v : "index";
  } catch {
    return "index";
  }
}

export function Knowledge() {
  const connections = useStore((s) => s.connections);
  const activeConn = useStore((s) => s.tabs.find((t) => t.id === s.activeTabId)?.connection_id ?? null);
  const toast = useStore((s) => s.toast);
  const progress = useAi((s) => s.knowledgeProgress);
  const version = useAi((s) => s.knowledgeVersion);
  const indexKnowledge = useAi((s) => s.indexKnowledge);
  const cancelIndex = useAi((s) => s.cancelIndex);
  const [connId, setConnId] = useState<string | null>(activeConn ?? connections[0]?.id ?? null);
  const [data, setData] = useState<KnowledgeView | null>(null);
  const [filter, setFilter] = useState("");
  const [objects, setObjects] = useState<{ list: KnObject[]; total: number } | null>(null);
  const [note, setNote] = useState({ target: "", body: "" });
  const [targetState, setTargetState] = useState<TargetState>({ status: "empty" });
  const targetOk = targetState.status === "empty" || targetState.status === "ok";
  const [importing, setImporting] = useState<NotesImportPreview | null>(null);
  const [part, setPartState] = useState<Part>(savedPart);
  const setPart = (p: Part) => {
    setPartState(p);
    try {
      localStorage.setItem(PART_KEY, p);
    } catch {
      // Remembering the tab is a convenience only.
    }
  };
  const [learned, setLearned] = useState<LearnedView | null>(null);
  const prog = connId ? progress[connId] : undefined;
  const connName = connections.find((c) => c.id === connId)?.name ?? "notes";

  const exportNotes = async () => {
    if (!connId) return;
    const path = await saveDialog({ defaultPath: exportFileName(connName), filters: [{ name: "DataBrain notes", extensions: ["json"] }] });
    if (!path) return;
    try {
      const n = await api.knExportNotes(connId, path);
      toast(`Exported ${n} note${n === 1 ? "" : "s"}. Share the file; others import it in Knowledge.`, "success");
    } catch (e) {
      toast(toError(e).message, "error");
    }
  };
  const importNotes = async () => {
    if (!connId) return;
    const path = await openDialog({ multiple: false, directory: false, filters: [{ name: "DataBrain notes", extensions: ["json"] }] });
    if (typeof path !== "string") return;
    try {
      setImporting(await api.knReadNotesFile(connId, path));
    } catch (e) {
      toast(toError(e).message, "error");
    }
  };

  const load = useCallback(() => {
    if (!connId) return;
    api
      .knGet(connId)
      .then(setData)
      .catch((e) => toast(toError(e).message, "error"));
  }, [connId, toast]);
  // `version` bumps when knowledge changes elsewhere.
  useEffect(load, [load, version]);
  const loadLearned = useCallback(() => {
    if (!connId) return setLearned(null);
    api
      .knLearned(connId)
      .then(setLearned)
      .catch((e) => toast(toError(e).message, "error"));
  }, [connId, toast]);
  useEffect(() => setLearned(null), [connId]);
  useEffect(loadLearned, [loadLearned, version]);

  // Indexed objects: one page at a time, filtered in the workspace DB (an
  // index can hold 10k+ tables; never load them all into the view).
  useEffect(() => {
    if (!connId) return setObjects(null);
    let stale = false;
    const t = setTimeout(
      () =>
        void api
          .knObjects(connId, filter, 0, OBJECT_PAGE)
          .then((p) => !stale && setObjects({ list: p.objects, total: p.total }))
          .catch(() => {}),
      filter ? 150 : 0,
    );
    return () => {
      stale = true;
      clearTimeout(t);
    };
  }, [connId, version, filter]);
  const moreObjects = () => {
    if (!connId || !objects) return;
    void api
      .knObjects(connId, filter, objects.list.length, OBJECT_PAGE)
      .then((p) => setObjects((o) => (o ? { list: [...o.list, ...p.objects], total: p.total } : o)))
      .catch((e) => toast(toError(e).message, "error"));
  };

  const saveNote = async (n: Partial<KnNote>) => {
    if (!connId) return;
    try {
      await api.knSaveNote({
        id: n.id ?? "",
        connection_id: connId,
        target: n.target || null,
        body: n.body ?? "",
        author: n.author ?? "user",
        status: n.status ?? "approved",
        created_at: n.created_at ?? 0,
        replaces: n.replaces ?? null,
      });
      load();
      return true;
    } catch (e) {
      toast(toError(e).message, "error");
      return false;
    }
  };

  const scopeList = connections.find((c) => c.id === connId)?.ai_policy?.index_schemas ?? [];
  const scopeText = scopeList.length === 0 ? "" : scopeList.includes("*") ? "all schemas" : scopeList.join(", ");
  const proposed = data?.notes.filter((n) => n.status === "proposed") ?? [];
  const approved = data?.notes.filter((n) => n.status === "approved") ?? [];

  const partTab = (p: Part, label: string, count: number | null, badge?: number) => (
    <button
      key={p}
      role="tab"
      aria-selected={part === p}
      className={`-mb-px flex items-center gap-1 border-b-2 px-2 py-1.5 text-[12px] ${part === p ? "border-accent font-medium text-fg" : "border-transparent text-muted hover:text-fg"}`}
      onClick={() => setPart(p)}
    >
      {label}
      {count != null && <span className="text-[11px] tabular-nums text-muted">{formatCount(count)}</span>}
      {!!badge && (
        <span className="rounded-full bg-warning/20 px-1.5 text-[10px] font-semibold text-warning" title={`${badge} proposed by the AI to review`}>
          {badge}
        </span>
      )}
    </button>
  );

  return (
    <div className="flex min-h-0 flex-1 flex-col text-[12.5px]">
      <div className="shrink-0 space-y-2 px-3 pt-3">
        <div className="flex items-center gap-2">
          <Database size={13} className="text-muted" />
          <select className="field h-7 min-w-0 flex-1 py-0" aria-label="Connection" value={connId ?? ""} onChange={(e) => setConnId(e.target.value || null)}>
            {connections.map((c) => (
              <option key={c.id} value={c.id}>
                {c.name}
              </option>
            ))}
          </select>
        </div>
        <div className="flex border-b border-line" role="tablist" aria-label="Knowledge">
          {partTab("index", "Index", data?.state ? data.state.objects : null)}
          {partTab("notes", "Notes", approved.length, proposed.length)}
          {partTab("learned", "Learned", learned?.enabled && learned.supported ? learned.joins.length : null)}
        </div>
      </div>
      <div className="min-h-0 flex-1 space-y-3 overflow-auto p-3">
      {part === "index" && (
      <div className="rounded-lg border border-line p-2.5">
        <div className="flex flex-wrap items-center gap-2">
          <div className="min-w-[9rem] flex-1">
            <div className="font-medium">Schema index</div>
            <div className="text-[11.5px] text-muted">
              {data?.state
                ? `${formatCount(data.state.objects)} objects · ${data.state.schemas.length} schemas · ${relativeTime(data.state.indexed_at)}`
                : "Not indexed yet. The assistant uses this metadata to find relevant tables."}
            </div>
          </div>
          <div className="flex flex-wrap gap-1.5">
          {prog ? (
            <button className="btn-ghost border border-line py-1" onClick={() => connId && void cancelIndex(connId)} title="Stop indexing; finished schemas are kept">
              <X size={12} /> Cancel
            </button>
          ) : (
            <>
              <button
                className="btn-ghost border border-line py-1"
                disabled={!connId}
                title="Choose catalogs and schemas to index"
                onClick={() => connId && void indexKnowledge(connId, { choose: true })}
              >
                <ListChecks size={12} /> Scope…
              </button>
              <button
                className="btn-ghost border border-line py-1"
                disabled={!connId}
                title={data?.state ? "Re-read schemas whose tables or columns changed since the last run" : "Index tables and columns for the AI"}
                onClick={() => connId && void indexKnowledge(connId)}
              >
                <RefreshCw size={12} /> {data?.state ? "Update" : "Index"}
              </button>
              {data?.state && (
                <button
                  className="btn-ghost border border-line py-1"
                  disabled={!connId}
                  title="Re-read every schema, changed or not"
                  onClick={() => connId && void indexKnowledge(connId, { full: true })}
                >
                  Rebuild
                </button>
              )}
            </>
          )}
          </div>
        </div>
        {scopeText && <div className="mt-1 truncate text-[11px] text-muted" title={scopeText}>Scope: {scopeText}</div>}
        {prog && (
          <div className="mt-2">
            <div className="h-1.5 overflow-hidden rounded bg-panel-2">
              <div className="h-full bg-accent transition-all" style={{ width: `${prog.total ? (prog.done / prog.total) * 100 : 5}%` }} />
            </div>
            <div className="mt-1 flex items-center gap-1.5 truncate text-[11px] text-muted">
              <Loader2 size={11} className="shrink-0 animate-spin" />
              {prog.total ? `${prog.done}/${prog.total} schemas${prog.schema ? ` · ${prog.schema}` : ""}` : prog.schema || "Starting…"}
            </div>
          </div>
        )}
        {data?.state?.error && <div className="mt-1 text-[11.5px] text-danger">{data.state.error}</div>}
        <p className="mt-2 text-[11px] text-muted">
          Only metadata (names, types, comments, keys) is indexed locally. What is sent to the model is controlled per connection
          (Edit connection → AI).
        </p>
      </div>
      )}

      {part === "notes" && proposed.length > 0 && (
        <div>
          <div className="mb-1 text-[11px] font-semibold uppercase tracking-wide text-muted">Proposed by AI · review</div>
          {proposed.map((n) => {
            const old = n.replaces ? approved.find((a) => a.id === n.replaces) : undefined;
            return (
            <div key={n.id} className="mb-1.5 rounded-lg border border-warning/40 bg-warning/5 p-2">
              <div className="flex items-center gap-1.5 text-[10.5px] font-medium uppercase text-muted">
                {old ? "Update" : "New"}
                {n.target && <span className="font-mono normal-case">{n.target}</span>}
              </div>
              {old && <div className="whitespace-pre-wrap text-muted line-through decoration-muted/60">{old.body}</div>}
              <div className="whitespace-pre-wrap">{n.body}</div>
              <div className="mt-1.5 flex justify-end gap-1">
                <button className="btn-ghost py-0.5" onClick={() => void api.knDeleteNote(n.id).then(load)}>
                  Discard
                </button>
                <button className="btn-primary py-0.5" onClick={() => void saveNote({ ...n, status: "approved" })}>
                  <Check size={12} /> {old ? "Apply update" : "Approve"}
                </button>
              </div>
            </div>
            );
          })}
        </div>
      )}

      {part === "notes" && (
      <div>
        <div className="mb-1 flex flex-wrap items-center gap-1">
          <span className="text-[11px] font-semibold uppercase tracking-wide text-muted">Notes & glossary</span>
          <button className="btn-ghost ml-auto py-0.5 text-[11.5px]" disabled={!connId} onClick={() => void importNotes()} title="Import a notes file shared by someone else">
            <FileUp size={12} /> Import…
          </button>
          <button className="btn-ghost py-0.5 text-[11.5px]" disabled={!connId || approved.length === 0} onClick={() => void exportNotes()} title="Save approved notes to a file you can share">
            <FileDown size={12} /> Export…
          </button>
        </div>
        {importing && connId && (
          <ImportNotesDialog
            connectionId={connId}
            preview={importing}
            onClose={() => setImporting(null)}
            onDone={() => {
              setImporting(null);
              load();
            }}
          />
        )}
        <form
          className="mb-2 space-y-1.5"
          onSubmit={(e) => {
            e.preventDefault();
            if (!note.body.trim() || !targetOk) return;
            void saveNote({ target: note.target.trim(), body: note.body.trim() }).then((ok) => ok && setNote({ target: "", body: "" }));
          }}
        >
          {connId && (
            <NoteTargetInput
              connId={connId}
              value={note.target}
              onChange={(target) => setNote((n) => ({ ...n, target }))}
              onState={setTargetState}
            />
          )}
          <textarea
            className="field min-h-[52px]"
            placeholder='Business rule, e.g. "Active customer = status IN (1, 2)"'
            aria-label="Note"
            value={note.body}
            onChange={(e) => setNote({ ...note, body: e.target.value })}
          />
          <div className="flex justify-end">
            <button type="submit" className="btn-primary py-1" disabled={!note.body.trim() || !targetOk} title={targetOk ? undefined : "Fix the tables first"}>
              Add note
            </button>
          </div>
        </form>
        {approved.map((n) => (
          <div key={n.id} className="group mb-1 rounded-md bg-panel-2 px-2 py-1.5">
            <div className="flex items-start gap-1">
              <div className="min-w-0 flex-1">
                {n.target && <div className="font-mono text-[11px] text-muted">{n.target}</div>}
                <div className="whitespace-pre-wrap">{n.body}</div>
              </div>
              <button className="icon-btn hidden h-5 w-5 group-hover:flex" aria-label="Delete note" onClick={() => void api.knDeleteNote(n.id).then(load)}>
                <Trash2 size={11} />
              </button>
            </div>
          </div>
        ))}
      </div>

      )}

      {part === "learned" && connId && <LearnedSection connId={connId} view={learned} reload={loadLearned} onMakeNote={(target, body) => saveNote({ target, body })} />}

      {part === "index" && objects && (objects.total > 0 || filter) && (
        <div>
          <div className="mb-1 flex flex-wrap items-center gap-2">
            <span className="text-[11px] font-semibold uppercase tracking-wide text-muted">Indexed objects</span>
            <span className="text-[11px] text-muted">{formatCount(objects.total)}</span>
            <input className="field ml-auto h-6 w-36 min-w-0 py-0 text-[11.5px]" placeholder="Filter" aria-label="Filter objects" value={filter} onChange={(e) => setFilter(e.target.value)} />
          </div>
          <div className="space-y-0.5">
            {objects.list.map((o) => (
              <div key={`${o.schema}.${o.name}`} className="rounded px-1.5 py-1 hover:bg-hover" title={o.columns.map((c) => `${c.name} ${c.data_type}`).join("\n")}>
                <span className="font-mono text-[11.5px]">
                  {o.schema}.{o.name}
                </span>
                <span className="ml-1.5 text-[10.5px] text-muted">
                  {o.kind} · {o.columns.length} cols
                </span>
                {o.comment && <div className="truncate text-[11px] text-muted">{o.comment}</div>}
              </div>
            ))}
            {objects.total === 0 && <div className="text-[11px] text-muted">No match</div>}
            {objects.total > objects.list.length && (
              <button className="btn-ghost py-0.5 text-[11.5px]" onClick={moreObjects}>
                Show {formatCount(Math.min(OBJECT_PAGE, objects.total - objects.list.length))} more of {formatCount(objects.total - objects.list.length)}
              </button>
            )}
          </div>
        </div>
      )}
      </div>
    </div>
  );
}
