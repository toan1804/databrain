import { act, render, screen } from "@testing-library/react";
import { useState } from "react";
import { describe, expect, it, vi } from "vitest";
import { Markdown, type CodeRenderer } from "./Markdown";

describe("Markdown", () => {
  it("renders blocks as elements, never as HTML", () => {
    render(<Markdown text={"# Title\n\n- **one**\n- `two`\n\n<img src=x onerror=alert(1)>"} />);
    expect(screen.getByRole("heading", { level: 1 })).toHaveTextContent("Title");
    expect(screen.getAllByRole("listitem")).toHaveLength(2);
    expect(document.querySelector("img")).toBeNull();
    expect(screen.getByText(/<img src=x/)).toBeInTheDocument();
  });

  it("only the message whose text changed renders again while an answer streams", () => {
    const code = vi.fn<CodeRenderer>((c, _l, k) => <pre key={k}>{c}</pre>);
    let addToken = () => {};
    function Chat() {
      const [last, setLast] = useState("Streaming");
      addToken = () => setLast((t) => t + " more");
      return (
        <>
          <Markdown text={"Earlier answer\n\n```sql\nselect 1\n```"} code={code} />
          <Markdown text={`${last}\n\n\`\`\`sql\nselect 2\n\`\`\``} code={code} />
        </>
      );
    }
    render(<Chat />);
    expect(code).toHaveBeenCalledTimes(2);
    act(() => addToken());
    act(() => addToken());
    // The earlier message's code block was rendered once; the streaming one per token.
    const sqls = code.mock.calls.map((c) => c[0]);
    expect(sqls.filter((s) => s === "select 1")).toHaveLength(1);
    expect(sqls.filter((s) => s === "select 2")).toHaveLength(3);
    expect(screen.getByText("Streaming more more")).toBeInTheDocument();
  });
});
