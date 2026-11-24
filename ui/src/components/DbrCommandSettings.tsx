import { useEffect, useState } from "react";
import { Check, Loader2, TerminalSquare } from "lucide-react";
import { api, isTauri, toError } from "../lib/api";
import type { DbrCommand } from "../lib/bindings/DbrCommand";
import { isWindows } from "../lib/platform";
import { useStore } from "../store";

/** Put `dbr` on the PATH (Settings → Terminal), as editors do for their command. */
export function DbrCommandSettings() {
  const [status, setStatus] = useState<DbrCommand | null>(null);
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);
  useEffect(() => {
    if (isTauri()) void api.dbrCommandStatus().then(setStatus, (e) => setError(toError(e).message));
  }, []);
  const install = async () => {
    setBusy(true);
    try {
      const s = await api.installDbrCommand();
      setStatus(s);
      useStore.getState().toast(s.installed ? `dbr installed${isWindows ? ": open a new terminal to use it" : ""}` : "dbr isn't on the PATH yet", s.installed ? "success" : "info");
    } catch (e) {
      useStore.getState().toast(toError(e).message, "error");
    } finally {
      setBusy(false);
    }
  };
  return (
    <div className="max-w-lg space-y-2 pb-4 text-[12.5px]">
      <h3 className="text-[13px] font-medium">Command line</h3>
      <p className="text-[11.5px] text-muted">
        <code>dbr</code> opens files and SQL here from a terminal (<code>dbr orders.csv</code>), or runs SQL there and prints the rows (<code>dbr -c sales -i "select …"</code>); <code>dbr --help</code> lists the rest.
      </p>
      {status && (
        <div className="flex items-center gap-2">
          {status.installed ? (
            <span className="flex items-center gap-1 text-[12px]">
              <Check size={13} className="text-success" /> Installed{status.location && <> at <code className="break-all">{status.location}</code></>}
            </span>
          ) : (
            <span className="text-[12px] text-muted">
              {status.can_install ? "Not on the PATH" : "Placed"}
              {status.location && (
                <>
                  {status.can_install ? ": installing puts it at " : " at "}
                  <code className="break-all">{status.location}</code>
                </>
              )}
            </span>
          )}
          {status.can_install && (
            <button className="btn-ghost ml-auto shrink-0 border border-line" onClick={() => void install()} disabled={busy}>
              {busy ? <Loader2 size={13} className="animate-spin" /> : <TerminalSquare size={13} />} Install the dbr command
            </button>
          )}
        </div>
      )}
      {error && <p className="text-[11.5px] text-danger">Couldn't tell where dbr stands: {error}</p>}
      {status?.note && <p className="text-[11.5px] text-muted">{status.note}</p>}
      {status?.other && (
        <p className="text-[11.5px] text-muted">
          A terminal finds another <code>dbr</code> first: <code className="break-all">{status.other}</code>
          {status.can_install ? " (installing replaces it only when it is a link)." : "."}
        </p>
      )}
    </div>
  );
}
