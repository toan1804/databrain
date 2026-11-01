import { useStore } from "../../store";

export const cellKey = (nbId: string, cellId: string) => `nb:${nbId}:${cellId}`;

/** Wait until the run for `key` is no longer running. */
export function waitForRun(key: string): Promise<void> {
  return new Promise((resolve) => {
    const check = () => !useStore.getState().runs[key]?.running;
    if (check()) return resolve();
    const unsub = useStore.subscribe((s) => {
      if (!s.runs[key]?.running) {
        unsub();
        resolve();
      }
    });
  });
}
