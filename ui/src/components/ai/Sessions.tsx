import { useCallback, useEffect, useState } from "react";
import { Trash2 } from "lucide-react";
import { api, toError } from "../../lib/api";
import type { AiSessionRecord } from "../../lib/types";
import { relativeTime } from "../../lib/util";
import { useStore } from "../../store";
import { useAi } from "../../aiStore";
import { ConnDot } from "../ui";

export function Sessions() {
  const version = useAi((s) => s.sessionsVersion);
  const loadSession = useAi((s) => s.loadSession);
  const current = useAi((s) => s.sessionId);
  const openSessions = useAi((s) => s.chats.map((c) => c.sessionId).join(","));
  const connections = useStore((s) => s.connections);
  const toast = useStore((s) => s.toast);
  const [items, setItems] = useState<AiSessionRecord[]>([]);
  const [q, setQ] = useState("");

  const load = useCallback(
    () =>
      api
        .aiSessions(null)
        .then(setItems)
        .catch((e) => toast(toError(e).message, "error")),
    [toast],
  );
  // `version` bumps when sessions change elsewhere.
  useEffect(() => {
    void load();
  }, [load, version]);

  const shown = items.filter((s) => s.title.toLowerCase().includes(q.toLowerCase()));
  return (
    <div className="flex min-h-0 flex-1 flex-col">
      <div className="p-2">
        <input className="field py-1" placeholder="Search conversations" aria-label="Search conversations" value={q} onChange={(e) => setQ(e.target.value)} />
      </div>
      <div className="min-h-0 flex-1 overflow-auto px-1.5 pb-2">
        {shown.length === 0 && <div className="py-8 text-center text-[12.5px] text-muted">No conversations yet</div>}
        {shown.map((s) => {
          const conn = connections.find((c) => c.id === s.connection_id);
          return (
            <div
              key={s.id}
              role="button"
              tabIndex={0}
              onClick={() => void loadSession(s.id)}
              onKeyDown={(e) => e.key === "Enter" && void loadSession(s.id)}
              className={`group mb-0.5 cursor-pointer rounded-md px-2 py-1.5 hover:bg-hover ${current === s.id ? "bg-hover" : ""}`}
            >
              <div className="flex items-center gap-1.5">
                <span className="min-w-0 flex-1 truncate text-[12.5px]">{s.title}</span>
                {openSessions.split(",").includes(s.id) && (
                  <span className="shrink-0 rounded bg-panel-2 px-1 text-[10px] text-muted" title="Open in a conversation tab">
                    open
                  </span>
                )}
                <button
                  className="icon-btn hidden h-5 w-5 group-hover:flex"
                  aria-label={`Delete ${s.title}`}
                  onClick={(e) => {
                    e.stopPropagation();
                    void api.aiDeleteSession(s.id).then(() => {
                      useAi.getState().forgetSession(s.id);
                      return load();
                    });
                  }}
                >
                  <Trash2 size={11} />
                </button>
              </div>
              <div className="mt-0.5 flex items-center gap-2 text-[10.5px] text-muted">
                {conn && (
                  <span className="flex items-center gap-1">
                    <ConnDot color={conn.color} /> {conn.name}
                  </span>
                )}
                <span>{relativeTime(s.updated_at)}</span>
                {s.model && <span className="truncate">{s.model}</span>}
              </div>
            </div>
          );
        })}
      </div>
    </div>
  );
}
