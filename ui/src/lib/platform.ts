// The computer the app runs on, and keyboard shortcuts written for it:
// shortcuts are written the macOS way (⌘S, ⇧⌘↵) and shown as Ctrl+S,
// Ctrl+Shift+Enter on Windows and Linux.

const ua = typeof navigator === "undefined" ? "" : `${navigator.platform ?? ""} ${navigator.userAgent ?? ""}`;

export const isMac = /Mac|iPhone|iPad/.test(ua);
export const isWindows = /Win/.test(ua);

const KEYS: Record<string, string> = { "↵": "Enter", "⌫": "Delete", "⎋": "Esc" };

/** One shortcut (`⇧⌘S`) for Windows/Linux (`Ctrl+Shift+S`). */
export function shortcutFor(s: string, mac = isMac): string {
  if (mac) return s;
  const mods = new Set(s.match(/[⌃⌥⇧⌘]/g) ?? []);
  const key = s.replace(/[⌃⌥⇧⌘]/g, "");
  const parts = [mods.has("⌘") || mods.has("⌃") ? "Ctrl" : "", mods.has("⌥") ? "Alt" : "", mods.has("⇧") ? "Shift" : ""].filter(Boolean);
  const k = [...key].map((c) => KEYS[c] ?? c).join("");
  return k ? [...parts, k.length === 1 ? k.toUpperCase() : k].join("+") : parts.join("+");
}

/** Every shortcut in a text (`Save (⌘S)`, `⌘↵ run · ⇧↵ next`) for this computer. */
export function kbd(text: string, mac = isMac): string {
  if (mac) return text;
  return text.replace(/[⌃⌥⇧⌘]+(?:[↵⌫⎋↑↓←→]|[A-Za-z0-9.,[\]+]|\/)?/g, (m) => shortcutFor(m, false));
}
