import { useEffect, useMemo, useRef, useState, type ReactNode } from "react";
import { createPortal } from "react-dom";
import { Bookmark, Database, FileCode2, FileSearch, FolderOpen, GitCompare, History, LayoutDashboard, Moon, NotebookPen, Play, Plus, Save, Search, Settings2, Sparkles, Square, Sun, Workflow, X } from "lucide-react";
import { useAi } from "../aiStore";
import { useJobs } from "../jobsStore";
import { openOutput, openResultsQuery, outputLabel, outputRef } from "../outputs";
import { queryLocalFile } from "./Sidebar";
import { exportConnections } from "./ConnectionsIoDialog";
import { openCompare } from "./CompareTab";
import { fuzzyMatch, relativeTime, sqlPreview } from "../lib/util";
import { baseName, isQueryTab, useStore } from "../store";
import { ConnDot } from "./ui";
import { kbd } from "../lib/platform";

interface Command {
  id: string;
  group: string;
  label: string;
  detail?: string;
  icon: ReactNode;
  hint?: string;
  run: () => void;
}

export function CommandPalette() {
  const open = useStore((s) => s.paletteOpen);
  if (!open) return null;
  return <Palette />;
}

function Palette() {
  const st = useStore();
  const close = () => st.setPaletteOpen(false);
  const [q, setQ] = useState("");
  const [idx, setIdx] = useState(0);
  const input = useRef<HTMLInputElement>(null);
  const list = useRef<HTMLDivElement>(null);

  const jobs = useJobs((s) => s.jobs);
  const jobsRunning = useJobs((s) => s.running);
  useEffect(() => {
    void useJobs.getState().init().catch(() => {});
  }, []);

  const commands = useMemo<Command[]>(() => {
    const active = st.tabs.find((t) => t.id === st.activeTabId);
    const cmds: Command[] = [
      { id: "new-tab", group: "Actions", label: "New query tab", icon: <Plus size={14} />, hint: kbd("⌘T"), run: () => st.newTab() },
      ...(st.closedTabs.length
        ? [{ id: "reopen-tab", group: "Actions", label: `Reopen closed tab “${st.closedTabs[st.closedTabs.length - 1].tab.title}”`, icon: <History size={14} />, hint: kbd("⇧⌘T"), run: () => void st.reopenClosedTab() }]
        : []),
      {
        id: "find-table",
        group: "Actions",
        label: "Find table…",
        icon: <Search size={14} />,
        hint: kbd("⌘P"),
        // After the palette closes, so focus lands in the search box.
        run: () => setTimeout(() => st.openCatalogSearch(), 0),
      },
      { id: "new-conn", group: "Actions", label: "New connection", icon: <Database size={14} />, run: () => st.openConnectionDialog(null) },
      { id: "import-conns", group: "Actions", label: "Import connections (DBeaver, DataGrip, pgpass, DataBrain file)…", icon: <Database size={14} />, run: () => st.setImportConnectionsOpen(true) },
      { id: "export-conns", group: "Actions", label: "Export connections (without passwords)…", icon: <Database size={14} />, run: () => void exportConnections() },
      { id: "compare", group: "Actions", label: "Compare schemas…", icon: <GitCompare size={14} />, run: () => openCompare() },
      ...st.dashboards.map((d) => ({ id: `dash:${d.id}`, group: "Dashboards", label: d.name, detail: `${d.charts.length} chart${d.charts.length === 1 ? "" : "s"}`, icon: <LayoutDashboard size={14} />, run: () => st.openDashboard(d.id) })),
      { id: "open-file", group: "Files", label: "Open SQL file…", icon: <FolderOpen size={14} />, hint: kbd("⌘O"), run: () => void st.openSqlFile() },
      ...(isQueryTab(active)
        ? [{ id: "save-file", group: "Files", label: active.file_path ? `Save to ${baseName(active.file_path)}` : "Save tab to a file…", icon: <Save size={14} />, hint: kbd(active.file_path ? "⌘S" : "⇧⌘S"), run: () => void st.saveTabFile(active.id) }]
        : []),
      ...st.recentFiles.map((p) => ({ id: `recent:${p}`, group: "Files", label: baseName(p), detail: p, icon: <FileCode2 size={14} />, run: () => void st.openSqlFile(p) })),
      {
        id: "theme",
        group: "Actions",
        label: st.theme === "dark" ? "Switch to light theme" : "Switch to dark theme",
        icon: st.theme === "dark" ? <Sun size={14} /> : <Moon size={14} />,
        run: () => st.setTheme(st.theme === "dark" ? "light" : "dark"),
      },
      { id: "new-nb", group: "Actions", label: "New notebook", icon: <NotebookPen size={14} />, run: () => void st.newNotebook() },
      { id: "new-job", group: "Actions", label: "New job", icon: <Workflow size={14} />, run: () => void useJobs.getState().create() },
      { id: "jobs", group: "Actions", label: "Show jobs", icon: <Workflow size={14} />, run: () => st.setSidebarPanel("jobs") },
      { id: "file", group: "Actions", label: "Query a local file (CSV, Parquet, JSON, Excel)…", icon: <FileSearch size={14} />, run: () => void queryLocalFile() },
      { id: "import-file", group: "Actions", label: "Import a file (CSV, Excel, Parquet, JSON) into a table…", icon: <FileSearch size={14} />, run: () => void queryLocalFile(true) },
      { id: "ai", group: "AI", label: "Open AI assistant", icon: <Sparkles size={14} />, hint: kbd("⌘L"), run: () => useAi.getState().setOpen(true, "chat") },
      { id: "ai-kn", group: "AI", label: "AI knowledge (index schema, notes)", icon: <Sparkles size={14} />, run: () => useAi.getState().setOpen(true, "knowledge") },
      { id: "settings", group: "Actions", label: "Settings: appearance, AI providers, MCP", icon: <Settings2 size={14} />, hint: kbd("⌘,"), run: () => st.setSettingsOpen(true) },
      { id: "outputs", group: "Actions", label: "Show outputs", icon: <Bookmark size={14} />, run: () => st.setSidebarPanel("outputs") },
      {
        id: "query-outputs",
        group: "Actions",
        label: "Query outputs with SQL (results.*)",
        icon: <Search size={14} />,
        run: () => void openResultsQuery(st.outputs[0] ? `SELECT *\nFROM ${outputRef(st.outputs[0])}\nLIMIT 100;` : "SELECT 1;", "Results", false),
      },
      { id: "saved", group: "Actions", label: "Show saved queries", icon: <Bookmark size={14} />, run: () => st.setSidebarPanel("saved") },
      { id: "history", group: "Actions", label: "Show history", icon: <History size={14} />, run: () => st.setSidebarPanel("history") },
    ];
    if (active) {
      cmds.push({
        id: "save",
        group: "Actions",
        label: "Save current query",
        icon: <Bookmark size={14} />,
        hint: kbd("⌘S"),
        run: () => void st.saveTabQuery(active.id),
      });
    }
    for (const c of st.connections) {
      cmds.push({
        id: `conn-${c.id}`,
        group: "Use connection",
        label: c.name,
        detail: c.config.kind,
        icon: <ConnDot color={c.color} connected={c.connected} />,
        run: () => active && st.updateTab(active.id, { connection_id: c.id }),
      });
    }
    for (const n of st.notebooks) {
      cmds.push({ id: `nb-${n.id}`, group: "Notebooks", label: n.name, detail: `${n.cell_count} cells`, icon: <NotebookPen size={14} />, run: () => st.openNotebook(n.id, n.name) });
    }
    for (const j of jobs) {
      const run = jobsRunning[j.id];
      const last = j.last_run;
      const detail = [
        `${j.node_count} step${j.node_count === 1 ? "" : "s"}`,
        run ? "running" : last ? `last run ${last.status} ${relativeTime(last.started_at)}` : null,
        j.schedule.enabled ? "scheduled" : null,
      ]
        .filter(Boolean)
        .join(" · ");
      const ui = useJobs.getState();
      cmds.push({ id: `job-${j.id}`, group: "Jobs", label: j.name, detail, icon: <Workflow size={14} />, run: () => ui.open(j.id, j.name) });
      cmds.push(
        run
          ? { id: `job-stop-${j.id}`, group: "Jobs", label: `Stop job: ${j.name}`, icon: <Square size={13} />, run: () => void ui.cancel(j.id) }
          : { id: `job-run-${j.id}`, group: "Jobs", label: `Run job: ${j.name}`, detail: `${j.node_count} steps`, icon: <Play size={14} />, run: () => void ui.run(j.id) },
      );
    }
    for (const o of st.outputs.slice(0, 50)) {
      if (o.state === "evicted") continue;
      cmds.push({
        id: `out-${o.handle}`,
        group: "Outputs",
        label: `${outputLabel(o)}${o.name ? ` (${o.handle})` : ""}`,
        detail: `${o.connection_name} · ${o.rows.toLocaleString()} rows`,
        icon: <Search size={14} />,
        run: () => void openOutput(o),
      });
    }
    for (const t of st.tabs) {
      cmds.push({ id: `tab-${t.id}`, group: "Open tabs", label: t.title, icon: <FileCode2 size={14} />, run: () => st.setActiveTab(t.id) });
    }
    for (const s of st.savedQueries) {
      cmds.push({
        id: `sq-${s.id}`,
        group: "Saved queries",
        label: s.name,
        detail: sqlPreview(s.sql, 60),
        icon: <Bookmark size={14} />,
        run: () => st.openSavedQuery(s),
      });
    }
    return cmds;
  }, [st, jobs, jobsRunning]);

  const shown = useMemo(
    () => commands.filter((c) => fuzzyMatch(q, `${c.group} ${c.label} ${c.detail ?? ""}`)).slice(0, 60),
    [commands, q],
  );

  useEffect(() => setIdx(0), [q]);
  useEffect(() => input.current?.focus(), []);
  useEffect(() => {
    list.current?.querySelector(`[data-idx="${idx}"]`)?.scrollIntoView({ block: "nearest" });
  }, [idx]);

  const exec = (c: Command | undefined) => {
    if (!c) return;
    close();
    c.run();
  };

  let lastGroup = "";
  return createPortal(
    <div className="fixed inset-0 z-50 flex items-start justify-center bg-black/40 pt-[14vh]" onMouseDown={(e) => e.target === e.currentTarget && close()}>
      <div className="w-[560px] overflow-hidden rounded-xl border border-line bg-panel shadow-2xl" role="dialog" aria-label="Command palette">
        <div className="flex items-center gap-2 border-b border-line px-3">
          <Search size={15} className="text-muted" />
          <input
            ref={input}
            className="h-11 flex-1 bg-transparent text-[14px] outline-none placeholder:text-muted"
            placeholder="Type a command, connection or saved query…"
            value={q}
            aria-label="Search commands"
            onChange={(e) => setQ(e.target.value)}
            onKeyDown={(e) => {
              if (e.key === "ArrowDown") {
                e.preventDefault();
                setIdx((i) => Math.min(i + 1, shown.length - 1));
              } else if (e.key === "ArrowUp") {
                e.preventDefault();
                setIdx((i) => Math.max(i - 1, 0));
              } else if (e.key === "Enter") {
                e.preventDefault();
                exec(shown[idx]);
              } else if (e.key === "Escape") {
                e.preventDefault();
                close();
              }
            }}
          />
          <button className="icon-btn" aria-label="Close" onClick={close}>
            <X size={14} />
          </button>
        </div>
        <div ref={list} className="max-h-[50vh] overflow-auto p-1.5" role="listbox">
          {shown.length === 0 && <div className="px-3 py-6 text-center text-muted">No results</div>}
          {shown.map((c, i) => {
            const header = c.group !== lastGroup ? c.group : null;
            lastGroup = c.group;
            return (
              <div key={c.id}>
                {header && <div className="px-2 pb-1 pt-2 text-[10.5px] font-semibold uppercase tracking-wider text-muted">{header}</div>}
                <div
                  role="option"
                  aria-selected={i === idx}
                  data-idx={i}
                  onMouseMove={() => setIdx(i)}
                  onClick={() => exec(c)}
                  className={`flex cursor-pointer items-center gap-2.5 rounded-md px-2 py-1.5 ${i === idx ? "bg-hover" : ""}`}
                >
                  <span className="flex w-4 justify-center text-muted">{c.icon}</span>
                  <span className="truncate">{c.label}</span>
                  {c.detail && <span className="truncate font-mono text-[11px] text-muted">{c.detail}</span>}
                  {c.hint && <span className="kbd ml-auto">{c.hint}</span>}
                </div>
              </div>
            );
          })}
        </div>
      </div>
    </div>,
    document.body,
  );
}
