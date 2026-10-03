import { useEffect, useState } from "react";
import { Command, Moon, Settings2, Sparkles, Sun, TriangleAlert } from "lucide-react";
import { useStore } from "./store";
import { editorBridge } from "./editorBridge";
import { Sidebar } from "./components/Sidebar";
import { EditorPane } from "./components/EditorPane";
import { ConnectionDialog } from "./components/ConnectionDialog";
import { SaveQueryDialog } from "./components/SaveQueryDialog";
import { CommandPalette } from "./components/CommandPalette";
import { ConfirmDialog, Toasts } from "./components/ui";
import { AiPanel } from "./components/AiPanel";
import { ExcelSheetDialog } from "./components/ExcelSheetDialog";
import { SplitHandle } from "./components/SplitHandle";
import { SettingsDialog } from "./components/SettingsDialog";
import { SignInDialog } from "./components/SignInDialog";
import { IndexScopeDialog } from "./components/IndexScopeDialog";
import { OracleClientDialog } from "./components/OracleClient";
import { InlineAi } from "./components/InlineAi";
import { useAi } from "./aiStore";

function useGlobalShortcuts() {
  useEffect(() => {
    const onKey = (e: KeyboardEvent) => {
      const mod = e.metaKey || e.ctrlKey;
      if (!mod) return;
      const st = useStore.getState();
      const key = e.key.toLowerCase();
      const inEditor = (e.target as HTMLElement | null)?.closest?.(".cm-editor");
      if (key === "l" && !e.shiftKey) {
        e.preventDefault();
        const ai = useAi.getState();
        ai.setOpen(!ai.open, "chat");
      } else if (key === "," ) {
        e.preventDefault();
        st.setSettingsOpen(true);
      } else if (key === "k") {
        e.preventDefault();
        st.setPaletteOpen(!st.paletteOpen);
      } else if (key === "p" && !e.shiftKey) {
        // Find a table in the explorer.
        e.preventDefault();
        st.openCatalogSearch();
      } else if (key === "t") {
        e.preventDefault();
        st.newTab();
      } else if (key === "w") {
        e.preventDefault();
        if (st.activeTabId) st.closeTab(st.activeTabId);
      } else if (key === ".") {
        e.preventDefault();
        if (st.activeTabId) void st.cancelTab(st.activeTabId);
      } else if (key === "enter" && !inEditor && st.activeTabId) {
        // Run from anywhere (e.g. while the grid has focus).
        const v = editorBridge.get(st.activeTabId);
        if (!v) return;
        e.preventDefault();
        const sel = v.state.selection.main;
        void st.runTab(st.activeTabId, e.shiftKey ? "all" : "statement", {
          doc: v.state.doc.toString(),
          selFrom: sel.from,
          selTo: sel.to,
          cursor: sel.head,
        });
      } else if (/^[1-9]$/.test(key) && !e.shiftKey && !e.altKey) {
        const t = st.tabs[Number(key) - 1];
        if (t) {
          e.preventDefault();
          st.setActiveTab(t.id);
        }
      }
    };
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  }, []);
}

