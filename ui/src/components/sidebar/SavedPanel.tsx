import { useMemo, useState } from "react";
import { Bookmark, FileCode2, FolderPlus, Sparkles, Trash2 } from "lucide-react";
import { api, toError } from "../../lib/api";
import type { SavedQuery } from "../../lib/types";
import { sqlPreview } from "../../lib/util";
import { FolderTree, createFolder, dragProps } from "../FolderTree";
import { useStore } from "../../store";
import { ConnDot } from "../ui";
import { kbd } from "../../lib/platform";
import { PanelHeader, SearchBox } from "./primitives";

// ------------------------------------------------------------------ saved queries

export function SavedPanel() {
  const saved = useStore((s) => s.savedQueries);
  const connections = useStore((s) => s.connections);
  const openSaved = useStore((s) => s.openSavedQuery);
  const refresh = useStore((s) => s.refreshSavedQueries);
  const toast = useStore((s) => s.toast);
  const [search, setSearch] = useState("");

  const shown = useMemo(() => {
    const q = search.toLowerCase().trim();
    if (!q) return saved;
    return saved.filter(
      (s) =>
        s.name.toLowerCase().includes(q) ||
        s.sql.toLowerCase().includes(q) ||
        s.tags.some((t) => t.toLowerCase().includes(q)) ||
        (s.description ?? "").toLowerCase().includes(q),
    );
  }, [saved, search]);

  const remove = (q: SavedQuery) =>
    useStore.getState().askConfirm({
      title: `Delete "${q.name}"?`,
      reasons: ["This saved query will be permanently deleted."],
      confirmLabel: "Delete",
      onConfirm: async () => {
        try {
          await api.deleteSavedQuery(q.id);
          await refresh();
          useStore.getState().tabs
            .filter((t) => t.saved_query_id === q.id)
            .forEach((t) => useStore.getState().updateTab(t.id, { saved_query_id: null }));
        } catch (e) {
          toast(toError(e).message, "error");
        }
      },
    });

  const renderQuery = (q: SavedQuery) => {
    const conn = connections.find((c) => c.id === q.connection_id);
    return (
      <div
        {...dragProps("queries", q.id)}
        role="button"
        tabIndex={0}
        onClick={() => openSaved(q)}
        onKeyDown={(e) => e.key === "Enter" && openSaved(q)}
        className="group mb-0.5 cursor-pointer rounded-md px-2 py-1.5 hover:bg-hover"
      >
        <div className="flex items-center gap-1.5">
          <FileCode2 size={13} className="shrink-0 text-muted" />
          <span className="min-w-0 flex-1 truncate font-medium">{q.name}</span>
          {q.ai_example && (
            <span title="Used as an AI example">
              <Sparkles size={11} className="text-accent" />
            </span>
          )}
          <button
            className="icon-btn hidden h-5 w-5 group-hover:flex"
            aria-label={`Delete ${q.name}`}
            onClick={(e) => {
              e.stopPropagation();
              remove(q);
            }}
          >
            <Trash2 size={12} />
          </button>
        </div>
        <div className="mt-0.5 truncate pl-5 font-mono text-[11px] text-muted">{sqlPreview(q.sql, 60)}</div>
        <div className="mt-1 flex flex-wrap items-center gap-1 pl-5">
          {conn && (
            <span className="flex items-center gap-1 text-[10.5px] text-muted">
              <ConnDot color={conn.color} /> {conn.name}
            </span>
          )}
          {q.tags.map((t) => (
            <span key={t} className="rounded bg-panel-2 px-1.5 text-[10.5px] text-muted">
              {t}
            </span>
          ))}
        </div>
      </div>
    );
  };

  return (
    <>
      <PanelHeader title="Saved queries">
        <button className="icon-btn" title="New folder" aria-label="New folder" onClick={() => void createFolder("queries")}>
          <FolderPlus size={14} />
        </button>
      </PanelHeader>
      <SearchBox value={search} onChange={setSearch} placeholder="Search saved queries" />
      <div className="min-h-0 flex-1 overflow-auto px-1.5 pb-3">
        {shown.length === 0 && (
          <div className="px-3 py-8 text-center text-[12.5px] text-muted">
            <Bookmark size={26} className="mx-auto mb-3 opacity-50" />
            {saved.length === 0 ? (
              <>
                No saved queries. Press <span className="kbd">{kbd("⌘S")}</span> in an editor tab to save one.
              </>
            ) : (
              "No matches"
            )}
          </div>
        )}
        <FolderTree kind="queries" items={shown} itemId={(q) => q.id} itemFolder={(q) => q.folder_id} filtering={!!search.trim()} renderItem={renderQuery} />
      </div>
    </>
  );
}
