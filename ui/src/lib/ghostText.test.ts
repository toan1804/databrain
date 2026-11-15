import { describe, expect, it } from "vitest";
import { canSuggest } from "./ghostText";

const at = (s: string) => canSuggest(s.replace("|", ""), s.indexOf("|"));

describe("when an inline suggestion is asked for", () => {
  it("at the end of SQL being written", () => {
    expect(at("select id, |")).toBe(true);
    expect(at("select * from orders where |")).toBe(true);
  });

  it("not in a string or comment, also one over several lines", () => {
    expect(at("select 'abc|")).toBe(false);
    expect(at("select 1 -- note |")).toBe(false);
    expect(at("/* a comment\nthat goes on |")).toBe(false);
    expect(at("select $$ body\nof a function |")).toBe(false);
    // A closed one before the cursor is fine.
    expect(at("/* note */ select id, |")).toBe(true);
    expect(at("select 'it''s', |")).toBe(true);
  });

  it("not after a finished statement or before text on the line", () => {
    expect(at("select 1;|")).toBe(false);
    expect(at("select | from t")).toBe(false);
  });
});
