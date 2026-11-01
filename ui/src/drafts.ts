// Editor text while it is typed: kept here, not in the store's `tabs`, so a
// keystroke doesn't rebuild the tabs array and re-render every component
// that lists tabs. Written to the tab (`Tab.sql`, then saved) once typing
// pauses, and right away before anything reads it (`flushDrafts`).
import { create } from "zustand";
import { useStore } from "./store";

/** Typing pause after which the text goes to the tab (and is saved). */
export const DRAFT_FLUSH_MS = 300;

interface DraftsState {
  /** Text of query tabs not written to their tab yet, by tab id. */
  text: Record<string, string>;
}

export const useDrafts = create<DraftsState>(() => ({ text: {} }));

const timers = new Map<string, ReturnType<typeof setTimeout>>();

/** The editor of `tabId` changed to `text`. */
export function setDraft(tabId: string, text: string) {
  useDrafts.setState((s) => ({ text: { ...s.text, [tabId]: text } }));
  clearTimeout(timers.get(tabId));
  timers.set(
    tabId,
    setTimeout(() => flushDraft(tabId), DRAFT_FLUSH_MS),
  );
}

/** Write the pending text of `tabId` to its tab now. */
export function flushDraft(tabId: string) {
  clearTimeout(timers.get(tabId));
  timers.delete(tabId);
  const text = useDrafts.getState().text[tabId];
  if (text === undefined) return;
  useDrafts.setState((s) => {
    const next = { ...s.text };
    delete next[tabId];
    return { text: next };
  });
  const st = useStore.getState();
  const tab = st.tabs.find((t) => t.id === tabId);
  if (tab && tab.sql !== text) st.updateTab(tabId, { sql: text });
}

/** Write every pending text to its tab (before reading `Tab.sql`). */
export function flushDrafts() {
  for (const id of Object.keys(useDrafts.getState().text)) flushDraft(id);
}

/** Pending text of a tab, if it was typed since the last flush. */
export const draftOf = (tabId: string): string | undefined => useDrafts.getState().text[tabId];
