import { describe, expect, it } from "vitest";
import { sshHostKeyPrompt } from "./api";

describe("SSH host key prompt", () => {
  it("reads the server and fingerprint from the refusal", () => {
    // As the tunnel words it (crates/ssh-tunnel TunnelError::UnknownHostKey).
    const message =
      "The SSH server bastion.example.com:2222 is not trusted yet. Its host key fingerprint is SHA256:q1w2E3r4/T5y+U6=: compare it with the server's (ssh-keygen -lf /etc/ssh/ssh_host_ed25519_key.pub) before trusting it.";
    expect(sshHostKeyPrompt({ kind: "connection", message, code: "ssh_host_key_unknown" })).toEqual({ host: "bastion.example.com", port: 2222, fingerprint: "SHA256:q1w2E3r4/T5y+U6=" });
    expect(sshHostKeyPrompt({ kind: "connection", message, code: "other" })).toBeNull();
  });
});
