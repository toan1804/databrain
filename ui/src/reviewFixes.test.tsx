// Fixes from the review of ui/src (2026-11): each test names the bug it keeps away.
import { fireEvent, render, screen } from "@testing-library/react";
import { afterEach, describe, expect, it, vi } from "vitest";
import { CommaListInput, parseCommaList } from "./components/connection/parts";
import { destructiveLoad } from "./components/job/LoadFields";
import { hasHiddenSecret } from "./components/sidebar/HistoryPanel";
import { backslashEscapes, blankLiterals } from "./lib/sqlComplete";
import { formatDuration, quoteIdent } from "./lib/util";
import { useStore } from "./store";

afterEach(() => useStore.setState({ confirm: null }));

describe("comma-separated settings", () => {
  it("keeps the comma while typing, so entries stay apart", () => {
    const seen: string[][] = [];
    render(<CommaListInput id="pii" value={[]} onChange={(l) => seen.push(l)} />);
    const input = screen.getByRole("textbox") as HTMLInputElement;
    // Key by key, as typed: the old field showed the parsed list back and lost the comma.
    let text = "";
    for (const ch of "email, *phone*, users.ssn") {
      text += ch;
      fireEvent.change(input, { target: { value: text } });
      expect(input.value).toBe(text);
    }
    expect(seen[seen.length - 1]).toEqual(["email", "*phone*", "users.ssn"]);
  });

  it("parses lists without empty entries", () => {
    expect(parseCommaList(" email ,, users.ssn , ")).toEqual(["email", "users.ssn"]);
    expect(parseCommaList("")).toEqual([]);
  });
});

describe("history", () => {
  it("knows text whose secret was hidden, which must not be run as it is", () => {
    expect(hasHiddenSecret("ALTER USER app IDENTIFIED BY '***'")).toBe(true);
    expect(hasHiddenSecret("select count(*) from t")).toBe(false);
    expect(hasHiddenSecret("select a * b from t")).toBe(false);
  });
});

describe("loads that remove rows", () => {
  it("says what truncate, replace and sync do; the others remove nothing", () => {
    expect(destructiveLoad("truncate")).toContain("deletes every row");
    expect(destructiveLoad("replace")).toContain("drops the table");
    expect(destructiveLoad("sync")).toContain("not in this result");
    for (const m of ["append", "update", "merge"] as const) expect(destructiveLoad(m)).toBeNull();
  });
});

describe("confirm dialog", () => {
  it("answers the dialog it replaces with no, and its own close cancels nothing", () => {
    const first = { title: "Run?", reasons: [], confirmLabel: "Run", onConfirm: vi.fn(), onCancel: vi.fn() };
    const second = { title: "Reload?", reasons: [], confirmLabel: "Reload", onConfirm: vi.fn(), onCancel: vi.fn() };
    const { askConfirm } = useStore.getState();
    askConfirm(first);
    askConfirm(second);
    expect(first.onCancel).toHaveBeenCalledOnce();
    expect(useStore.getState().confirm).toBe(second);
    // Closing after an answer (the dialog passes null) is not a second answer.
    askConfirm(null);
    expect(second.onCancel).not.toHaveBeenCalled();
    expect(first.onConfirm).not.toHaveBeenCalled();
  });
});

describe("completion lexer", () => {
  it("reads a backslash before a quote as the database does", () => {
    const sql = "select * from t where name = 'O\\'Brien' and ";
    // MySQL: one string; what follows is code again.
    expect(backslashEscapes("mysql")).toBe(true);
    expect(blankLiterals(sql, true).endsWith(" and ")).toBe(true);
    // Postgres: the backslash is an ordinary character, the string ends at the first quote after it.
    expect(backslashEscapes("postgres")).toBe(false);
    expect(blankLiterals("select 'C:\\' , x", false)).toContain(", x");
  });

  it("keeps quoted names, so an apostrophe in one opens no string", () => {
    const sql = 'select "it\'s" from t where ';
    const clean = blankLiterals(sql);
    expect(clean).toBe(sql);
    expect(blankLiterals("select `it's` from t where 'x' = ").endsWith(" = ")).toBe(true);
    expect(blankLiterals("a -- it's\nb")).toBe("a        \nb");
  });
});

describe("names and durations", () => {
  it("quotes words only the database at hand reserves", () => {
    expect(quoteIdent("oracle", "COMMENT")).toBe('"COMMENT"');
    expect(quoteIdent("oracle", "LEVEL")).toBe('"LEVEL"');
    expect(quoteIdent("mysql", "rank")).toBe("`rank`");
    expect(quoteIdent("bigquery", "hash")).toBe("`hash`");
    // Left readable where the word is free.
    expect(quoteIdent("postgres", "comment")).toBe("comment");
    expect(quoteIdent("postgres", "audit")).toBe("audit");
  });

  it("never shows 60 seconds", () => {
    expect(formatDuration(119_600)).toBe("2m 0s");
    expect(formatDuration(59_999)).toBe("1m 0s");
    expect(formatDuration(59_900)).toBe("59.9 s");
    expect(formatDuration(61_000)).toBe("1m 1s");
  });
});
