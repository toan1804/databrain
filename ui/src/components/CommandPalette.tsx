import { useEffect, useMemo, useRef, useState, type ReactNode } from "react";
import { createPortal } from "react-dom";
import { Bookmark, Database, FileCode2, FileSearch, History, Moon, NotebookPen, Plus, Search, Settings2, Sparkles, Sun, X } from "lucide-react";
import { useAi } from "../aiStore";
import { openOutput, openResultsQuery, outputLabel, outputRef } from "../outputs";
import { queryLocalFile } from "./Sidebar";
import { fuzzyMatch, sqlPreview } from "../lib/util";
import { useStore } from "../store";
import { ConnDot } from "./ui";

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

  const commands = useMemo<Command[]>(() => {
    const active = st.tabs.find((t) => t.id === st.activeTabId);
    const cmds: Command[] = [
      { id: "new-tab", group: "Actions", label: "New query tab", icon: <Plus size={14} />, hint: "⌘T", run: () => st.newTab() },
      {
        id: "find-table",
        group: "Actions",
        label: "Find table…",
        icon: <Search size={14} />,
        hint: "⌘P",
        // After the palette closes, so focus lands in the search box.
        run: () => setTimeout(() => st.openCatalogSearch(), 0),
      },
      { id: "new-conn", group: "Actions", label: "New connection", icon: <Database size={14} />, run: () => st.openConnectionDialog(null) },
      {
        id: "theme",
        group: "Actions",
        label: st.theme === "dark" ? "Switch to light theme" : "Switch to dark theme",
        icon: st.theme === "dark" ? <Sun size={14} /> : <Moon size={14} />,
        run: () => st.setTheme(st.theme === "dark" ? "light" : "dark"),
      },
      { id: "new-nb", group: "Actions", label: "New notebook", icon: <NotebookPen size={14} />, run: () => void st.newNotebook() },
      { id: "file", group: "Actions", label: "Query a local file (CSV, Parquet, JSON, Excel)…", icon: <FileSearch size={14} />, run: () => void queryLocalFile() },
      { id: "ai", group: "AI", label: "Open AI assistant", icon: <Sparkles size={14} />, hint: "⌘L", run: () => useAi.getState().setOpen(true, "chat") },
      { id: "ai-kn", group: "AI", label: "AI knowledge (index schema, notes)", icon: <Sparkles size={14} />, run: () => useAi.getState().setOpen(true, "knowledge") },
      { id: "settings", group: "Actions", label: "Settings: appearance, AI providers, MCP", icon: <Settings2 size={14} />, hint: "⌘,", run: () => st.setSettingsOpen(true) },
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
        hint: "⌘S",
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
  }, [st]);

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
