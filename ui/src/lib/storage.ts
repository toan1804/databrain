// Values the UI keeps in localStorage. A value from an older version, or
// one edited or cut short, must never break the window: it is checked,
// and dropped when it doesn't fit.

/** `key`'s JSON value when `fits` accepts it, else `fallback` (the bad value is removed). */
export function readStored<T>(key: string, fallback: T, fits: (v: unknown) => v is T): T {
  try {
    const raw = localStorage.getItem(key);
    if (raw === null) return fallback;
    const v: unknown = JSON.parse(raw);
    if (fits(v)) return v;
  } catch {
    /* not JSON */
  }
  try {
    localStorage.removeItem(key);
  } catch {
    /* storage unavailable */
  }
  return fallback;
}

export const isStringList = (v: unknown): v is string[] => Array.isArray(v) && v.every((x) => typeof x === "string");

export const isListRecord = (v: unknown): v is Record<string, string[]> =>
  !!v && typeof v === "object" && !Array.isArray(v) && Object.values(v).every(isStringList);

/** Every key DataBrain keeps in localStorage (layout, panels, chats…). */
export function clearStoredLayout() {
  try {
    for (const k of Object.keys(localStorage)) if (k.startsWith("db.")) localStorage.removeItem(k);
  } catch {
    /* storage unavailable */
  }
}
