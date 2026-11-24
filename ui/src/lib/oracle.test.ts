import { describe, expect, it } from "vitest";
import { isInstantClient } from "./oracle";

describe("Oracle driver option", () => {
  it("reads thick as Instant Client, as the app does; anything else is thin", () => {
    expect(["instant_client", "thick", " thick "].map(isInstantClient)).toEqual([true, true, true]);
    expect(["", "thin", undefined, null, "oci"].map(isInstantClient)).toEqual([false, false, false, false, false]);
  });
});
