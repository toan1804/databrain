import { useEffect, useState } from "react";
import { FolderPlus, NotebookPen, Loader2, Play, Plus, Trash2, Workflow, Square } from "lucide-react";
import { api, toError } from "../../lib/api";
import type { NotebookSummary } from "../../lib/types";
import { relativeTime } from "../../lib/util";
import { FolderTree, createFolder, dragProps } from "../FolderTree";
import { useJobs } from "../../jobsStore";
import { scheduleText } from "../../lib/jobGraph";
import { useStore } from "../../store";
import { ConnDot } from "../ui";
import { PanelHeader, SearchBox } from "./primitives";

// ------------------------------------------------------------------ notebooks

export function NotebooksPanel() {
  const notebooks = useStore((s) => s.notebooks);
  const connections = useStore((s) => s.connections);
  const openNotebook = useStore((s) => s.openNotebook);
  const newNotebook = useStore((s) => s.newNotebook);
  const refresh = useStore((s) => s.refreshNotebooks);
  const toast = useStore((s) => s.toast);
  const [search, setSearch] = useState("");
  const shown = notebooks.filter((n) => n.name.toLowerCase().includes(search.toLowerCase().trim()));

  useEffect(() => {
    void refresh().catch(() => {});
  }, [refresh]);

  const remove = (n: NotebookSummary) =>
    useStore.getState().askConfirm({
      title: `Delete notebook "${n.name}"?`,
      reasons: ["All cells of this notebook are deleted."],
      confirmLabel: "Delete",
      onConfirm: async () => {
        try {
          const st = useStore.getState();
          st.tabs.filter((t) => t.notebook_id === n.id).forEach((t) => st.closeTab(t.id));
          await api.deleteNotebook(n.id);
          await refresh();
        } catch (e) {
          toast(toError(e).message, "error");
        }
      },
    });

  return (
    <>
      <PanelHeader title="Notebooks">
        <button className="icon-btn" title="New folder" aria-label="New folder" onClick={() => void createFolder("notebooks")}>
          <FolderPlus size={14} />
        </button>
        <button className="icon-btn" title="New notebook" aria-label="New notebook" onClick={() => void newNotebook()}>
          <Plus size={15} />
        </button>
      </PanelHeader>
      {notebooks.length > 4 && <SearchBox value={search} onChange={setSearch} placeholder="Search notebooks" />}
      <div className="min-h-0 flex-1 overflow-auto px-1.5 pb-3">
        {notebooks.length === 0 && (
          <div className="px-3 py-8 text-center text-[12.5px] text-muted">
            <NotebookPen size={26} className="mx-auto mb-3 opacity-50" />
            Notebooks mix SQL and notes. Cells run on the notebook's connection and share one session.
            <button className="btn-primary mx-auto mt-3" onClick={() => void newNotebook()}>
              <Plus size={14} /> New notebook
            </button>
          </div>
        )}
        <FolderTree
          kind="notebooks"
          newItem={{ label: "New notebook", create: (folderId) => void newNotebook(null, folderId) }}
          items={shown}
          itemId={(n) => n.id}
          itemFolder={(n) => n.folder_id}
          filtering={!!search.trim()}
          renderItem={(n) => {
            const conn = connections.find((c) => c.id === n.connection_id);
            return (
              <div
                {...dragProps("notebooks", n.id)}
                role="button"
                tabIndex={0}
                onClick={() => openNotebook(n.id, n.name)}
                onKeyDown={(e) => e.key === "Enter" && openNotebook(n.id, n.name)}
                className="group mb-0.5 cursor-pointer rounded-md px-2 py-1.5 hover:bg-hover"
              >
                <div className="flex items-center gap-1.5">
                  <NotebookPen size={13} className="shrink-0 text-muted" />
                  <span className="min-w-0 flex-1 truncate font-medium">{n.name}</span>
                  <button
                    className="icon-btn hidden h-5 w-5 group-hover:flex"
                    aria-label={`Delete ${n.name}`}
                    onClick={(e) => {
                      e.stopPropagation();
                      remove(n);
                    }}
                  >
                    <Trash2 size={12} />
                  </button>
                </div>
                <div className="mt-0.5 flex items-center gap-2 pl-5 text-[10.5px] text-muted">
                  {conn && (
                    <span className="flex items-center gap-1">
                      <ConnDot color={conn.color} /> {conn.name}
                    </span>
                  )}
                  <span>{n.cell_count} cells</span>
                  <span>{relativeTime(n.updated_at)}</span>
                </div>
              </div>
            );
          }}
        />
      </div>
    </>
  );
}

