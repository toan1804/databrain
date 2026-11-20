// Change a connection's password on the server (ALTER USER / ALTER LOGIN,
// as each database does it), then the one saved for it.
import { useState } from "react";
import { KeyRound, Loader2 } from "lucide-react";
import { api, toError } from "../../lib/api";
import { traitsOf } from "../../lib/sourceTraits";
import type { ConnectionView } from "../../lib/types";
import { useStore } from "../../store";
import { Modal } from "../ui";

/** Whether a connection can change its password here. */
/** Not on a read-only connection: it changes the server. */
export const canChangePassword = (conn: ConnectionView) => traitsOf(conn.config.kind).change_password && conn.config.auth.method === "password" && !conn.config.read_only;

export function ChangePasswordDialog({ conn, onClose }: { conn: ConnectionView; onClose: () => void }) {
  const [current, setCurrent] = useState("");
  const [next, setNext] = useState("");
  const [again, setAgain] = useState("");
  const [busy, setBusy] = useState(false);
  const [problem, setProblem] = useState<string | null>(null);
  const user = conn.config.auth.method === "password" ? conn.config.auth.user : "";
  const mismatch = again !== "" && again !== next;
  const ready = current !== "" && next !== "" && next === again && !busy;
  const submit = async () => {
    if (!ready) return;
    setBusy(true);
    setProblem(null);
    try {
      await api.changePassword(conn.id, current, next);
      useStore.getState().toast(conn.has_secret ? `Password of ${user} changed on the server and saved for "${conn.name}"` : `Password of ${user} changed on the server`, "success");
      onClose();
    } catch (e) {
      setProblem(toError(e).message);
    } finally {
      setBusy(false);
    }
  };
  return (
    <Modal
      title={`Change the password of ${user}`}
      onClose={() => !busy && onClose()}
      width={420}
      footer={
        <>
          <button className="btn-ghost" onClick={onClose} disabled={busy}>
            Cancel
          </button>
          <button className="btn-primary" disabled={!ready} onClick={() => void submit()}>
            {busy ? <Loader2 size={13} className="animate-spin" /> : <KeyRound size={13} />} Change password
          </button>
        </>
      }
    >
      <form
        className="space-y-3 text-[12.5px]"
        onSubmit={(e) => {
          e.preventDefault();
          void submit();
        }}
      >
        <p className="text-muted">
          Changes it on {traitsOf(conn.config.kind).name} for every program that signs in as {user}
          {conn.has_secret ? `, and saves the new one for "${conn.name}"` : ""}. The current password is checked by signing in with it first. Running queries and open transactions go on; other connections with this user keep the old password until you change them.
        </p>
        <label className="block space-y-1">
          <span className="text-muted">Current password</span>
          <input type="password" className="field" autoFocus autoComplete="current-password" value={current} onChange={(e) => setCurrent(e.target.value)} />
        </label>
        <label className="block space-y-1">
          <span className="text-muted">New password</span>
          <input type="password" className="field" autoComplete="new-password" value={next} onChange={(e) => setNext(e.target.value)} />
        </label>
        <label className="block space-y-1">
          <span className="text-muted">New password again</span>
          <input type="password" className="field" autoComplete="new-password" value={again} onChange={(e) => setAgain(e.target.value)} aria-invalid={mismatch} aria-describedby={mismatch ? "pw-mismatch" : undefined} />
        </label>
        {mismatch && (
          <p id="pw-mismatch" className="text-danger">
            The new passwords differ.
          </p>
        )}
        {problem && (
          <p className="text-danger" role="alert">
            {problem}
          </p>
        )}
        <button type="submit" hidden />
      </form>
    </Modal>
  );
}
