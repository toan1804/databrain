import { type ReactNode } from "react";
import { Bookmark, Database, NotebookPen, Sparkles, History, Layers, Workflow } from "lucide-react";
import { useAi } from "../aiStore";
import { OutputsPanel } from "./OutputsPanel";
import { useJobs } from "../jobsStore";
import { useStore, type SidebarPanel } from "../store";
import { kbd } from "../lib/platform";
import { ConnectionsPanel } from "./sidebar/ConnectionsPanel";
import { SavedPanel } from "./sidebar/SavedPanel";
import { NotebooksPanel, JobsPanel } from "./sidebar/NotebooksJobsPanels";
import { HistoryPanel } from "./sidebar/HistoryPanel";
export { queryLocalFile } from "./sidebar/queryLocalFile";

export function Sidebar() {
  const panel = useStore((s) => s.sidebarPanel);
  return (
    <div className="flex h-full min-w-0">
      <ActivityRail />
      <div className="flex min-w-0 flex-1 flex-col border-r border-line bg-panel">
        {panel === "connections" && <ConnectionsPanel />}
        {panel === "saved" && <SavedPanel />}
        {panel === "notebooks" && <NotebooksPanel />}
        {panel === "jobs" && <JobsPanel />}
        {panel === "outputs" && <OutputsPanel />}
        {panel === "history" && <HistoryPanel />}
      </div>
    </div>
  );
}

function ActivityRail() {
  const panel = useStore((s) => s.sidebarPanel);
  const setPanel = useStore((s) => s.setSidebarPanel);
  const aiOpen = useAi((s) => s.open);
  const setAiOpen = useAi((s) => s.setOpen);
  const scheduled = useJobs((s) => s.jobs.filter((j) => j.schedule.enabled).length);
  // Run by the app, or by the background runner (another process).
  const runningJobs = useJobs((s) => new Set([...Object.values(s.runs).filter((r) => r.status === "running").map((r) => r.job_id), ...Object.keys(s.elsewhere).filter((id) => s.elsewhere[id])]).size);
  const item = (p: SidebarPanel, icon: ReactNode, label: string, dot = false, dotClass = "bg-danger") => (
    <button
      key={p}
      title={label}
      aria-label={label}
      aria-pressed={panel === p}
      onClick={() => {
        setPanel(p);
        // Work hidden from the activity bar (×) shows there again.
        if (p === "jobs") useJobs.getState().showBar();
      }}
      className={`relative flex h-10 w-10 items-center justify-center rounded-lg transition-colors ${
        panel === p ? "bg-hover text-fg" : "text-muted hover:text-fg"
      }`}
    >
      {panel === p && <span className="absolute left-[-6px] h-5 w-[3px] rounded-r bg-accent" />}
      {icon}
      {dot && <span className={`absolute right-2 top-2 h-2 w-2 rounded-full ring-2 ring-bg ${dotClass}`} aria-hidden />}
    </button>
  );
  return (
    <div className="flex w-12 shrink-0 flex-col items-center gap-1 border-r border-line bg-bg pt-2">
      {item("connections", <Database size={18} />, "Connections")}
      {item("saved", <Bookmark size={18} />, "Saved queries")}
      {item("notebooks", <NotebookPen size={18} />, "Notebooks")}
      {item(
        "jobs",
        <Workflow size={18} />,
        ["Jobs", runningJobs ? `${runningJobs} running` : "", scheduled ? `${scheduled} scheduled` : ""].filter(Boolean).join(" · "),
        runningJobs > 0 || scheduled > 0,
        // Running: the accent, pulsing; else the scheduled jobs' dot.
        runningJobs > 0 ? "animate-pulse bg-accent" : "bg-danger",
      )}
      {item("outputs", <Layers size={18} />, "Outputs")}
      {item("history", <History size={18} />, "History")}
      <div className="flex-1" />
      <button
        title={kbd("AI assistant (⌘L)")}
        aria-label="AI assistant"
        aria-pressed={aiOpen}
        onClick={() => setAiOpen(!aiOpen)}
        className={`mb-2 flex h-10 w-10 items-center justify-center rounded-lg ${aiOpen ? "bg-accent/15 text-accent" : "text-muted hover:text-fg"}`}
      >
        <Sparkles size={18} />
      </button>
    </div>
  );
}
