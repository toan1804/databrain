import { act, render, screen } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { useState } from "react";
import { afterEach, describe, expect, it, vi } from "vitest";
import { useStore } from "../store";
import { ConfirmDialog, Modal } from "./ui";

afterEach(() => useStore.setState({ confirm: null }));

function Opener() {
  const [open, setOpen] = useState(false);
  return (
    <>
      <button onClick={() => setOpen(true)}>Open</button>
      {open && (
        <Modal title="Rename" onClose={() => setOpen(false)} footer={<button>Save</button>}>
          <input aria-label="Name" />
        </Modal>
      )}
    </>
  );
}

describe("Modal", () => {
  it("is named by its title, keeps Tab inside and gives focus back on close", async () => {
    const user = userEvent.setup();
    render(<Opener />);
    await user.click(screen.getByRole("button", { name: "Open" }));
    const dialog = screen.getByRole("dialog", { name: "Rename" });
    const name = screen.getByRole("textbox", { name: "Name" });
    const close = screen.getByRole("button", { name: "Close" });
    const save = screen.getByRole("button", { name: "Save" });
    expect(name).toHaveFocus();
    // Order in the dialog: Close (header), Name (body), Save (footer).
    await user.tab();
    expect(save).toHaveFocus();
    await user.tab();
    expect(close).toHaveFocus();
    await user.tab({ shift: true });
    expect(save).toHaveFocus();
    expect(dialog).toContainElement(document.activeElement as HTMLElement);
    await user.keyboard("{Escape}");
    expect(screen.queryByRole("dialog")).not.toBeInTheDocument();
    expect(screen.getByRole("button", { name: "Open" })).toHaveFocus();
  });
});

describe("ConfirmDialog", () => {
  it("has the confirmation's title as its accessible name", async () => {
    const onConfirm = vi.fn();
    render(<ConfirmDialog />);
    act(() => useStore.getState().askConfirm({ title: "Delete step \"load\"?", reasons: ["Its SQL is removed."], confirmLabel: "Delete", onConfirm }));
    expect(screen.getByRole("dialog", { name: 'Delete step "load"?' })).toBeInTheDocument();
    await userEvent.click(screen.getByRole("button", { name: "Delete" }));
    expect(onConfirm).toHaveBeenCalledOnce();
    expect(screen.queryByRole("dialog")).not.toBeInTheDocument();
  });
});
