import { act, render } from "@testing-library/react";
import { useEffect, useState } from "react";
import { describe, expect, it } from "vitest";
import { useEvent } from "./hooks";

describe("useEvent", () => {
  it("is stable across renders and calls the latest function", () => {
    const seen: number[] = [];
    const identities = new Set<unknown>();
    let bump = () => {};
    function Probe() {
      const [n, setN] = useState(0);
      bump = () => setN((x) => x + 1);
      const read = useEvent(() => n);
      identities.add(read);
      // Runs once: the function's identity never changes.
      useEffect(() => {
        seen.push(read());
      }, [read]);
      return <button onClick={() => seen.push(read())}>{n}</button>;
    }
    const { getByRole } = render(<Probe />);
    act(() => bump());
    act(() => bump());
    act(() => getByRole("button").click());
    expect(identities.size).toBe(1);
    expect(seen).toEqual([0, 2]);
  });
});
