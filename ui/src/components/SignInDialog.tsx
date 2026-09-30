import { writeText } from "@tauri-apps/plugin-clipboard-manager";
import { Copy, ExternalLink, Globe, Loader2 } from "lucide-react";
import { api } from "../lib/api";
import { useStore } from "../store";
import { Modal } from "./ui";

/** Shown while an interactive sign-in (browser/OAuth/device code) runs. */
export function SignInDialog() {
  const signIn = useStore((s) => s.signIn);
  const toast = useStore((s) => s.toast);
  if (!signIn) return null;
  const ev = signIn.event;
  const cancel = () => {
    void api.cancelSignIn();
    useStore.getState().setSignIn(null);
  };
  return (
    <Modal
      title={signIn.label}
      onClose={cancel}
      width={440}
      footer={
        <button className="btn-ghost" onClick={cancel}>
          Cancel
        </button>
      }
    >
      {ev?.type === "device_code" ? (
        <div className="space-y-3 text-center text-[13px]">
          <p>
            Enter this code at <span className="font-medium">{ev.prompt.verification_uri}</span>
          </p>
          <div className="flex items-center justify-center gap-2">
            <span className="rounded-lg border border-line bg-panel-2 px-4 py-2 font-mono text-[22px] tracking-[0.2em] select-text">
              {ev.prompt.user_code}
            </span>
            <button
              className="icon-btn"
              aria-label="Copy code"
              onClick={() => void writeText(ev.prompt.user_code).then(() => toast("Code copied", "success"))}
            >
              <Copy size={15} />
            </button>
          </div>
          <button
            className="inline-flex items-center gap-1 text-accent underline"
            onClick={() =>
              void writeText(ev.prompt.verification_uri_complete ?? ev.prompt.verification_uri).then(() => toast("Link copied", "success"))
            }
          >
            The page opened in your browser — copy link <ExternalLink size={12} />
          </button>
          <div className="flex items-center justify-center gap-2 text-muted">
            <Loader2 size={14} className="animate-spin" /> Waiting for you to finish in the browser…
          </div>
        </div>
      ) : (
        <div className="space-y-3 text-center text-[13px]">
          <Globe size={28} className="mx-auto text-accent" />
          <p>
            {ev?.type === "browser_opened"
              ? "Continue in the browser window that just opened."
              : ev?.type === "finished"
                ? "Finishing sign-in…"
                : "Starting sign-in…"}
          </p>
          {ev?.type === "browser_opened" && (
            <p className="text-[11.5px] text-muted">
              Browser didn't open?{" "}
              <button className="text-accent underline" onClick={() => void writeText(ev.url).then(() => toast("Link copied", "success"))}>
                Copy the sign-in link
              </button>
            </p>
          )}
          <div className="flex items-center justify-center gap-2 text-muted">
            <Loader2 size={14} className="animate-spin" /> Waiting…
          </div>
        </div>
      )}
    </Modal>
  );
}
