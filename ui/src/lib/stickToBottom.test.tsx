import { act, fireEvent, render } from "@testing-library/react";
import { useRef, useState } from "react";
import { describe, expect, it } from "vitest";
import { useStickToBottom } from "./hooks";

/** jsdom has no layout: the chat box reports 100px visible and 50px per message. */
function setup() {
  let add = () => {};
  let pin = () => {};
  function Chat() {
    const box = useRef<HTMLDivElement | null>(null);
    const [items, setItems] = useState(["hello"]);
    const stick = useStickToBottom(box, items);
    add = () => setItems((xs) => [...xs, `token ${xs.length}`]);
    pin = stick.pin;
    return (
      <div
        data-testid="box"
        ref={(el) => {
          box.current = el;
          if (!el) return;
          Object.defineProperty(el, "clientHeight", { configurable: true, get: () => 100 });
          Object.defineProperty(el, "scrollHeight", { configurable: true, get: () => el.children.length * 50 });
          el.scrollTo = ((o: ScrollToOptions) => {
            el.scrollTop = Math.max(0, Math.min(o.top ?? 0, el.scrollHeight - 100));
          }) as typeof el.scrollTo;
        }}
        onScroll={stick.onScroll}
      >
        {items.map((t) => (
          <p key={t}>{t}</p>
        ))}
      </div>
    );
  }
  const { getByTestId } = render(<Chat />);
  return { box: getByTestId("box"), add: () => act(() => add()), pin: () => act(() => pin()) };
}

describe("AI chat scrolling", () => {
  it("follows new content at the bottom", () => {
    const { box, add } = setup();
    for (let i = 0; i < 5; i++) add();
    expect(box.scrollTop).toBe(6 * 50 - 100);
  });

  it("stops following when the user scrolls up, and follows again after pin()", () => {
    const { box, add, pin } = setup();
    for (let i = 0; i < 5; i++) add();
    box.scrollTop = 20;
    fireEvent.scroll(box);
    add();
    add();
    expect(box.scrollTop).toBe(20);
    // Sending a message pins it to the bottom again.
    pin();
    add();
    expect(box.scrollTop).toBe(9 * 50 - 100);
  });

  it("follows again once the user scrolls back to the bottom", () => {
    const { box, add } = setup();
    for (let i = 0; i < 5; i++) add();
    box.scrollTop = 0;
    fireEvent.scroll(box);
    add();
    expect(box.scrollTop).toBe(0);
    box.scrollTop = box.scrollHeight - 100 - 10;
    fireEvent.scroll(box);
    add();
    expect(box.scrollTop).toBe(box.scrollHeight - 100);
  });
});
