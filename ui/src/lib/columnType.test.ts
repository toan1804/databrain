import { describe, expect, it } from "vitest";
import { columnTypeInfo } from "./columnType";
import type { ColumnMeta, OutputInfo } from "./types";

const col = (name: string, db_type: string | null, data_type = "Utf8"): ColumnMeta => ({ name, data_type, db_type, family: "text" });
const out = (p: Partial<OutputInfo>): OutputInfo => ({ handle: "r1", name: null, version_of: null, result_id: "x", connection_id: "c", connection_name: "C", kind: "postgres", sql: "", tab_id: "t", statement_index: 0, created_at: 0, rows: 1, columns: [], truncated: false, row_limit: null, bytes: 0, pinned: false, state: "memory" as never, origin: "user", last_used: 0, active: true, ...p }) as OutputInfo;

describe("column type tooltip", () => {
  it("shows the database's own type, not the Arrow one", () => {
    const outputs = [out({ result_id: "a", kind: "postgres" })];
    const t = columnTypeInfo(col("amount", "numeric(12,2)"), "a", outputs);
    expect([t.type, t.engine]).toEqual(["numeric(12,2)", "postgres"]);
    expect(columnTypeInfo(col("n", null, "Int64"), "zz", []).type).toBe("Int64");
  });

  it("adds the source type for DuckDB queries over outputs", () => {
    const outputs = [
      out({ result_id: "pg", handle: "r1", name: "orders", kind: "postgres", connection_name: "Sales PG", columns: [col("AMOUNT", "numeric(12,2)")] }),
      out({ result_id: "ora", handle: "r2", kind: "oracle", connection_name: "ERP", columns: [col("amount", "NUMBER(10,0)")] }),
      out({ result_id: "d", handle: "r3", kind: "duckdb", sql: 'select * from results.orders o join results."r2" e using (amount)' }),
    ];
    const t = columnTypeInfo(col("amount", "DECIMAL(12,2)"), "d", outputs);
    expect(t.type).toBe("DECIMAL(12,2)");
    expect(t.sources).toEqual([
      { ref: "results.orders", connection: "Sales PG", engine: "postgres", type: "numeric(12,2)" },
      { ref: "results.r2", connection: "ERP", engine: "oracle", type: "NUMBER(10,0)" },
    ]);
  });
});
