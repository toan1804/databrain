import { useEffect, useState } from "react";
import { open as openDialog } from "@tauri-apps/plugin-dialog";
import { writeText } from "@tauri-apps/plugin-clipboard-manager";
import { Copy, Database, ExternalLink, FolderOpen, Loader2, RefreshCw, X } from "lucide-react";
import { create } from "zustand";
import { api, isTauri, toError } from "../lib/api";
import type { DuckdbEngineStatus } from "../lib/types";
import { useStore } from "../store";

const RELEASES = "https://github.com/toan1804/databrain/releases";
const DUCKDB_INSTALL = "https://duckdb.org/docs/installation/?environment=c";

const OS = typeof navigator === "undefined" ? "mac" : /Windows/i.test(navigator.userAgent) ? "windows" : /Mac/i.test(navigator.userAgent) ? "mac" : "linux";

/** How to get the DuckDB library on this OS. */
export const INSTALL_STEPS: Record<string, { command?: string; steps: string[] }> = {
  mac: {
    command: "brew install duckdb",
    steps: ["With Homebrew: run the command below; DataBrain finds /opt/homebrew/lib/libduckdb.dylib.", "Or download libduckdb-osx-universal.zip from DuckDB's site, unzip it, and choose libduckdb.dylib."],
  },
  linux: {
    steps: ["Download libduckdb-linux-amd64.zip (or -arm64) from DuckDB's site.", "Unzip it and copy libduckdb.so to /usr/local/lib, or choose the file below."],
  },
  windows: {
    steps: ["Download libduckdb-windows-amd64.zip from DuckDB's site, unzip it, and choose duckdb.dll.", "The DuckDB CLI alone (duckdb.exe) is not enough: DataBrain needs the library, duckdb.dll."],
  },
};

interface EngineState {
  status: DuckdbEngineStatus | null;
  busy: boolean;
  refresh: () => Promise<void>;
  load: (path: string | null) => Promise<DuckdbEngineStatus | null>;
}

export const useDuckdbEngine = create<EngineState>((set) => ({
  status: null,
  busy: false,
  refresh: async () => {
    if (!isTauri()) return;
    set({ status: await api.duckdbEngineStatus().catch(() => null) });
  },
  load: async (path) => {
    set({ busy: true });
    try {
      const status = await api.duckdbEngineLoad(path);
      set({ status, busy: false });
      return status;
    } catch (e) {
      set({ busy: false });
      useStore.getState().toast(toError(e).message, "error");
      return null;
    }
  },
}));

/** Pick the library file (or its folder) and load it. */
async function chooseLibrary(fileName: string) {
  const picked = await openDialog({ multiple: false, directory: false, title: `Choose ${fileName}` }).catch(() => null);
  if (typeof picked !== "string") return;
  const st = await useDuckdbEngine.getState().load(picked);
  const toast = useStore.getState().toast;
  if (st?.available && st.path && !st.path.startsWith(picked.replace(/[\\/]+$/, ""))) toast("Saved: restart DataBrain to use that library", "success");
  else if (st?.available) toast(`DuckDB ${st.version ?? ""} is ready`, "success");
  else if (st) toast(st.error ?? "That file is not a DuckDB library", "error");
}

