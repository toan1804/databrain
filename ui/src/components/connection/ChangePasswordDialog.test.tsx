import { fireEvent, render, screen, waitFor } from "@testing-library/react";
import { afterEach, describe, expect, it, vi } from "vitest";
import { api } from "../../lib/api";
import type { ConnectionView } from "../../lib/types";
import { canChangePassword, ChangePasswordDialog } from "./ChangePasswordDialog";

const conn = (kind: string, method = "password") =>
  ({
    id: "c1",
    name: "Sales",
    env: "none",
    connected: true,
    has_secret: true,
    created_at: 0,
    updated_at: 0,
    config: { kind, auth: { method, user: "app" }, ssl_mode: "prefer", read_only: false },
    ai_policy: { ai_enabled: true, share_metadata: true, allowed_providers: [] },
  }) as unknown as ConnectionView;

describe("change password", () => {
  afterEach(() => vi.restoreAllMocks());

  it("is offered where the database lets a user change its own password with a password sign-in", () => {
    expect(canChangePassword(conn("postgres"))).toBe(true);
    expect(canChangePassword(conn("oracle"))).toBe(true);
    expect(canChangePassword(conn("snowflake"))).toBe(false);
    expect(canChangePassword(conn("postgres", "cloud_cli"))).toBe(false);
    const ro = conn("postgres");
    ro.config.read_only = true;
    expect(canChangePassword(ro)).toBe(false);
  });

  it("needs the new password twice, sends both and closes; a refusal stays shown", async () => {
    const change = vi.spyOn(api, "changePassword").mockRejectedValueOnce({ kind: "query", message: "ORA-28003: password verification failed" }).mockResolvedValueOnce(null);
    const onClose = vi.fn();
    render(<ChangePasswordDialog conn={conn("oracle")} onClose={onClose} />);
    const [current, next, again] = [screen.getByLabelText("Current password"), screen.getByLabelText("New password"), screen.getByLabelText("New password again")];
    fireEvent.change(current, { target: { value: "old" } });
    fireEvent.change(next, { target: { value: "n1" } });
    fireEvent.change(again, { target: { value: "n2" } });
    expect(screen.getByText("The new passwords differ.")).toBeTruthy();
    const button = screen.getByRole("button", { name: /Change password/ }) as HTMLButtonElement;
    expect(button.disabled).toBe(true);
    fireEvent.change(again, { target: { value: "n1" } });
    fireEvent.click(button);
    expect(await screen.findByRole("alert")).toHaveProperty("textContent", "ORA-28003: password verification failed");
    expect(onClose).not.toHaveBeenCalled();
    fireEvent.click(button);
    await waitFor(() => expect(onClose).toHaveBeenCalled());
    expect(change).toHaveBeenLastCalledWith("c1", "old", "n1");
  });
});
