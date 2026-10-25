import { useEffect, useState } from "react";
import { Loader2 } from "lucide-react";
import { api, toError } from "../lib/api";
import type { BackgroundStatus } from "../lib/types";
import { relativeTime } from "../lib/util";
import { useStore } from "../store";

/**
 * Settings → Jobs: scheduled jobs run while DataBrain is open (the default);
 * this turns on a runner the OS starts every minute so they also run when
 * it is closed.
 */
export function BackgroundJobsSettings() {
  const toast = useStore((s) => s.toast);
  const [st, setSt] = useState<BackgroundStatus | null>(null);
  const [busy, setBusy] = useState(false);
  const load = () => api.backgroundJobsStatus().then(setSt, (e) => toast(toError(e).message, "error"));
  useEffect(() => {
    void load();
    const t = setInterval(() => void load(), 30_000);
    return () => clearInterval(t);
  }, []); // eslint-disable-line react-hooks/exhaustive-deps

  const toggle = async (on: boolean) => {
    setBusy(true);
    try {
      setSt(await api.setBackgroundJobs(on));
      toast(on ? "Scheduled jobs now also run when DataBrain is closed" : "Scheduled jobs run only while DataBrain is open", "success");
    } catch (e) {
      toast(toError(e).message, "error");
      void load();
    } finally {
      setBusy(false);
    }
  };

  return (
    <div className="max-w-lg space-y-2 text-[12.5px]">
      <h3 className="text-[13px] font-medium">Scheduled jobs</h3>
      <p className="text-[12px] text-muted">Jobs with a schedule run while DataBrain is open. A run missed while it was closed runs once at the next start.</p>
      <label className="flex items-start gap-2">
        <input type="checkbox" className="mt-0.5" disabled={busy || !st} checked={!!st?.enabled} onChange={(e) => void toggle(e.target.checked)} />
        <span>
          Also run them when DataBrain is closed
          {busy && <Loader2 size={12} className="ml-1.5 inline animate-spin" />}
        </span>
      </label>
      <p className="text-[11.5px] text-muted">
        The computer starts DataBrain's job runner every minute ({st?.mechanism ?? "OS scheduler"}) while you are logged in: it runs the jobs that are due and exits. A job never runs twice at once, in the app and the runner. Failed runs show a desktop notification and are in the job's runs as usual.
      </p>
      {st?.enabled && (
        <div className="space-y-1 rounded-md border border-line bg-panel-2 p-2 text-[11.5px]">
          <div>
            {st.installed ? "Installed" : <span className="text-warning">Not installed: turn it off and on again</span>} ·{" "}
            <span className="font-mono">{st.location}</span>
          </div>
          <div>{st.last_tick ? `Runner last checked ${relativeTime(st.last_tick)}` : "The runner hasn't run yet (within a minute)"}</div>
          <div>
            Log: <span className="font-mono">{st.log_file}</span>
          </div>
        </div>
      )}
      <ul className="list-disc space-y-1 pl-4 text-[11.5px] text-muted">
        <li>Nothing runs while the computer sleeps; a run missed then runs at the next check.</li>
        <li>Connections that sign in through the browser (SSO) can't sign in again by themselves: once their sign-in expires, background runs fail until you sign in in the app.</li>
        <li>macOS may ask once whether “databrain-jobs” may use your keychain passwords. Settings → Passwords → local vault avoids the prompt.</li>
        <li>
          From a terminal: <span className="font-mono">databrain-jobs run &lt;job&gt;</span> runs a job now, <span className="font-mono">databrain-jobs list</span> shows them (next to the DataBrain app).
        </li>
      </ul>
    </div>
  );
}
