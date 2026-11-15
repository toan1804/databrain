import { describe, expect, it } from "vitest";
import { endpointOf } from "./useConnectionForm";

const cfg = (p: Record<string, unknown> = {}) => ({ kind: "postgres", host: "db.example.com", port: 5432, auth: { method: "password", user: "app" }, options: { sslmode: "require" }, ...p }) as never;

describe("where a connection's saved secret may be sent", () => {
  it("changes with the server, the port, the user or the SSH host", () => {
    const base = endpointOf(cfg());
    for (const changed of [
      cfg({ host: "evil.example.com" }),
      cfg({ port: 6543 }),
      cfg({ auth: { method: "password", user: "admin" } }),
      cfg({ kind: "mysql" }),
      cfg({ options: { sslmode: "require", account: "other" } }),
      cfg({ ssh: { host: "jump", port: 22, user: "me", auth: { method: "password" } } }),
    ]) {
      expect(endpointOf(changed)).not.toBe(base);
    }
  });

  it("stays for settings that don't send the secret elsewhere", () => {
    expect(endpointOf(cfg({ options: { sslmode: "verify_full" } }))).toBe(endpointOf(cfg()));
    expect(endpointOf(cfg({ read_only: true, database: "other" }))).toBe(endpointOf(cfg()));
  });
});
