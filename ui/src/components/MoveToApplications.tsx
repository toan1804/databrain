import { useEffect, useState } from "react";
import { FolderInput, X } from "lucide-react";
import { api, isTauri, toError } from "../lib/api";
import { useStore } from "../store";

const DISMISSED = "db.moveToApps.dismissed";

/** macOS asks for Downloads access when DataBrain runs from the Downloads
 *  folder (it reads its own app bundle there). Offer to move it. */
export function MoveToApplications() {
  const [path, setPath] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);
  useEffect(() => {
    if (!isTauri() || localStorage.getItem(DISMISSED) === "1") return;
    api.appInDownloads().then(setPath).catch(() => {});
  }, []);
  if (!path) return null;
  const move = async (replace: boolean) => {
    setBusy(true);
    try {
      await api.moveToApplications(replace);
    } catch (e) {
      const err = toError(e);
      setBusy(false);
      if (err.kind === "exists") {
        useStore.getState().askConfirm({
          title: "Replace DataBrain in Applications?",
          reasons: ["Applications already has a DataBrain.app. It is replaced by this version; your connections and settings are kept."],
          confirmLabel: "Replace",
          onConfirm: () => void move(true),
        });
      } else {
        useStore.getState().toast(err.message, "error");
      }
    }
  };
  return (
    <div role="status" className="flex shrink-0 items-center gap-2 border-b border-line bg-accent/10 px-3 py-1.5 text-[12px]">
      <FolderInput size={14} className="shrink-0 text-accent" />
      <span className="min-w-0 flex-1">
        DataBrain is running from the Downloads folder, so macOS asks for access to Downloads. Move it to Applications and it won't ask
        again.
      </span>
      <button className="btn shrink-0 py-0.5" disabled={busy} onClick={() => void move(false)}>
        {busy ? "Moving…" : "Move to Applications"}
      </button>
      <button
        className="icon-btn shrink-0"
        aria-label="Dismiss"
        title="Don't show again"
        onClick={() => {
          localStorage.setItem(DISMISSED, "1");
          setPath(null);
        }}
      >
        <X size={13} />
      </button>
    </div>
  );
}
