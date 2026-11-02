import { describe, expect, it } from "vitest";
import { quoteIdent } from "./util";

describe("quoteIdent on backslash-escaping engines", () => {
  it("escapes backslashes before backticks, so a name can't close its quotes", () => {
    for (const kind of ["clickhouse", "bigquery"]) {
      expect(quoteIdent(kind as never, "a\\")).toBe("`a\\\\`");
      expect(quoteIdent(kind as never, "x\\` ; DROP TABLE t; --")).toBe("`x\\\\\\` ; DROP TABLE t; --`");
      expect(quoteIdent(kind as never, "a`b")).toBe("`a\\`b`");
    }
    expect(quoteIdent("mysql", "a`b")).toBe("`a``b`");
  });
});
