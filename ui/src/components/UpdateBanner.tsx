import { useEffect, useState } from "react";
import { Download, Loader2, X } from "lucide-react";
import { getVersion } from "@tauri-apps/api/app";
import { api, isTauri, onUpdateEvent, toError } from "../lib/api";
import type { UpdateInfo } from "../lib/types";
import { readStored } from "../lib/storage";

/** A cached check result as this version shows it. */
function isUpdateInfo(v: unknown): v is UpdateInfo {
  const o = v as Partial<UpdateInfo> | null;
  return !!o && typeof o === "object" && typeof o.current === "string" && typeof o.latest === "string" && typeof o.newer === "boolean" && typeof o.url === "string";
}
import { useStore } from "../store";

const LAST_CHECK = "db.update.lastCheck";
const DISMISSED = "db.update.dismissed";
const EVERY_MS = 24 * 3600_000;

/** `update_check` setting: false turns the startup check off. */
async function checkEnabled(): Promise<boolean> {
  const s = await api.getSettings().catch(() => ({}) as Record<string, unknown>);
  return s.update_check !== false;
}

/** Ask again and keep the answer for the banner (at most once a day otherwise). */
async function freshCheck(): Promise<UpdateInfo | null> {
  const i = await api.checkForUpdate();
  localStorage.setItem(LAST_CHECK, String(Date.now()));
  if (i) localStorage.setItem("db.update.info", JSON.stringify(i));
  return i;
}

/**
 * Install the update in place (signed builds), else open the download. A
 * click asks again first: the banner may show a result kept from an
 * earlier check, made by the version installed before this one (which
 * couldn't install updates) or before the release had its signed update.
 */
function useInstall(info: UpdateInfo | null | undefined, onFresh?: (i: UpdateInfo | null) => void) {
  const toast = useStore((s) => s.toast);
  const [progress, setProgress] = useState<{ done: number; total: number | null; finished: boolean } | null>(null);
  const install = async () => {
    if (!info) return;
    let now = info;
    if (!info.installable) {
      setProgress({ done: 0, total: null, finished: false });
      const fresh = await freshCheck().catch(() => null);
      setProgress(null);
      if (fresh) {
        onFresh?.(fresh);
        now = fresh;
      }
    }
    if (!now.installable) {
      if (now.install_note) toast(`Can't install it from the app (${now.install_note}): opening the download.`, "info");
      return void api.openLink(now.download ?? now.url);
    }
    setProgress({ done: 0, total: null, finished: false });
    const off = await onUpdateEvent((e) => setProgress((p) => ({ done: e.downloaded ?? p?.done ?? 0, total: e.total ?? p?.total ?? null, finished: !!e.finished || !!p?.finished })));
    try {
      // Restarts the app when it worked.
      await api.installUpdate();
    } catch (e) {
      setProgress(null);
      toast(`${toError(e).message}. Opening the download instead.`, "error");
      void api.openLink(now.download ?? now.url);
    } finally {
      off();
    }
  };
  const label = !progress
    ? info?.installable
      ? "Update and relaunch"
      : "Download"
    : progress.finished
      ? "Relaunching…"
      : progress.total
        ? `Downloading ${Math.round((progress.done / progress.total) * 100)}%`
        : "Downloading…";
  return { install, busy: !!progress, label };
}

/** A banner when a newer DataBrain is released (checked at most once a day). */
export function UpdateBanner() {
  const [info, setInfo] = useState<UpdateInfo | null>(null);
  const { install, busy, label } = useInstall(info, setInfo);
  useEffect(() => {
    if (!isTauri()) return;
    const last = Number(localStorage.getItem(LAST_CHECK) ?? 0);
    const cached = readStored<UpdateInfo | null>("db.update.info", null, isUpdateInfo);
    if (Date.now() - last < EVERY_MS) {
      // A result kept by the version installed before this one is not this
      // version's answer (it couldn't install updates, say): asked again.
      void getVersion()
        .catch(() => cached?.current)
        .then((running) => {
          if (!cached) return;
          if (cached.current === running) return setInfo(cached);
          void checkEnabled().then(async (on) => on && setInfo(await freshCheck().catch(() => null)));
        });
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
      <button className="btn shrink-0 py-0.5" disabled={busy} onClick={() => void install()} title={info.installable ? "Download the update, check its signature, install it and relaunch DataBrain" : (info.download_name ?? "Release page")}>
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
  const { install, busy: installing, label } = useInstall(result, setResult);
  useEffect(() => {
    void checkEnabled().then(setOn);
  }, []);
  const checkNow = async () => {
    setBusy(true);
    try {
      setResult(await freshCheck());
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
        the update is downloaded, its signature is checked against the key built into DataBrain, and the app relaunches. Otherwise the banner links to
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
