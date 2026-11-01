import { act, render, screen } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { afterEach, describe, expect, it, vi } from "vitest";
import { useStore } from "../store";
import { TabErrorBoundary } from "./TabErrorBoundary";
import { Toasts } from "./ui";

afterEach(() => useStore.setState({ toasts: [] }));

describe("Toasts", () => {
  it("shows errors as alerts and dismisses on click", async () => {
    render(<Toasts />);
    act(() => {
      useStore.getState().toast("Saved", "success");
      useStore.getState().toast("Could not connect", "error");
    });
    expect(screen.getByRole("alert")).toHaveTextContent("Could not connect");
    expect(screen.getByRole("status")).toHaveTextContent("Saved");
    await userEvent.click(screen.getAllByRole("button", { name: "Dismiss" })[1]);
    expect(screen.queryByRole("alert")).not.toBeInTheDocument();
    expect(screen.getByRole("status")).toBeInTheDocument();
  });
});

describe("TabErrorBoundary", () => {
  it("keeps a crash inside the tab and recovers with Try again", async () => {
    vi.spyOn(console, "error").mockImplementation(() => {});
    let fail = true;
    function Boom() {
      if (fail) throw new Error("bad plan");
      return <p>fine</p>;
    }
    render(
      <TabErrorBoundary label="tab">
        <Boom />
      </TabErrorBoundary>,
    );
    expect(screen.getByText("This tab hit an error")).toBeInTheDocument();
    expect(screen.getByText("bad plan")).toBeInTheDocument();
    fail = false;
    await userEvent.click(screen.getByRole("button", { name: "Try again" }));
    expect(screen.getByText("fine")).toBeInTheDocument();
    vi.restoreAllMocks();
  });
});
