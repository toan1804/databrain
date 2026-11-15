import { fireEvent, render, screen, waitFor } from "@testing-library/react";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

const calls = { check: 0, install: 0, opened: [] as string[] };
let answer: Record<string, unknown> | null = null;
vi.mock("@tauri-apps/api/app", () => ({ getVersion: async () => "0.3.5" }));
vi.mock("../lib/api", async (orig) => ({
  ...(await orig<typeof import("../lib/api")>()),
  isTauri: () => true,
  onUpdateEvent: async () => () => {},
  api: {
    getSettings: async () => ({}),
    checkForUpdate: async () => {
      calls.check++;
      return answer;
    },
    installUpdate: async () => {
      calls.install++;
    },
    openLink: async (u: string) => {
      calls.opened.push(u);
    },
  },
}));

const { UpdateBanner } = await import("./UpdateBanner");
const info = (p: Record<string, unknown>) => ({ current: "0.3.5", latest: "0.4.1", newer: true, url: "https://release", download: "https://dmg", download_name: "x.dmg", notes: null, published_at: null, installable: true, install_note: null, ...p });

beforeEach(() => {
  calls.check = 0;
  calls.install = 0;
  calls.opened = [];
  localStorage.clear();
});
afterEach(() => localStorage.clear());

describe("update banner", () => {
  it("asks again when the kept result came from the version installed before", async () => {
    // Kept a few minutes ago by 0.3.4, which couldn't install updates.
    localStorage.setItem("db.update.lastCheck", String(Date.now()));
    localStorage.setItem("db.update.info", JSON.stringify(info({ current: "0.3.4", installable: false })));
    answer = info({});
    render(<UpdateBanner />);
    await screen.findByText("Update and relaunch");
    expect(calls.check).toBe(1);
  });

  it("a click on Download asks again, and installs when it now can", async () => {
    localStorage.setItem("db.update.lastCheck", String(Date.now()));
    localStorage.setItem("db.update.info", JSON.stringify(info({ installable: false })));
    answer = info({});
    render(<UpdateBanner />);
    fireEvent.click(await screen.findByText("Download"));
    await waitFor(() => expect(calls.install).toBe(1));
    expect(calls.opened).toEqual([]);
  });

  it("opens the download, saying why, when it still can't install", async () => {
    localStorage.setItem("db.update.lastCheck", String(Date.now()));
    localStorage.setItem("db.update.info", JSON.stringify(info({ installable: false })));
    answer = info({ installable: false, install_note: "this build has no updater key" });
    render(<UpdateBanner />);
    fireEvent.click(await screen.findByText("Download"));
    await waitFor(() => expect(calls.opened).toEqual(["https://dmg"]));
    expect(calls.install).toBe(0);
  });
});
