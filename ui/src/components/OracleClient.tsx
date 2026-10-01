import { useCallback, useEffect, useState } from "react";
import { AlertTriangle, CheckCircle2, Download, ExternalLink, FolderOpen, Loader2, RefreshCw } from "lucide-react";
import { open as openDialog } from "@tauri-apps/plugin-dialog";
import { api, isTauri, toError } from "../lib/api";
import type { OracleClientStatus } from "../lib/types";
import { useStore } from "../store";
import { Modal } from "./ui";

/** Check + one-click install of Oracle Instant Client. */
export function OracleClientPanel({ libDir, onUseDir, compact }: { libDir?: string | null; onUseDir?: (dir: string) => void; compact?: boolean }) {
  const toast = useStore((s) => s.toast);
  const [st, setSt] = useState<OracleClientStatus | null>(null);
  const [busy, setBusy] = useState<null | "check" | "install">(null);

  const check = useCallback(async () => {
    if (!isTauri()) return;
    setBusy((b) => b ?? "check");
    try {
      setSt(await api.oracleClientStatus(libDir));
    } catch (e) {
      setSt(null);
      toast(toError(e).message, "error");
    } finally {
      setBusy((b) => (b === "check" ? null : b));
    }
  }, [libDir, toast]);

  useEffect(() => {
    const t = setTimeout(() => void check(), 300);
    return () => clearTimeout(t);
  }, [check]);

  const install = async () => {
    setBusy("install");
    try {
      const r = await api.oracleInstallClient();
      setSt(r);
      if (r.installed) {
        toast(`Oracle Instant Client ${r.version ?? ""} installed`, "success");
        if (r.lib_dir) onUseDir?.(r.lib_dir);
      }
    } catch (e) {
      toast(`Install failed: ${toError(e).message}. Use the download page instead.`, "error");
    } finally {
      setBusy(null);
    }
  };

  const choose = async () => {
    const d = await openDialog({ directory: true, multiple: false, title: "Instant Client folder (contains the Oracle client library)" });
    if (typeof d === "string") onUseDir?.(d);
  };

  if (!st && busy)
    return (
      <div className="flex items-center gap-1.5 rounded-lg border border-line bg-panel-2 p-2.5 text-[12px] text-muted">
        <Loader2 size={12} className="animate-spin" /> Checking Oracle Instant Client…
      </div>
    );
  if (!st) return null;
  if (st.installed)
    return (
      <div className="flex items-center gap-1.5 rounded-lg border border-line bg-panel-2 px-2.5 py-2 text-[12px]">
        <CheckCircle2 size={13} className="shrink-0 text-success" />
        <span className="min-w-0 flex-1 truncate">
          Oracle Instant Client {st.version} {st.lib_dir && <span className="font-mono text-[11px] text-muted">· {st.lib_dir}</span>}
        </span>
      </div>
    );
  return (
    <div className="space-y-2 rounded-lg border border-warning/40 bg-warning/5 p-2.5 text-[12.5px]">
      <div className="flex items-start gap-2">
        <AlertTriangle size={14} className="mt-0.5 shrink-0 text-warning" />
        <div className="min-w-0 flex-1">
          <div className="font-medium">Oracle Instant Client is required</div>
          {!compact && (
            <div className="text-[12px] text-muted">
              Oracle connections use Oracle's client library. It was not found on this computer{st.lib_dir ? ` (checked ${st.lib_dir})` : ""}.
            </div>
          )}
        </div>
        <button className="icon-btn h-6 w-6" title="Check again" aria-label="Check again" onClick={() => void check()} disabled={!!busy}>
          {busy === "check" ? <Loader2 size={12} className="animate-spin" /> : <RefreshCw size={12} />}
        </button>
      </div>
      <div className="flex flex-wrap items-center gap-1.5">
        {st.platform.auto_install && (
          <button className="btn-primary py-1" onClick={() => void install()} disabled={!!busy}>
            {busy === "install" ? <Loader2 size={12} className="animate-spin" /> : <Download size={12} />}
            {busy === "install" ? "Downloading and installing (~70–140 MB)…" : "Install Instant Client"}
          </button>
        )}
        <button className="btn-ghost border border-line py-1" onClick={() => void api.oracleOpenDownload().catch((e) => toast(toError(e).message, "error"))}>
          <ExternalLink size={12} /> Download page
        </button>
        {onUseDir && (
          <button className="btn-ghost border border-line py-1" onClick={() => void choose()} disabled={!!busy}>
            <FolderOpen size={12} /> I have it: choose folder…
          </button>
        )}
      </div>
      <p className="text-[11px] text-muted">
        {st.platform.os === "macos"
          ? "Installs Oracle's notarized package into ~/Downloads/instantclient_* (Oracle's default)."
          : st.platform.auto_install
            ? "Installs into DataBrain's app-data folder."
            : "Download the Basic package for your system and choose its folder."}
        {st.platform.note ? ` ${st.platform.note}` : ""} By installing you accept Oracle's license terms for Instant Client.
      </p>
    </div>
  );
}

/** App-wide prompt (opened at startup or after a DPI-1047 connect error). */
export function OracleClientDialog() {
  const open = useStore((s) => s.oracleClientPrompt);
  const close = () => useStore.setState({ oracleClientPrompt: false });
  if (!open) return null;
  return (
    <Modal title="Oracle Instant Client" onClose={close} width={520} footer={<button className="btn-ghost" onClick={close}>Close</button>}>
      <OracleClientPanel />
    </Modal>
  );
}

/** At startup: if Oracle connections exist, check once and prompt when missing. */
export async function checkOracleClientAtStartup() {
  const st = useStore.getState();
  if (!isTauri() || !st.connections.some((c) => c.config.kind === "oracle")) return;
  try {
    const dir = st.connections.map((c) => (c.config.kind === "oracle" ? c.config.options?.client_lib_dir : undefined)).find(Boolean) ?? null;
    const r = await api.oracleClientStatus(dir);
    if (!r.installed) useStore.setState({ oracleClientPrompt: true });
  } catch {
    /* Oracle not enabled in this build */
  }
}