export default function App() {
  const ready = useStore((s) => s.ready);
  const backend = useStore((s) => s.backendAvailable);
  const theme = useStore((s) => s.theme);
  const setTheme = useStore((s) => s.setTheme);
  const setPalette = useStore((s) => s.setPaletteOpen);
  const [sidebarW, setSidebarW] = useState(() => Number(localStorage.getItem("db.sidebar")) || 300);
  const [aiW, setAiW] = useState(() => Number(localStorage.getItem("db.aiw")) || 400);
  const aiOpen = useAi((s) => s.open);
  const setAiOpen = useAi((s) => s.setOpen);

  useEffect(() => {
    void useStore
      .getState()
      .init()
      .then(() => useAi.getState().init())
      .catch(() => {});
  }, []);

  useEffect(() => {
    document.documentElement.classList.toggle("dark", theme === "dark");
    document.documentElement.style.colorScheme = theme;
  }, [theme]);
  useGlobalShortcuts();

  const save = (key: string) => (w: number) => localStorage.setItem(key, String(w));
  // Keep the editor at least 360px wide.
  const maxSidebar = () => Math.min(640, window.innerWidth - (aiOpen ? aiW : 0) - 360);
  const maxAi = () => Math.min(1100, window.innerWidth - sidebarW - 360);

  if (!ready) {
    return <div className="flex h-full items-center justify-center text-muted">Loading…</div>;
  }

  return (
    <div className="flex h-full flex-col">
      {/* Title bar area (macOS overlay title bar leaves room for traffic lights). */}
      <div className="flex h-9 shrink-0 items-center border-b border-line bg-bg pl-20 pr-2" data-tauri-drag-region>
        <div className="flex items-center gap-2 text-[12.5px] font-semibold tracking-tight" data-tauri-drag-region>
          <span className="flex h-5 w-5 items-center justify-center rounded-md bg-accent text-[11px] font-bold text-accent-fg">D</span>
          DataBrain
        </div>
        <button
          className="mx-auto flex h-6 w-[360px] items-center gap-2 rounded-md border border-line bg-panel px-2 text-[12px] text-muted hover:text-fg"
          onClick={() => setPalette(true)}
        >
          <Command size={12} /> Search commands, connections, saved queries…
          <span className="kbd ml-auto">⌘K</span>
        </button>
        <button
          className={`icon-btn ${aiOpen ? "text-accent" : ""}`}
          aria-label="AI assistant"
          aria-pressed={aiOpen}
          title="AI assistant (⌘L)"
          onClick={() => setAiOpen(!aiOpen)}
        >
          <Sparkles size={14} />
        </button>
        <button className="icon-btn" aria-label="Settings" title="Settings (⌘,)" onClick={() => useStore.getState().setSettingsOpen(true)}>
          <Settings2 size={14} />
        </button>
        <button
          className="icon-btn"
          aria-label={theme === "dark" ? "Switch to light theme" : "Switch to dark theme"}
          title="Toggle theme"
          onClick={() => setTheme(theme === "dark" ? "light" : "dark")}
        >
          {theme === "dark" ? <Sun size={14} /> : <Moon size={14} />}
        </button>
      </div>
      {!backend && (
        <div className="flex items-center gap-2 border-b border-warning/40 bg-warning/10 px-3 py-1.5 text-[12px] text-warning" role="status">
          <TriangleAlert size={13} /> Backend not available — run inside the desktop app (<span className="font-mono">npm run tauri dev</span>).
        </div>
      )}
      <div className="flex min-h-0 flex-1">
        <div style={{ width: sidebarW }} className="shrink-0">
          <Sidebar />
        </div>
        <SplitHandle
          side="left"
          label="Resize sidebar"
          width={sidebarW}
          min={200}
          max={maxSidebar}
          onChange={setSidebarW}
          onCommit={save("db.sidebar")}
          onReset={() => (setSidebarW(300), save("db.sidebar")(300))}
        />
        <div className="min-w-0 flex-1">
          <EditorPane />
        </div>
        {aiOpen && (
          <>
            <SplitHandle
              side="right"
              label="Resize assistant"
              width={aiW}
              min={300}
              max={maxAi}
              onChange={setAiW}
              onCommit={save("db.aiw")}
              onReset={() => (setAiW(400), save("db.aiw")(400))}
            />
            <div style={{ width: aiW }} className="shrink-0">
              <AiPanel />
            </div>
          </>
        )}
      </div>
      <ConnectionDialog />
      <SaveQueryDialog />
      <CommandPalette />
      <SettingsDialog />
      <SignInDialog />
      <IndexScopeDialog />
      <OracleClientDialog />
      <InlineAi />
      <ConfirmDialog />
      <ExcelSheetDialog />
      <Toasts />
    </div>
  );
}