// ------------------------------------------------------------------ jobs

export function JobsPanel() {
  const jobs = useJobs((s) => s.jobs);
  const running = useJobs((s) => s.running);
  const elsewhere = useJobs((s) => s.elsewhere);
  const live = useJobs((s) => s.runs);
  const [search, setSearch] = useState("");
  const shown = jobs.filter((j) => j.name.toLowerCase().includes(search.toLowerCase().trim()));
  useEffect(() => {
    void useJobs.getState().init().catch(() => {});
    void useJobs.getState().refresh().catch(() => {});
  }, []);
  return (
    <>
      <PanelHeader title="Jobs">
        <button className="icon-btn" title="New job" aria-label="New job" onClick={() => void useJobs.getState().create()}>
          <Plus size={15} />
        </button>
      </PanelHeader>
      {jobs.length > 4 && <SearchBox value={search} onChange={setSearch} placeholder="Search jobs" />}
      <div className="min-h-0 flex-1 overflow-auto px-1.5 pb-3">
        {jobs.length === 0 && (
          <div className="px-3 py-8 text-center text-[12.5px] text-muted">
            <Workflow size={26} className="mx-auto mb-3 opacity-50" />
            Jobs run SQL steps in order: query several connections, combine the results with DuckDB, load them into another database. By hand or on a schedule.
            <button className="btn-primary mx-auto mt-3" onClick={() => void useJobs.getState().create()}>
              <Plus size={14} /> New job
            </button>
          </div>
        )}
        {shown.map((j) => {
          const isRunning = !!running[j.id];
          const last = live[j.id] ?? j.last_run;
          return (
            <div
              key={j.id}
              role="button"
              tabIndex={0}
              onClick={() => useJobs.getState().open(j.id, j.name)}
              onKeyDown={(e) => e.key === "Enter" && useJobs.getState().open(j.id, j.name)}
              className="group mb-0.5 cursor-pointer rounded-md px-2 py-1.5 hover:bg-hover"
            >
              <div className="flex items-center gap-1.5">
                {isRunning ? (
                  <Loader2 size={13} className="shrink-0 animate-spin text-accent" />
                ) : (
                  <Workflow size={13} className={`shrink-0 ${last?.status === "error" ? "text-danger" : last?.status === "success" ? "text-success" : "text-muted"}`} />
                )}
                <span className="min-w-0 flex-1 truncate font-medium">{j.name}</span>
                <button
                  className="icon-btn hidden h-5 w-5 group-hover:flex"
                  aria-label={isRunning ? `Stop ${j.name}` : `Run ${j.name}`}
                  title={isRunning ? "Stop" : "Run now"}
                  onClick={(e) => {
                    e.stopPropagation();
                    void (isRunning ? useJobs.getState().cancel(j.id) : useJobs.getState().run(j.id));
                  }}
                >
                  {isRunning ? <Square size={11} /> : <Play size={12} />}
                </button>
                <button
                  className="icon-btn hidden h-5 w-5 group-hover:flex"
                  aria-label={`Delete ${j.name}`}
                  onClick={(e) => {
                    e.stopPropagation();
                    useJobs.getState().remove(j);
                  }}
                >
                  <Trash2 size={12} />
                </button>
              </div>
              <div className="mt-0.5 flex flex-wrap items-center gap-x-2 pl-5 text-[10.5px] text-muted">
                <span>{j.node_count} steps</span>
                <span>{scheduleText(j.schedule)}</span>
                {elsewhere[j.id] && !isRunning && (
                  <span className="text-accent" title="The background runner is running it now (Settings → Jobs)">
                    running in background
                  </span>
                )}
                {j.schedule.enabled && j.next_run_at && <span title={new Date(j.next_run_at).toLocaleString()}>next {new Date(j.next_run_at).toLocaleTimeString([], { hour: "2-digit", minute: "2-digit" })}</span>}
                {last && !isRunning && (
                  <span className={last.status === "error" ? "text-danger" : undefined} title={last.error ?? undefined}>
                    {last.status} {relativeTime(last.started_at)}
                  </span>
                )}
              </div>
            </div>
          );
        })}
      </div>
    </>
  );
}
