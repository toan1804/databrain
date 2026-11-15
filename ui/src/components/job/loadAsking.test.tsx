import { afterEach, describe, expect, it, vi } from "vitest";
import { api } from "../../lib/api";
import type { ConnectionView, LoadReport, LoadResultArgs } from "../../lib/types";
import { useStore } from "../../store";
import { loadResultAsking } from "./LoadFields";

const report: LoadReport = { rows: 2, deleted: 0, created_table: false, transaction: true, notices: [] };
const args: LoadResultArgs = { target_connection_id: "c1", table: "t", mode: "append", key_columns: [], create_table: false, confirmed: false };
const withEnv = (env: ConnectionView["env"]) => useStore.setState({ connections: [{ id: "c1", name: "Main", env } as ConnectionView] });

/** Answer the confirm the helper opens (once it is open). */
async function answer(yes: boolean) {
  await vi.waitFor(() => expect(useStore.getState().confirm).not.toBeNull());
  const c = useStore.getState().confirm!;
  useStore.getState().askConfirm(null);
  if (yes) c.onConfirm();
  else c.onCancel?.();
}

describe("loading into a production connection", () => {
  afterEach(() => {
    vi.restoreAllMocks();
    useStore.setState({ connections: [], confirm: null });
  });

  it("asks first, and loads confirmed once agreed", async () => {
    withEnv("prod");
    const load = vi.spyOn(api, "loadResult").mockResolvedValue(report);
    const started = vi.fn();
    const p = loadResultAsking("r", args, "l", started);
    expect(started).not.toHaveBeenCalled();
    await answer(true);
    expect(await p).toEqual(report);
    expect(started).toHaveBeenCalledOnce();
    expect(load).toHaveBeenCalledWith("r", { ...args, confirmed: true }, "l");
  });

  it("writes nothing when the user says no", async () => {
    withEnv("prod");
    const load = vi.spyOn(api, "loadResult").mockResolvedValue(report);
    const started = vi.fn();
    const p = loadResultAsking("r", args, "l", started);
    await answer(false);
    expect(await p).toBeNull();
    expect(load).not.toHaveBeenCalled();
    expect(started).not.toHaveBeenCalled();
  });

  it("doesn't ask elsewhere, but asks when the server says it's production", async () => {
    withEnv("dev");
    const load = vi
      .spyOn(api, "loadResult")
      .mockRejectedValueOnce({ kind: "invalid", message: "\"Main\" is a production connection", code: "needs_confirmation" })
      .mockResolvedValueOnce(report);
    const steps: string[] = [];
    const p = loadResultAsking(
      "r",
      args,
      "l",
      () => steps.push("start"),
      () => steps.push("pause"),
    );
    await vi.waitFor(() => expect(useStore.getState().confirm).not.toBeNull());
    // Progress and Stop are gone while the prompt is open.
    expect(steps).toEqual(["start", "pause"]);
    await answer(true);
    expect(await p).toEqual(report);
    expect(steps).toEqual(["start", "pause", "start"]);
    expect(load.mock.calls.map((c) => c[1].confirmed)).toEqual([false, true]);
  });

  it("when the server asks and the user says no, nothing shows as running", async () => {
    withEnv("dev");
    vi.spyOn(api, "loadResult").mockRejectedValueOnce({ kind: "invalid", message: "production", code: "needs_confirmation" });
    const steps: string[] = [];
    const p = loadResultAsking(
      "r",
      args,
      "l",
      () => steps.push("start"),
      () => steps.push("pause"),
    );
    await answer(false);
    expect(await p).toBeNull();
    expect(steps).toEqual(["start", "pause"]);
  });
});

describe("loading with a mode that removes rows", () => {
  afterEach(() => {
    vi.restoreAllMocks();
    useStore.setState({ connections: [], confirm: null });
  });

  it("asks on any connection, naming the table and what happens", async () => {
    withEnv("dev");
    const load = vi.spyOn(api, "loadResult").mockResolvedValue(report);
    for (const mode of ["truncate", "replace", "sync"] as const) {
      const p = loadResultAsking("r", { ...args, mode }, "l");
      await vi.waitFor(() => expect(useStore.getState().confirm).not.toBeNull());
      const text = useStore.getState().confirm!.reasons.join(" ");
      expect(text).toContain('t on "Main"');
      await answer(false);
      expect(await p).toBeNull();
    }
    expect(load).not.toHaveBeenCalled();
    // Agreed: loaded (not marked as a production confirmation: it isn't one).
    const p = loadResultAsking("r", { ...args, mode: "truncate" }, "l");
    await answer(true);
    expect(await p).toEqual(report);
    expect(load).toHaveBeenCalledWith("r", { ...args, mode: "truncate", confirmed: false }, "l");
  });

  it("does not ask for an append outside production", async () => {
    withEnv("dev");
    const load = vi.spyOn(api, "loadResult").mockResolvedValue(report);
    expect(await loadResultAsking("r", args, "l")).toEqual(report);
    expect(useStore.getState().confirm).toBeNull();
    expect(load).toHaveBeenCalledOnce();
  });
});
