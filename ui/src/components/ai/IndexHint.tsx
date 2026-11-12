// A hint above the chat when the knowledge index doesn't cover the chat's
// connection: nothing indexed, or far fewer tables than the explorer lists.
// The assistant finds tables through the index, so it would guess.
import { useEffect, useMemo, useState } from "react";
import { BookOpen, Loader2, X } from "lucide-react";
import { api } from "../../lib/api";
import { explorerTables, indexCoverage } from "../../lib/indexCoverage";
import type { KnState } from "../../lib/types";
import { formatCount } from "../../lib/util";
import { useAi } from "../../aiStore";
import { useStore } from "../../store";

const DISMISS_KEY = "db.ai.indexHint";
/** Waits before reading the index state again after a failed read. */
export const RETRY_MS = [1_000, 3_000, 10_000];

function dismissed(key: string): boolean {
  try {
    return localStorage.getItem(`${DISMISS_KEY}.${key}`) === "1";
  } catch {
    return false;
  }
}

export function IndexHint({ connId }: { connId: string | null }) {
  const conn = useStore((s) => s.connections.find((c) => c.id === connId));
  // Counted only when the explorer's objects change, not on every store update.
  const objects = useStore((s) => s.objects);
  const scope = conn?.ai_policy?.index_schemas;
  const total = useMemo(() => (connId ? explorerTables(objects, connId, scope ?? []) : null), [objects, connId, scope]);
  const version = useAi((s) => s.knowledgeVersion);
  const progress = useAi((s) => (connId ? s.knowledgeProgress[connId] : undefined));
  const openKnowledge = useAi((s) => s.openKnowledge);
  const [state, setState] = useState<{ conn: string; version: number; value: KnState | null } | null>(null);
  const [, bump] = useState(0);

  useEffect(() => {
    if (!connId) return;
    let live = true;
    let timer: ReturnType<typeof setTimeout> | undefined;
    // A failed read is tried again a few times, so the hint doesn't stay
    // hidden until the index changes next.
    const load = (attempt: number) => {
      api
        .knGet(connId)
        .then((v) => live && setState({ conn: connId, version, value: v.state }))
        .catch(() => {
          if (live && attempt < RETRY_MS.length) timer = setTimeout(() => load(attempt + 1), RETRY_MS[attempt]);
        });
    };
    load(0);
    return () => {
      live = false;
      clearTimeout(timer);
    };
  }, [connId, version]);

  // The index isn't used: AI off, or table structure not shared.
  if (!connId || !conn || conn.ai_policy?.ai_enabled === false || conn.ai_policy?.share_metadata === false) return null;
  if (progress) {
    return (
      <div className="flex items-center gap-1.5 border-b border-line px-3 py-1.5 text-[11.5px] text-muted" role="status">
        <Loader2 size={12} className="animate-spin" /> Indexing {conn.name}: {progress.schema}
        {progress.total > 0 && ` (${progress.done} of ${progress.total})`}
      </div>
    );
  }
  // Loaded for an older index (one just finished): wait for the new one.
  if (!state || state.conn !== connId || state.version !== version) return null;
  const cov = indexCoverage(state.value, total);
  if (!cov) return null;
  const key = `${connId}.${cov.kind}`;
  if (dismissed(key)) return null;
  const dismiss = () => {
    try {
      localStorage.setItem(`${DISMISS_KEY}.${key}`, "1");
    } catch {
      // Dismissing is a convenience: the hint comes back next time.
    }
    bump((n) => n + 1);
  };
  return (
    <div className="flex items-start gap-2 border-b border-warning/30 bg-warning/10 px-3 py-2 text-[12px]" role="note">
      <BookOpen size={14} className="mt-0.5 shrink-0 text-warning" />
      <div className="min-w-0 flex-1 space-y-1">
        {cov.kind === "none" ? (
          <p>
            The assistant may not use any of <span className="font-medium">{conn.name}</span>'s tables yet, so it can only guess table and column
            names. Choose the schemas it may use to get SQL that fits your schema.
          </p>
        ) : (
          <p>
            The AI may use {formatCount(cov.indexed)} of the {formatCount(cov.total)} tables in the explorer for{" "}
            <span className="font-medium">{conn.name}</span>, so it may not know the tables you ask about. Allow the other schemas to fill the gaps.
          </p>
        )}
        <button className="btn-ghost border border-line bg-panel py-0.5 text-[11.5px]" onClick={() => openKnowledge(connId)}>
          <BookOpen size={12} /> {cov.kind === "none" ? "Choose in Knowledge" : "Review in Knowledge"}
        </button>
      </div>
      <button className="icon-btn shrink-0" title="Don't show again for this connection" aria-label="Dismiss" onClick={dismiss}>
        <X size={13} />
      </button>
    </div>
  );
}
