import { useState } from "react";
import { ShieldQuestion } from "lucide-react";
import { api, toError } from "../lib/api";
import { useStore } from "../store";
import { Modal } from "./ui";

/**
 * An SSH server DataBrain hasn't trusted yet: its key fingerprint, how to
 * check it, and Trust. Nothing (no password, no key) was sent to it.
 */
export function SshHostKeyDialog() {
  const p = useStore((s) => s.sshHostKey);
  const toast = useStore((s) => s.toast);
  const [busy, setBusy] = useState(false);
  if (!p) return null;
  const close = () => useStore.setState({ sshHostKey: null });
  const trust = async () => {
    setBusy(true);
    try {
      const n = await api.trustSshHostKey(p.host, p.port, p.fingerprint);
      // An open connection form (an unsaved connection being tested) takes it too.
      window.dispatchEvent(new CustomEvent("db:ssh-host-key-trusted", { detail: p }));
      toast(n ? `Trusted ${p.host}:${p.port} on ${n} connection${n === 1 ? "" : "s"}: connect again` : `Trusted ${p.host}:${p.port}: test or connect again`, "success");
      close();
    } catch (e) {
      toast(toError(e).message, "error");
    } finally {
      setBusy(false);
    }
  };
  return (
    <Modal
      title="Trust this SSH server?"
      onClose={close}
      width={520}
      footer={
        <>
          <button className="btn-ghost" onClick={close}>
            Cancel
          </button>
          <button className="btn-primary" onClick={() => void trust()} disabled={busy}>
            Trust this key
          </button>
        </>
      }
    >
      <div className="space-y-2 text-[12.5px]">
        <div className="flex items-start gap-2">
          <ShieldQuestion size={16} className="mt-0.5 shrink-0 text-warning" />
          <p>
            DataBrain hasn't connected to <span className="font-mono">{p.host}:{p.port}</span> before. Nothing was sent to it yet. Check that this is its key before trusting it:
          </p>
        </div>
        <div className="select-all rounded-md border border-line bg-panel-2 px-2 py-1.5 font-mono text-[12px]">{p.fingerprint}</div>
        <p className="text-muted">
          On the server, <span className="font-mono">ssh-keygen -lf /etc/ssh/ssh_host_ed25519_key.pub</span> prints it (or ask its administrator). If it differs, someone may be between you and the server: don't trust it.
        </p>
      </div>
    </Modal>
  );
}
