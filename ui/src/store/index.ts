import { create } from "zustand";
import { createAppSlice } from "./appSlice";
import { createConnectionsSlice } from "./connectionsSlice";
import { createExplorerSlice } from "./explorerSlice";
import { createLibrarySlice } from "./librarySlice";
import { createParamsSlice } from "./paramsSlice";
import { createRunsSlice } from "./runsSlice";
import { createTabsSlice } from "./tabsSlice";
import { createUiSlice } from "./uiSlice";
import type { State } from "./types";

export * from "./types";
export { RESTORED_NOTICE, applyTheme, baseName, chosenSchemaIds, nextSignIn, parseSchemaFilter, restoredRun } from "./helpers";
export { _earlyEventCount } from "./runsSlice";

export const useStore = create<State>()((...a) => ({
  ...createAppSlice(...a),
  ...createUiSlice(...a),
  ...createTabsSlice(...a),
  ...createRunsSlice(...a),
  ...createConnectionsSlice(...a),
  ...createExplorerSlice(...a),
  ...createLibrarySlice(...a),
  ...createParamsSlice(...a),
}));

export const useActiveTab = () =>
  useStore((s) => s.tabs.find((t) => t.id === s.activeTabId) ?? null);
