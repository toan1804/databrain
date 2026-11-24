import { fireEvent, render, screen, waitFor } from "@testing-library/react";
import { afterEach, describe, expect, it, vi } from "vitest";
import { announcePasswordExpired, api } from "../../lib/api";
import type { ConnectionView } from "../../lib/types";
import { useStore } from "../../store";
import { canChangePassword, ChangePasswordDialog, ExpiredPasswordDialog } from "./ChangePasswordDialog";

const conn = (kind: string, method = "password", saved = false) =>
  ({
    id: "c1",
    name: "Sales",
    env: "none",
    connected: true,
    has_secret: saved,
    created_at: 0,
    updated_at: 0,
    config: { kind, auth: { method, user: "app" }, ssl_mode: "prefer", read_only: false },
    ai_policy: { ai_enabled: true, share_metadata: true, allowed_providers: [] },
  }) as unknown as ConnectionView;

describe("change password", () => {
  const realToast = useStore.getState().toast;
  afterEach(() => {
    vi.restoreAllMocks();
    useStore.setState({ expiredPassword: null, expiredDismissed: [], connections: [], toast: realToast });
  });

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
    const change = vi.spyOn(api, "changePassword").mockRejectedValueOnce({ kind: "query", message: "ORA-28003: password verification failed" }).mockResolvedValueOnce({ others: [] });
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

  it("offers the new password to the other connections with this login, and says which it couldn't update", async () => {
    vi.spyOn(api, "changePassword").mockResolvedValueOnce({ others: [{ id: "c2", name: "Sales copy" }, { id: "c3", name: "Reports" }] });
    const update = vi
      .spyOn(api, "updateSavedPasswords")
      .mockResolvedValueOnce([{ id: "c3", name: "Reports", error: "Couldn't sign in to check the new password: timeout" }]);
    const onClose = vi.fn();
    render(<ChangePasswordDialog conn={conn("postgres")} onClose={onClose} />);
    fireEvent.change(screen.getByLabelText("Current password"), { target: { value: "old" } });
    fireEvent.change(screen.getByLabelText("New password"), { target: { value: "n1" } });
    fireEvent.change(screen.getByLabelText("New password again"), { target: { value: "n1" } });
    fireEvent.click(screen.getByRole("button", { name: /Change password/ }));
    expect(await screen.findByText("Update the other connections too?")).toBeTruthy();
    expect(onClose).not.toHaveBeenCalled();
    // Both ticked; one unticked is left out.
    fireEvent.click(screen.getByLabelText("Sales copy"));
    fireEvent.click(screen.getByRole("button", { name: /Update it too/ }));
    expect(await screen.findByText("Couldn't sign in to check the new password: timeout")).toBeTruthy();
    expect(update).toHaveBeenCalledWith("c1", ["c3"], "n1");
    fireEvent.click(screen.getByText("Close", { selector: "button" }));
    expect(onClose).toHaveBeenCalled();
  });

  it("with a password saved, asks only for the new one and lets the app use the saved one", async () => {
    const change = vi.spyOn(api, "changePassword").mockResolvedValueOnce({ others: [] });
    const onClose = vi.fn();
    render(<ChangePasswordDialog conn={conn("oracle", "password", true)} onClose={onClose} />);
    expect(screen.queryByLabelText("Current password")).toBeNull();
    fireEvent.change(screen.getByLabelText("New password"), { target: { value: "n1" } });
    fireEvent.change(screen.getByLabelText("New password again"), { target: { value: "n1" } });
    fireEvent.click(screen.getByRole("button", { name: /Change password/ }));
    await waitFor(() => expect(onClose).toHaveBeenCalled());
    // null: the saved password, read by the app (it never comes to the window).
    expect(change).toHaveBeenCalledWith("c1", null, "n1");
  });

  it("an error saying the password expired is announced with its connection; others aren't", () => {
    const seen: string[] = [];
    const on = (e: Event) => seen.push((e as CustomEvent<{ connectionId: string }>).detail.connectionId);
    window.addEventListener("db:password-expired", on);
    announcePasswordExpired({ kind: "auth", code: "password_expired", message: "ORA-28001: the password has expired" }, "c1");
    announcePasswordExpired({ kind: "auth", message: "ORA-01017: invalid username/password" }, "c2");
    announcePasswordExpired({ kind: "auth", code: "password_expired", message: "x" }, null);
    window.removeEventListener("db:password-expired", on);
    expect(seen).toEqual(["c1"]);
  });

  it("opens for the connection whose password expired, asking only the new one", async () => {
    useStore.setState({ connections: [conn("oracle", "password", true)], expiredPassword: "c1" });
    const change = vi.spyOn(api, "changePassword").mockResolvedValueOnce({ others: [] });
    render(<ExpiredPasswordDialog />);
    expect(await screen.findByText("The password of app has expired")).toBeTruthy();
    expect(screen.queryByLabelText("Current password")).toBeNull();
    expect(screen.getByRole("status").textContent).toContain('refuses "Sales" until app has a new password');
    fireEvent.change(screen.getByLabelText("New password"), { target: { value: "n1" } });
    fireEvent.change(screen.getByLabelText("New password again"), { target: { value: "n1" } });
    fireEvent.click(screen.getByRole("button", { name: /Change password/ }));
    await waitFor(() => expect(useStore.getState().expiredPassword).toBeNull());
    expect(change).toHaveBeenCalledWith("c1", null, "n1");
  });

  it("after Cancel, the explorer doesn't ask again for that connection; a run does, and one at a time", () => {
    const ask = useStore.getState().askExpiredPassword;
    ask({ connectionId: "c1", run: false });
    expect(useStore.getState().expiredPassword).toBe("c1");
    ask({ connectionId: "c2", run: true });
    expect(useStore.getState().expiredPassword).toBe("c1");
    // What Cancel in the dialog leaves.
    useStore.setState({ expiredPassword: null, expiredDismissed: ["c1"] });
    ask({ connectionId: "c1", run: false });
    expect(useStore.getState().expiredPassword).toBeNull();
    ask({ connectionId: "c1", run: true });
    expect(useStore.getState().expiredPassword).toBe("c1");
  });

  it("Cancel remembers the dismissal; a change clears it and reloads the explorer", async () => {
    useStore.setState({ connections: [conn("oracle", "password", true)], expiredPassword: "c1" });
    const { unmount } = render(<ExpiredPasswordDialog />);
    fireEvent.click(await screen.findByText("Cancel"));
    expect(useStore.getState().expiredDismissed).toEqual(["c1"]);
    unmount();
    const load = vi.fn().mockResolvedValue([]);
    useStore.setState({ expiredPassword: "c1", loadSchemas: load });
    vi.spyOn(api, "changePassword").mockResolvedValueOnce({ others: [] });
    render(<ExpiredPasswordDialog />);
    fireEvent.change(screen.getByLabelText("New password"), { target: { value: "n1" } });
    fireEvent.change(screen.getByLabelText("New password again"), { target: { value: "n1" } });
    fireEvent.click(screen.getByRole("button", { name: /Change password/ }));
    await waitFor(() => expect(load).toHaveBeenCalledWith("c1", true));
    expect(useStore.getState().expiredDismissed).toEqual([]);
  });

  it("asks for the current password after all when the keychain doesn't give the saved one", async () => {
    vi.spyOn(api, "changePassword").mockRejectedValueOnce({ kind: "invalid", message: "No password is saved for this connection: type the current one" });
    render(<ChangePasswordDialog conn={conn("oracle", "password", true)} onClose={vi.fn()} />);
    expect(screen.queryByLabelText("Current password")).toBeNull();
    fireEvent.change(screen.getByLabelText("New password"), { target: { value: "n1" } });
    fireEvent.change(screen.getByLabelText("New password again"), { target: { value: "n1" } });
    fireEvent.click(screen.getByRole("button", { name: /Change password/ }));
    expect(await screen.findByLabelText("Current password")).toBeTruthy();
  });

  it("an expired password DataBrain can't change is said, without a dialog", async () => {
    const ro = conn("oracle", "password", true);
    ro.config.read_only = true;
    const toast = vi.fn();
    useStore.setState({ connections: [ro], expiredPassword: "c1", toast });
    render(<ExpiredPasswordDialog />);
    await waitFor(() => expect(useStore.getState().expiredPassword).toBeNull());
    expect(toast).toHaveBeenCalledWith(expect.stringContaining('The password of "Sales" has expired'), "error");
    expect(screen.queryByText(/has expired/)).toBeNull();
  });
});
