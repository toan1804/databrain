import { describe, expect, it } from "vitest";
import { cellDeps, extractMentions, outputLabel, outputRef, referencedOutputs } from "./dataflow";
import type { NotebookCell, OutputInfo } from "./types";

const out = (handle: string, name: string | null = null, version_of: [string, number] | null = null) =>
  ({ handle, name, version_of }) as unknown as OutputInfo;

describe("output references", () => {
  it("builds refs and labels", () => {
    expect(outputRef(out("r12"))).toBe("results.r12");
    expect(outputRef(out("r3", "revenue"))).toBe("results.revenue");
    expect(outputRef(out("r1", null, ["revenue", 1]))).toBe("results.revenue__1");
    expect(outputRef(out("r4", "Big Name"))).toBe('results."Big Name"');
    expect(outputLabel(out("r1", null, ["revenue", 2]))).toBe("revenue~2");
  });

  it("finds results.<name> in SQL, skipping comments and strings", () => {
    const sql = `select * from results.revenue r join RESULTS . "Big Name" b on true
      -- results.commented
      where x = 'results.str' and y in (select 1 from results.r12) and z = myresults.no`;
    expect(referencedOutputs(sql)).toEqual(["revenue", "Big Name", "r12"]);
  });

  it("extracts only known mentions", () => {
    const outputs = [out("r1", "revenue"), out("r2")];
    expect(extractMentions("why did @revenue drop vs @r2? mail me@example.com @unknown", outputs)).toEqual(["revenue", "r2"]);
  });
});

describe("notebook dataflow", () => {
  const cell = (id: string, source: string, output_name?: string, ranAt?: number): NotebookCell => ({
    id,
    kind: "sql",
    source,
    output_name: output_name ?? null,
    last_run: ranAt ? { finished_at: ranAt, duration_ms: 1 } : null,
  });
  it("links producers, dependents, staleness and missing inputs", () => {
    const cells = [
      cell("a", "select 1", "base", 200),
      cell("b", "select * from results.base", "agg", 100),
      cell("c", "select * from results.agg join results.nope using (x) join results.r7 using (y)", undefined, 300),
    ];
    const d = cellDeps(cells, []);
    expect(d.b.producers).toEqual(["a"]);
    expect(d.a.dependents).toEqual(["b"]);
    expect(d.b.stale).toBe(true); // a ran after b
    expect(d.c.stale).toBe(false);
    expect(d.c.missing).toEqual(["nope"]); // handles like r7 are not reported
    expect(cellDeps(cells, ["nope"]).c.missing).toEqual([]);
    // Versions resolve to their base name.
    expect(cellDeps([cell("a", "select 1", "base"), cell("b", "select * from results.base__1")], []).b.producers).toEqual(["a"]);
  });
});
