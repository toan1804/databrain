import { useCallback, useEffect, useState } from "react";
import { AlertTriangle, CheckCircle2, Download, ExternalLink, FolderOpen, Loader2, RefreshCw } from "lucide-react";
import { open as openDialog } from "@tauri-apps/plugin-dialog";
import { api, isTauri, onOracleInstall, toError } from "../lib/api";
import type { OracleClientStatus, OracleInstallProgress } from "../lib/types";
import { formatBytes } from "../lib/util";
import { useStore } from "../store";
import { Modal } from "./ui";

/** Check + one-click install of Oracle Instant Client. */
export function OracleClientPanel({ libDir, onUseDir, compact }: { libDir?: string | null; onUseDir?: (dir: string) => void; compact?: boolean }) {
  const toast = useStore((s) => s.toast);
  const [st, setSt] = useState<OracleClientStatus | null>(null);
  const [busy, setBusy] = useState<null | "check" | "install">(null);
  const [prog, setProg] = useState<OracleInstallProgress | null>(null);

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
    setProg(null);
    const un = await onOracleInstall(setProg).catch(() => null);
    try {
      const r = await api.oracleInstallClient();
      setSt(r);
      if (r.installed) {
        toast(`Oracle Instant Client ${r.version ?? ""} installed`, "success");
        if (r.lib_dir) onUseDir?.(r.lib_dir);
      }
    } catch (e) {
      const err = toError(e);
      if (err.kind === "cancelled") toast("Download cancelled", "info");
      else toast(`Install failed: ${err.message}. Use the download page instead.`, "error");
    } finally {
      un?.();
      setProg(null);
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
      {busy === "install" && <InstallProgressBar p={prog} onCancel={() => void api.oracleCancelInstall()} />}
      <div className={`flex flex-wrap items-center gap-1.5 ${busy === "install" ? "hidden" : ""}`}>
        {st.platform.auto_install && (
          <button className="btn-primary py-1" onClick={() => void install()} disabled={!!busy}>
            {busy === "install" ? <Loader2 size={12} className="animate-spin" /> : <Download size={12} />}
            Install Instant Client
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
        {st.platform.auto_install
          ? "Downloads Oracle's latest Basic package (~110–140 MB) and installs it into DataBrain's app-data folder."
          : "Download the Basic package for your system and choose its folder."}
        {st.platform.note ? ` ${st.platform.note}` : ""} By installing you accept Oracle's license terms for Instant Client.
      </p>
    </div>
  );
}

export function installPercent(p: OracleInstallProgress | null): number | null {
  if (!p) return null;
  if (p.phase !== "downloading") return 100;
  return p.total ? Math.min(100, Math.round((p.received / p.total) * 100)) : null;
}

function InstallProgressBar({ p, onCancel }: { p: OracleInstallProgress | null; onCancel: () => void }) {
  const pct = installPercent(p);
  const label =
    !p
      ? "Starting download…"
      : p.phase === "downloading"
        ? `Downloading ${formatBytes(p.received)}${p.total ? ` of ${formatBytes(p.total)}` : ""}${pct !== null ? ` · ${pct}%` : ""}`
        : p.phase === "installing"
          ? "Installing…"
          : "Checking the library…";
  return (
    <div className="space-y-1">
      <div
        className="h-2 overflow-hidden rounded-full bg-panel-2"
        role="progressbar"
        aria-label="Instant Client download"
        aria-valuemin={0}
        aria-valuemax={100}
        aria-valuenow={pct ?? undefined}
      >
        <div
          className={`h-full rounded-full bg-accent transition-[width] duration-200 ${pct === null ? "w-1/3 animate-pulse" : ""}`}
          style={pct === null ? undefined : { width: `${pct}%` }}
        />
      </div>
      <div className="flex items-center gap-2 text-[11.5px] text-muted">
        <Loader2 size={11} className="animate-spin" />
        <span aria-live="polite">{label}</span>
        {(!p || p.phase === "downloading") && (
          <button className="ml-auto text-[11.5px] hover:text-fg hover:underline" onClick={onCancel}>
            Cancel
          </button>
        )}
      </div>
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
