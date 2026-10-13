import { describe, expect, it } from "vitest";
import { sqlFoldPairs } from "./components/sqlFold";

const folded = (doc: string) => sqlFoldPairs(doc).map((p) => [doc.slice(p.open, p.open + 6).split(/\s/)[0], doc.slice(p.from, p.to)]);

describe("sql folding", () => {
  it("folds multi-line parentheses, CASE and BEGIN … END blocks", () => {
    const doc = [
      "with t as (", //
      "  select id,",
      "    case",
      "      when a = ')' then 1",
      "      else 2",
      "    end as k",
      "  from x (1)",
      ")",
      "select * from t;",
    ].join("\n");
    const f = folded(doc);
    expect(f.map(([o]) => o)).toEqual(["(", "case"]);
    expect(f[0][1].trim().startsWith("select id")).toBe(true);
    expect(f[0][1].endsWith("from x (1)")).toBe(true);
    expect(f[1][1].trim()).toBe("when a = ')' then 1\n      else 2");
  });

  it("BEGIN blocks with END IF / END LOOP inside, and transactions", () => {
    const doc = "begin\n  if x then\n    null;\n  end if;\n  loop exit; end loop;\nend;\nbegin transaction;\ncommit;";
    const f = folded(doc);
    expect(f).toHaveLength(1);
    expect(f[0][0]).toBe("begin");
    expect(f[0][1]).toBe("\n  if x then\n    null;\n  end if;\n  loop exit; end loop;");
  });

  it("regions, and nothing inside strings or comments", () => {
    const doc = "-- region setup\ncreate table a (x int);\n-- endregion\nselect '(\n' /* ( */ from b;";
    const f = folded(doc);
    expect(f).toHaveLength(1);
    expect(f[0][0]).toBe("--");
    expect(f[0][1]).toBe("\ncreate table a (x int);");
  });

  it("single-line sections are not folded", () => {
    expect(sqlFoldPairs("select (1 + 2), case when a then b end from t")).toEqual([]);
  });
});
