import { describe, expect, it } from "vitest";
import { hexDump, imageType, prettyXml, valueKinds, valueSize } from "./valueView";

describe("valueView", () => {
  it("tells what a value is", () => {
    expect(valueKinds('{"a": [1, 2]}', "text")).toEqual(["json", "text"]);
    expect(valueKinds("[1, 2", "text")).toEqual(["text"]);
    expect(valueKinds("<a><b>1</b></a>", "text")).toEqual(["xml", "text"]);
    expect(valueKinds("a < b > c", "text")).toEqual(["text"]);
    expect(valueKinds("89504e470d0a1a0a0000", "binary")).toEqual(["image", "hex", "text"]);
    expect(valueKinds("00ff", "binary")).toEqual(["hex", "text"]);
    expect(imageType("ffd8ffe0")).toBe("image/jpeg");
  });

  it("formats XML and hex", () => {
    expect(prettyXml("<a><b>1</b><c/></a>")).toBe("<a>\n  <b>1</b>\n  <c/>\n</a>");
    expect(hexDump("48656c6c6f00")).toBe("00000000  48 65 6c 6c 6f 00" + " ".repeat(30) + "  Hello.");
    expect(valueSize("ab\ncd", "text")).toBe("5 chars · 2 lines");
    expect(valueSize("00".repeat(2048), "binary")).toBe("2.0 KB binary");
  });
});