/** Install steps, the file to choose, and the alternative download. */
export function EngineGuide({ status }: { status: DuckdbEngineStatus }) {
  const load = useDuckdbEngine((s) => s.load);
  const busy = useDuckdbEngine((s) => s.busy);
  const toast = useStore((s) => s.toast);
  const g = INSTALL_STEPS[OS];
  return (
    <div className="space-y-2 text-[12px]">
      <p>
        This DataBrain doesn't include DuckDB's engine; it uses the DuckDB library installed on your computer ({status.file_name}), which
        powers Results, jobs that combine connections, outputs, local files and DuckDB connections.
      </p>
      <ol className="list-decimal space-y-1 pl-5">
        {g.steps.map((s) => (
          <li key={s}>{s}</li>
        ))}
      </ol>
      {g.command && (
        <div className="flex items-center gap-2">
          <code className="flex-1 rounded border border-line bg-panel-2 px-2 py-1 font-mono text-[11.5px] select-text">{g.command}</code>
          <button className="icon-btn" aria-label="Copy the command" onClick={() => void writeText(g.command!).then(() => toast("Command copied", "success"))}>
            <Copy size={13} />
          </button>
        </div>
      )}
      <div className="flex flex-wrap gap-2 pt-1">
        <button className="btn" disabled={busy} onClick={() => void load(null)}>
          {busy ? <Loader2 size={13} className="animate-spin" /> : <RefreshCw size={13} />} Look again
        </button>
        <button className="btn" disabled={busy} onClick={() => void chooseLibrary(status.file_name)}>
          <FolderOpen size={13} /> Choose {status.file_name}…
        </button>
        <button className="btn-ghost" onClick={() => void api.openLink(DUCKDB_INSTALL).catch(() => {})}>
          Get DuckDB <ExternalLink size={11} />
        </button>
        <button className="btn-ghost" onClick={() => void api.openLink(RELEASES).catch(() => {})}>
          Download DataBrain with DuckDB included <ExternalLink size={11} />
        </button>
      </div>
      {status.tried.length > 0 && (
        <details className="text-[11px] text-muted">
          <summary className="cursor-pointer">Where DataBrain looked</summary>
          <ul className="mt-1 space-y-0.5 font-mono">
            {status.tried.map((t) => (
              <li key={t} className="break-all">
                {t}
              </li>
            ))}
          </ul>
        </details>
      )}
    </div>
  );
}

/** Banner under the title bar when the system engine is missing. */
export function DuckdbEngineBanner() {
  const status = useDuckdbEngine((s) => s.status);
  const refresh = useDuckdbEngine((s) => s.refresh);
  const [open, setOpen] = useState(false);
  const [hidden, setHidden] = useState(false);
  useEffect(() => {
    void refresh();
  }, [refresh]);
  if (!status || status.bundled || status.available || hidden) return null;
  return (
    <div role="alert" className="shrink-0 border-b border-line bg-warning/10 px-3 py-1.5 text-[12px]">
      <div className="flex items-center gap-2">
        <Database size={14} className="shrink-0 text-warning" />
        <span className="min-w-0 flex-1">DuckDB engine not found: Results, outputs, jobs that combine connections and local files need it.</span>
        <button className="btn shrink-0 py-0.5" onClick={() => setOpen((o) => !o)} aria-expanded={open}>
          {open ? "Hide" : "How to fix"}
        </button>
        <button className="icon-btn shrink-0" aria-label="Dismiss until restart" title="Hide until restart" onClick={() => setHidden(true)}>
          <X size={13} />
        </button>
      </div>
      {open && (
        <div className="mt-2 max-w-3xl pb-1">
          <EngineGuide status={status} />
        </div>
      )}
    </div>
  );
}

/** Settings → DuckDB engine. */
export function DuckdbEngineSettings() {
  const status = useDuckdbEngine((s) => s.status);
  const refresh = useDuckdbEngine((s) => s.refresh);
  const busy = useDuckdbEngine((s) => s.busy);
  useEffect(() => {
    void refresh();
  }, [refresh]);
  if (!status) return <p className="text-[12px] text-muted">Loading…</p>;
  return (
    <div className="max-w-lg space-y-2 text-[12.5px]">
      <h3 className="text-[13px] font-medium">DuckDB engine</h3>
      {status.bundled ? (
        <p className="text-[12px] text-muted">DuckDB is built into this DataBrain, with its extensions (Excel, Delta, Iceberg, httpfs…). Nothing to set up.</p>
      ) : status.available ? (
        <>
          <p className="text-[12px]">
            Using DuckDB <span className="font-mono">{status.version}</span> installed on this computer:
          </p>
          <code className="block break-all rounded border border-line bg-panel-2 px-2 py-1 font-mono text-[11.5px]">{status.path}</code>
          <p className="text-[11.5px] text-muted">
            Extensions come from DuckDB's own folder (~/.duckdb/extensions) and are installed on first use. To use another library, choose it and
            restart DataBrain.
          </p>
          <button className="btn" disabled={busy} onClick={() => void chooseLibrary(status.file_name)}>
            <FolderOpen size={13} /> Choose another {status.file_name}…
          </button>
        </>
      ) : (
        <EngineGuide status={status} />
      )}
    </div>
  );
}
