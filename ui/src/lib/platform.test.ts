import { describe, expect, it } from "vitest";
import { kbd, shortcutFor } from "./platform";

describe("shortcuts", () => {
  it("are shown the Windows/Linux way off macOS", () => {
    expect(shortcutFor("⌘S", false)).toBe("Ctrl+S");
    expect(shortcutFor("⇧⌘↵", false)).toBe("Ctrl+Shift+Enter");
    expect(shortcutFor("⇧⌥F", false)).toBe("Alt+Shift+F");
    expect(shortcutFor("⌘", false)).toBe("Ctrl");
    expect(shortcutFor("⌘⌫", false)).toBe("Ctrl+Delete");
    expect(kbd("Run all statements (⇧⌘↵)", false)).toBe("Run all statements (Ctrl+Shift+Enter)");
    expect(kbd("SQL…  ⌘↵ run · ⇧↵ run & next · ⌘I ask AI", false)).toBe("SQL…  Ctrl+Enter run · Shift+Enter run & next · Ctrl+I ask AI");
    expect(kbd("Settings (⌘,)", false)).toBe("Settings (Ctrl+,)");
    expect(kbd("Save (⌘S)", true)).toBe("Save (⌘S)");
  });
});
