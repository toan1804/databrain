import { fireEvent, render, screen } from "@testing-library/react";
import { afterEach, describe, expect, it, vi } from "vitest";
import { api } from "../../lib/api";
import type { ConnectionView } from "../../lib/types";
import { useStore } from "../../store";
import { BulkSettingField, bulkLoadInfo } from "./LoadFields";

const conn = (kind: string, options: Record<string, string> = {}): ConnectionView =>
  ({ id: "c1", name: "Lake", env: "none", has_secret: true, connected: true, config: { kind, options, auth: { method: "none" } } }) as never;

describe("bulk load per target", () => {
  afterEach(() => {
    vi.restoreAllMocks();
    useStore.setState({ connections: [] });
  });

  it("says how each database bulk loads, not Postgres's text for all", () => {
    expect(bulkLoadInfo("postgres").text).toContain("COPY");
    expect(bulkLoadInfo("clickhouse").text).toContain("TabSeparated");
    expect(bulkLoadInfo("duckdb").text).toContain("appender");
    expect(bulkLoadInfo("mssql").text).toContain("bulk copy");
    expect(bulkLoadInfo("oracle").text).toContain("arrays");
    expect(bulkLoadInfo("mysql").text).toContain("no bulk load");
    expect(bulkLoadInfo("sqlite").text).toContain("no bulk load");
    for (const k of ["postgres", "clickhouse", "duckdb", "mssql", "oracle", "mysql"]) expect(bulkLoadInfo(k).setting, k).toBeUndefined();
    expect(bulkLoadInfo("databricks").setting?.option).toBe("staging_volume");
    expect(bulkLoadInfo("bigquery").setting?.option).toBe("load_jobs");
    expect(bulkLoadInfo("snowflake").setting?.option).toBe("array_binds");
  });

  it("shows the connection's staging volume and saves a new one into the connection", async () => {
    const target = conn("databricks", { staging_volume: "/Volumes/main/default/old", warehouse: "w1" });
    const save = vi.spyOn(api, "saveConnection").mockResolvedValue(target);
    vi.spyOn(api, "listConnections").mockResolvedValue([target]);
    render(<BulkSettingField target={target} setting={bulkLoadInfo("databricks").setting!} />);
    const input = screen.getByLabelText(/Staging volume/) as HTMLInputElement;
    expect(input.value).toBe("/Volumes/main/default/old");
    const button = screen.getByRole("button", { name: /Save/ });
    expect(button).toBeDisabled();
    fireEvent.change(input, { target: { value: "/Volumes/main/etl/stage" } });
    fireEvent.click(button);
    await vi.waitFor(() => expect(save).toHaveBeenCalledOnce());
    const [profile, secret] = save.mock.calls[0];
    // The other settings stay, the secret is kept (null), and `connected` isn't sent.
    expect(profile.config.options).toEqual({ staging_volume: "/Volumes/main/etl/stage", warehouse: "w1" });
    expect(secret).toBeNull();
    expect("connected" in profile).toBe(false);
  });

  it("an emptied volume or a toggle set to off removes the option", async () => {
    const save = vi.spyOn(api, "saveConnection").mockImplementation(async (p) => p);
    vi.spyOn(api, "listConnections").mockResolvedValue([]);
    const dbx = conn("databricks", { staging_volume: "/Volumes/a/b/c" });
    const { unmount } = render(<BulkSettingField target={dbx} setting={bulkLoadInfo("databricks").setting!} />);
    fireEvent.change(screen.getByLabelText(/Staging volume/), { target: { value: "  " } });
    fireEvent.click(screen.getByRole("button", { name: /Save/ }));
    await vi.waitFor(() => expect(save).toHaveBeenCalledTimes(1));
    expect(save.mock.calls[0][0].config.options).toEqual({});
    unmount();
    const bq = conn("bigquery", { load_jobs: "on" });
    render(<BulkSettingField target={bq} setting={bulkLoadInfo("bigquery").setting!} />);
    const select = screen.getByLabelText(/Load jobs/) as HTMLSelectElement;
    expect(select.value).toBe("on");
    fireEvent.change(select, { target: { value: "off" } });
    fireEvent.click(screen.getByRole("button", { name: /Save/ }));
    await vi.waitFor(() => expect(save).toHaveBeenCalledTimes(2));
    expect(save.mock.calls[1][0].config.options).toEqual({});
  });
});
