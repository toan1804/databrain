import { render, screen, waitFor } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { beforeEach, describe, expect, it, vi } from "vitest";
import { api } from "../lib/api";
import type { ParamSet } from "../lib/types";
import { useStore } from "../store";
import { ParamSetPicker } from "./ParamBar";

let sets: ParamSet[] = [];

beforeEach(() => {
  sets = [{ id: "s1", name: "Daily", values: { day: { value: "2026-11-01" }, tenant: { value: "acme" } }, updated_at: 0 }];
  vi.spyOn(api, "listParamSets").mockImplementation(async () => sets);
  vi.spyOn(api, "saveParamSet").mockImplementation(async (s) => {
    const saved = { ...s, id: s.id || "s2" };
    sets = [...sets.filter((x) => x.id !== saved.id), saved];
    return saved;
  });
  useStore.setState({ params: { t1: { day: { value: "2026-12-24" } } }, paramSets: [] });
});

describe("parameter sets in the parameter bar", () => {
  it("applies a saved set to the tab's boxes", async () => {
    const user = userEvent.setup();
    render(<ParamSetPicker tabId="t1" names={["day", "tenant"]} />);
    const pick = screen.getByRole("combobox", { name: "Parameter set" });
    await waitFor(() => expect(screen.getByRole("option", { name: "Daily" })).toBeInTheDocument());
    await user.selectOptions(pick, "s1");
    expect(useStore.getState().params.t1).toMatchObject({ day: { value: "2026-11-01" }, tenant: { value: "acme" } });
  });

  it("saves the current values as a new set", async () => {
    const user = userEvent.setup();
    render(<ParamSetPicker tabId="t1" names={["day", "tenant"]} />);
    await waitFor(() => expect(screen.getByRole("option", { name: "Daily" })).toBeInTheDocument());
    await user.selectOptions(screen.getByRole("combobox", { name: "Parameter set" }), "__new");
    await user.type(screen.getByRole("textbox", { name: "Name of the new parameter set" }), "Christmas");
    await user.click(screen.getByRole("button", { name: "Save" }));
    await waitFor(() => expect(api.saveParamSet).toHaveBeenCalledWith({ id: "", name: "Christmas", values: { day: { value: "2026-12-24" } }, updated_at: 0 }));
    expect(await screen.findByRole("option", { name: "Christmas" })).toBeInTheDocument();
  });
});
