// @vitest-environment jsdom
// Which failing calls open the expired-password dialog: the ones the user
// makes happen, not completions or revalidation, which fail quietly.
import { afterEach, describe, expect, it, vi } from "vitest";

vi.mock("@tauri-apps/api/core", () => ({
  invoke: vi.fn(() => Promise.reject({ kind: "auth", code: "password_expired", message: "ORA-28001: the password has expired" })),
}));

import { api } from "./api";

describe("expired password announcements", () => {
  const seen: string[] = [];
  const on = (e: Event) => seen.push((e as CustomEvent<{ connectionId: string }>).detail.connectionId);
  afterEach(() => {
    window.removeEventListener("db:password-expired", on);
    seen.length = 0;
  });

  it("connecting and opening the explorer announce it; completions, revalidation and the change itself don't", async () => {
    window.addEventListener("db:password-expired", on);
    await api.connect("c1").catch(() => {});
    await api.listSchemas("c2").catch(() => {});
    await api.completeTables("c3", null, "em", 20).catch(() => {});
    await api.revalidateExplorer("c4", [], false).catch(() => {});
    await api.changePassword("c5", null, "n").catch(() => {});
    expect(seen).toEqual(["c1", "c2"]);
  });
});
