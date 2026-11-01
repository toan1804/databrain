// Connection form, SSH tunnel.
import { open as openDialog } from "@tauri-apps/plugin-dialog";
import { FolderOpen } from "lucide-react";
import { Check, Label, SecretField } from "./parts";
import { ConnectionFormState } from "./useConnectionForm";

export function SshSection({ f }: { f: ConnectionFormState }) {
  const { sshOn, setSshOn, ssh, setSsh, sshSecret, setSshSecret, initial } = f;
  return (
    <div className="space-y-3">
      <Check checked={sshOn} onChange={setSshOn} label="Connect through an SSH tunnel" help="The database host/port are resolved from the SSH server." />
      {sshOn && (
        <>
          <div className="grid grid-cols-[1fr_90px] gap-3">
            <div>
              <Label htmlFor="s-host">SSH host *</Label>
              <input id="s-host" className="field" value={ssh.host} placeholder="bastion.example.com" onChange={(e) => setSsh({ ...ssh, host: e.target.value })} />
            </div>
            <div>
              <Label htmlFor="s-port">Port</Label>
              <input
                id="s-port"
                className="field"
                inputMode="numeric"
                value={ssh.port}
                onChange={(e) => setSsh({ ...ssh, port: Number(e.target.value.replace(/\D/g, "")) || 22 })}
              />
            </div>
          </div>
          <div className="grid grid-cols-2 gap-3">
            <div>
              <Label htmlFor="s-user">SSH user *</Label>
              <input id="s-user" className="field" value={ssh.user} onChange={(e) => setSsh({ ...ssh, user: e.target.value })} />
            </div>
            <div>
              <Label htmlFor="s-auth">Authentication</Label>
              <select
                id="s-auth"
                className="field"
                value={ssh.auth.method}
                onChange={(e) =>
                  setSsh({
                    ...ssh,
                    auth: e.target.value === "key" ? { method: "key", path: "~/.ssh/id_ed25519" } : { method: e.target.value as "agent" | "password" },
                  })
                }
              >
                <option value="agent">SSH agent</option>
                <option value="key">Private key file</option>
                <option value="password">Password</option>
              </select>
            </div>
          </div>
          {ssh.auth.method === "key" && (
            <div>
              <Label htmlFor="s-key">Private key file</Label>
              <div className="flex gap-2">
                <input
                  id="s-key"
                  className="field font-mono"
                  value={ssh.auth.path}
                  onChange={(e) => setSsh({ ...ssh, auth: { method: "key", path: e.target.value } })}
                />
                <button
                  type="button"
                  className="btn-ghost shrink-0 border border-line"
                  onClick={async () => {
                    const p = await openDialog({ multiple: false, directory: false });
                    if (typeof p === "string") setSsh({ ...ssh, auth: { method: "key", path: p } });
                  }}
                >
                  <FolderOpen size={14} /> Browse
                </button>
              </div>
            </div>
          )}
          {ssh.auth.method !== "agent" && (
            <SecretField
              label={ssh.auth.method === "key" ? "Key passphrase (if any)" : "SSH password"}
              value={sshSecret}
              onChange={setSshSecret}
              placeholder={initial?.config.ssh ? "(unchanged if empty)" : ""}
            />
          )}
          <div className="rounded-lg bg-panel-2 px-3 py-2 text-[12px]">
            <span className="text-muted">Host key: </span>
            {ssh.host_key_fingerprint ? (
              <>
                <span className="font-mono">{ssh.host_key_fingerprint}</span>
                <button type="button" className="ml-2 text-accent underline" onClick={() => setSsh({ ...ssh, host_key_fingerprint: null })}>
                  Forget
                </button>
              </>
            ) : (
              <span className="text-muted">pinned on first connect (trust on first use)</span>
            )}
          </div>
        </>
      )}
    </div>
  );
}
