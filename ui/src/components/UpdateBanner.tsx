import { useEffect, useState } from "react";
import { Download, Loader2, X } from "lucide-react";
import { api, isTauri, onUpdateEvent, toError } from "../lib/api";
import type { UpdateInfo } from "../lib/types";
import { useStore } from "../store";

const LAST_CHECK = "db.update.lastCheck";
const DISMISSED = "db.update.dismissed";
const EVERY_MS = 24 * 3600_000;

/** `update_check` setting: false turns the startup check off. */
async function checkEnabled(): Promise<boolean> {
  const s = await api.getSettings().catch(() => ({}) as Record<string, unknown>);
  return s.update_check !== false;
}

/** Install the update in place (signed builds), else open the download. */
function useInstall(info: UpdateInfo | null | undefined) {
  const toast = useStore((s) => s.toast);
  const [progress, setProgress] = useState<{ done: number; total: number | null; finished: boolean } | null>(null);
  const install = async () => {
    if (!info) return;
    if (!info.installable) return void api.openLink(info.download ?? info.url);
    setProgress({ done: 0, total: null, finished: false });
    const off = await onUpdateEvent((e) => setProgress((p) => ({ done: e.downloaded ?? p?.done ?? 0, total: e.total ?? p?.total ?? null, finished: !!e.finished || !!p?.finished })));
    try {
      // Restarts the app when it worked.
      await api.installUpdate();
    } catch (e) {
      setProgress(null);
      toast(`${toError(e).message}. Opening the download instead.`, "error");
      void api.openLink(info.download ?? info.url);
    } finally {
      off();
    }
  };
  const label = !progress
    ? info?.installable
      ? "Install and restart"
      : "Download"
    : progress.finished
      ? "Restarting…"
      : progress.total
        ? `Downloading ${Math.round((progress.done / progress.total) * 100)}%`
        : "Downloading…";
  return { install, busy: !!progress, label };
}

/** A banner when a newer DataBrain is released (checked at most once a day). */
export function UpdateBanner() {
  const [info, setInfo] = useState<UpdateInfo | null>(null);
  const { install, busy, label } = useInstall(info);
  useEffect(() => {
    if (!isTauri()) return;
    const last = Number(localStorage.getItem(LAST_CHECK) ?? 0);
    if (Date.now() - last < EVERY_MS) {
      const cached = localStorage.getItem("db.update.info");
      if (cached) setInfo(JSON.parse(cached) as UpdateInfo);
      return;
    }
    void checkEnabled().then(async (on) => {
      if (!on) return;
      const i = await api.checkForUpdate().catch(() => null);
      localStorage.setItem(LAST_CHECK, String(Date.now()));
      if (i) localStorage.setItem("db.update.info", JSON.stringify(i));
      setInfo(i);
    });
  }, []);
  if (!info?.newer || localStorage.getItem(DISMISSED) === info.latest) return null;
  return (
    <div role="status" className="flex shrink-0 items-center gap-2 border-b border-line bg-accent/10 px-3 py-1.5 text-[12px]">
      <Download size={14} className="shrink-0 text-accent" />
      <span className="min-w-0 flex-1">
        DataBrain {info.latest} is available (you have {info.current}).
      </span>
      <button className="btn shrink-0 py-0.5" disabled={busy} onClick={() => void install()} title={info.installable ? "Download, check its signature, install and restart DataBrain" : (info.download_name ?? "Release page")}>
        {busy && <Loader2 size={12} className="animate-spin" />} {label}
      </button>
      {(info.download || info.installable) && (
        <button className="btn-ghost shrink-0 py-0.5" onClick={() => void api.openLink(info.url)}>
          What's new
        </button>
      )}
      <button
        className="icon-btn shrink-0"
        aria-label="Dismiss"
        title="Not for this version"
        onClick={() => {
          localStorage.setItem(DISMISSED, info.latest);
          setInfo({ ...info });
        }}
      >
        <X size={13} />
      </button>
    </div>
  );
}

/** Settings → Updates. */
export function UpdateSettings() {
  const toast = useStore((s) => s.toast);
  const [on, setOn] = useState(true);
  const [busy, setBusy] = useState(false);
  const [result, setResult] = useState<UpdateInfo | null | undefined>(undefined);
  const { install, busy: installing, label } = useInstall(result);
  useEffect(() => {
    void checkEnabled().then(setOn);
  }, []);
  const checkNow = async () => {
    setBusy(true);
    try {
      const i = await api.checkForUpdate();
      setResult(i);
      localStorage.setItem(LAST_CHECK, String(Date.now()));
      if (i) localStorage.setItem("db.update.info", JSON.stringify(i));
    } catch (e) {
      toast(toError(e).message, "error");
    } finally {
      setBusy(false);
    }
  };
  return (
    <div className="max-w-md space-y-2 text-[12.5px]">
      <h3 className="text-[13px] font-medium">Updates</h3>
      <label className="flex items-center gap-2">
        <input
          type="checkbox"
          checked={on}
          onChange={(e) => {
            setOn(e.target.checked);
            void api.setSetting("update_check", e.target.checked).catch(() => {});
          }}
        />
        Check for a new version once a day
      </label>
      <p className="text-[11.5px] text-muted">
        DataBrain asks GitHub for its latest release (only the app's version is sent) and shows a banner. Signed releases install from the banner:
        the update is downloaded, its signature is checked against the key built into DataBrain, and the app restarts. Otherwise the banner links to
        the download for this computer. Nothing is installed without you clicking.
      </p>
      <button className="btn-ghost border border-line" disabled={busy} onClick={() => void checkNow()}>
        {busy && <Loader2 size={13} className="animate-spin" />} Check now
      </button>
      {result !== undefined && (
        <p className="text-[12px]" aria-live="polite">
          {result === null ? (
            "No release found."
          ) : result.newer ? (
            <>
              DataBrain {result.latest} is available.{" "}
              <button className="text-accent hover:underline disabled:opacity-60" disabled={installing} onClick={() => void install()}>
                {label}
              </button>
            </>
          ) : (
            `You have the latest version (${result.current}).`
          )}
        </p>
      )}
      <h3 className="pt-3 text-[13px] font-medium">Diagnostic log</h3>
      <p className="text-[11.5px] text-muted">What DataBrain did (connections, runs, jobs, assistant tool calls, errors), one file a day for a week. It holds no query text, results or passwords.</p>
      <button className="btn-ghost border border-line" onClick={() => void api.revealLogs().catch((e) => toast(toError(e).message, "error"))}>
        Show log files
      </button>
    </div>
  );
}
