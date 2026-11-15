import { describe, expect, it } from "vitest";
import { blankLiterals, inStringOrComment, outputNames, outputRefs, renameOutputRefs, sqlSpans } from "./sqlText";

describe("sqlText: strings, comments and quoted names", () => {
  it("finds each kind, closed or not", () => {
    const sql = "select 'a''b', \"it's\" -- note\nfrom t /* c */ where x = $q$ ; $q$ and y = 'open";
    expect(sqlSpans(sql).map((s) => [s.kind, sql.slice(s.start, s.end), s.closed])).toEqual([
      ["string", "'a''b'", true],
      ["name", '"it\'s"', true],
      ["comment", "-- note", false],
      ["comment", "/* c */", true],
      ["string", "$q$ ; $q$", true],
      ["string", "'open", false],
    ]);
  });

  it("reads a backslash before a quote as an escape only where asked", () => {
    const sql = "select 'O\\'Brien', x";
    expect(blankLiterals(sql, true).endsWith(", x")).toBe(true);
    // Without: the string ends at the quote after the backslash.
    expect(sqlSpans(sql)[0].end).toBe(sql.indexOf("'Brien") + 1);
  });

  it("blanks strings and comments, keeping offsets, lines and quoted names", () => {
    const sql = "select \"a b\", 'x'\n/* one\ntwo */ $$body$$ from t";
    const clean = blankLiterals(sql);
    expect(clean.length).toBe(sql.length);
    expect(clean.split("\n").length).toBe(sql.split("\n").length);
    expect(clean).toContain('"a b"');
    expect(clean).not.toMatch(/'x'|one|two|body/);
    // Postgres parameters are not dollar quotes.
    expect(blankLiterals("select $1, $2")).toBe("select $1, $2");
  });

  it("knows when a cursor is inside a string or comment, across lines", () => {
    const at = (s: string) => inStringOrComment(s.replace("|", ""), s.indexOf("|"));
    expect(at("select 'abc|")).toBe(true);
    expect(at("select 'abc'|")).toBe(false);
    expect(at("select 1 -- why|")).toBe(true);
    expect(at("select 1 -- why\nfrom t|")).toBe(false);
    expect(at("/* a long\ncomment, still| */")).toBe(true);
    expect(at("/* done */ select|")).toBe(false);
    expect(at('select "it\'s" from t|')).toBe(false);
  });
});

describe("sqlText: results.<name> references", () => {
  it("reads them as DuckDB does", () => {
    const sql = [
      "select * from results.sales a",
      "join RESULTS . \"Big Name\" b on 1 = 1",
      "join results/**/.hidden c on 1 = 1",
      "join \"results\".quoted d on 1 = 1",
      "join results.\"a\"\"b\" e on 1 = 1",
      "join results.ventes_été f on 1 = 1",
      "where x = 'results.in_string' -- results.in_comment",
      "and y in (select 1 from myresults.not_this, results_tbl.nor_this)",
    ].join("\n");
    expect(outputNames(sql)).toEqual(["sales", "Big Name", "hidden", "quoted", 'a"b', "ventes_été"]);
  });

  it("gives each reference's place, so it can be rewritten", () => {
    const sql = 'select * from results.sales s, results."sales" t';
    const refs = outputRefs(sql);
    expect(refs.map((r) => sql.slice(r.from, r.to))).toEqual(["sales", '"sales"']);
    expect(refs.map((r) => r.quoted)).toEqual([false, true]);
  });

  it("renames real references only", () => {
    const sql = "select * from results.orders o, RESULTS.\"orders\" p, results.orders_old q where note = 'results.orders' -- results.orders";
    expect(renameOutputRefs(sql, "ORDERS", "sales")).toBe(
      "select * from results.sales o, RESULTS.sales p, results.orders_old q where note = 'results.orders' -- results.orders",
    );
    expect(renameOutputRefs("select * from results.a", "a", "with space")).toBe('select * from results."with space"');
  });
});

describe("sqlText: per dialect", () => {
  it("reads dollar quotes only where the database has them", async () => {
    const { lexOptionsFor } = await import("./sqlText");
    const body = "create procedure p() begin select 1; end $$\nselect * from t where ";
    // MySQL: `DELIMITER $$` is no string; the code after it is code.
    expect(blankLiterals(body, lexOptionsFor("mysql")).endsWith("where ")).toBe(true);
    expect(blankLiterals("select $q$ x $q$", lexOptionsFor("postgres"))).toBe("select " + " ".repeat(9));
    // Snowflake: only `$$`.
    expect(blankLiterals("select $$ x $$, $q$", lexOptionsFor("snowflake"))).toBe("select " + " ".repeat(7) + ", $q$");
  });

  it("reads E'…' strings, where \\' doesn't end them", () => {
    const sql = "select e'it\\'s', * from results.after_it";
    expect(outputNames(sql)).toEqual(["after_it"]);
    // `type'…'` (a typed literal) is no escape string.
    expect(sqlSpans("select date'2024-01-01'")[0].start).toBe(11);
  });
});
