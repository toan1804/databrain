// What the user hid from the activity bar (×) while it ran, by key: job runs
// (`run:<id>`, `bg:<job id>`), full exports (`export:<id>`), loads
// ("Load into…", `load:<id>`). Per item: new work still shows, and a hidden
// item that fails shows its failure (its store keeps the outcome); one that
// ends well is told by a toast instead.
import { create } from "zustand";

interface ActivityState {
  hidden: Record<string, true>;
  /** Hide the work running now (the bar's ×). */
  hide: (keys: string[]) => void;
  /** Show everything hidden again (the Jobs icon, the command palette). */
  showAll: () => void;
  /** An item ended: its key goes. Says whether it was hidden. */
  ended: (key: string) => boolean;
}

export const useActivity = create<ActivityState>((set, get) => ({
  hidden: {},
  hide: (keys) => set((s) => ({ hidden: { ...s.hidden, ...Object.fromEntries(keys.map((k) => [k, true as const])) } })),
  showAll: () => set((s) => (Object.keys(s.hidden).length ? { hidden: {} } : {})),
  ended: (key) => {
    if (!get().hidden[key]) return false;
    set((s) => ({ hidden: Object.fromEntries(Object.entries(s.hidden).filter(([k]) => k !== key)) }));
    return true;
  },
}));
