import { describe, expect, it } from "vitest";
import { clauseContext, completeSql, tableRefs, type MetaProvider } from "./sqlComplete";
import type { DbObject, SchemaInfo } from "./types";

const t = (schema: string, name: string, kind: DbObject["kind"] = "table"): DbObject => ({ schema, name, kind });

function provider(kind: MetaProvider["kind"] = "postgres"): MetaProvider & { searched: string[] } {
  const schemas: SchemaInfo[] =
    kind === "databricks"
      ? [
          { name: "main.sales", catalog: "main", is_default: true },
          { name: "main.crm", catalog: "main", is_default: false },
          { name: "dev.lab", catalog: "dev", is_default: false },
        ]
      : [
          { name: "public", is_default: true },
          { name: "audit", is_default: false },
        ];
  const objs: Record<string, DbObject[]> =
    kind === "databricks"
      ? { "main.sales": [t("main.sales", "orders"), t("main.sales", "order_items")], "main.crm": [t("main.crm", "customers")], "dev.lab": [t("dev.lab", "orders_test")] }
      : { public: [t("public", "orders"), t("public", "order_items"), t("public", "v_revenue", "view")], audit: [t("audit", "logins")] };
  const cols: Record<string, string[]> = { orders: ["order_id", "customer_id", "amount", "created_at"], order_items: ["order_id", "product_id", "qty"], customers: ["customer_id", "name", "country"], logins: ["user_id", "at"] };
  const searched: string[] = [];
  return {
    kind,
    searched,
    schemas: () => schemas,
    objects: async (s) => objs[s],
    cachedObjects: () => objs[schemas[0].name],
    searchTables: async (q) => {
      searched.push(q);
      return Object.values(objs).flat().filter((o) => o.name.includes(q.toLowerCase()));
    },
    columns: async (_s, name) => cols[name],
    virtualTables: () => (kind === "duckdb" ? [{ schema: "results", name: "revenue", columns: ["country", "total"] }] : []),
  };
}

const at = async (sql: string, p: MetaProvider = provider(), explicit = false) => {
  const pos = sql.indexOf("|");
  const doc = sql.replace("|", "");
  const r = await completeSql(doc, pos, p, explicit);
  return r ? r.options.map((o) => o.apply ?? o.label) : [];
};
const labels = async (sql: string, p: MetaProvider = provider()) => {
  const pos = sql.indexOf("|");
  return (await completeSql(sql.replace("|", ""), pos, p))?.options.map((o) => `${o.type}:${o.label}`) ?? [];
};

describe("clause context", () => {
  it("detects where the cursor is", () => {
    expect(clauseContext("")).toBe("start");
    expect(clauseContext("select * from ")).toBe("table");
    expect(clauseContext("select * from a x, ")).toBe("table");
    expect(clauseContext("select * from orders ")).toBe("after_table");
    expect(clauseContext("select a, ")).toBe("expr");
    expect(clauseContext("select * from t where ")).toBe("expr");
    expect(clauseContext("select * from t join u on ")).toBe("expr");
  });

  it("finds table references and aliases", () => {
    expect(tableRefs("select * from public.orders o join order_items as i on o.id = i.order_id")).toEqual([
      { parts: ["public", "orders"], alias: "o" },
      { parts: ["order_items"], alias: "i" },
    ]);
    expect(tableRefs("select * from a, b y where x")).toEqual([{ parts: ["a"], alias: undefined }, { parts: ["b"], alias: "y" }]);
    expect(tableRefs('select * from "My Schema"."T" where')[0].parts).toEqual(["My Schema", "T"]);
  });
});

describe("completion", () => {
  it("suggests statements at the start, not random words", async () => {
    expect(await at("sel|")).toEqual(["select"]);
    expect(await at("SEL|")).toEqual(["SELECT"]);
    expect(await at("x|")).toEqual([]);
  });

  it("suggests the connection's tables after FROM (server search included)", async () => {
    const p = provider();
    const r = await at("select * from ord|", p);
    expect(r.slice(0, 2)).toEqual(["public.orders o", "public.order_items oi"]);
    expect(r).not.toContain("select");
    expect(p.searched).toEqual(["ord"]);
    expect(await at("select * from log|", p)).toEqual(["audit.logins l"]);
  });

  it("suggests columns of the statement's tables, then functions/keywords", async () => {
    const r = await labels("select am| from orders");
    expect(r[0]).toBe("column:amount");
    const c = await labels("select c| from orders o join customers c2 on o.customer_id = c2.customer_id");
    expect(c.slice(0, 3)).toEqual(["column:customer_id", "column:created_at", "column:country"]);
    const p = provider();
    const both = (await completeSql("select customer from orders o join customers c on true", 15, p))!.options[0];
    expect(both.detail).toBe("orders, customers");
    expect(c).toContain("function:coalesce");
    // word-part prefix: id → order_id, customer_id
    expect((await labels("select * from orders where id|")).slice(0, 2)).toEqual(["column:order_id", "column:customer_id"]);
  });

  it("completes alias. and schema. paths", async () => {
    expect(await at("select o.| from orders o")).toEqual(["order_id", "customer_id", "amount", "created_at"]);
    expect(await at("select o.cu| from orders o")).toEqual(["customer_id"]);
    expect(await at("select * from audit.|")).toEqual(["logins l"]);
    expect(await at("select * from public.orders where public.orders.am|")).toEqual(["amount"]);
  });

  it("handles three-level catalogs (Databricks)", async () => {
    const p = provider("databricks");
    expect(await at("select * from dev.|", p)).toEqual(["lab."]);
    expect(await at("select * from dev.lab.|", p)).toEqual(["orders_test ot"]);
    // Tables in the default catalog need only schema.table; others are fully qualified.
    expect(await at("select * from cust|", p)).toEqual(["crm.customers c"]);
    expect((await at("select * from orde|", p))[0]).toBe("sales.orders o");
    expect(await at("select * from orders_t|", p)).toEqual(["dev.lab.orders_test ot"]);
  });

  it("inserts DuckDB attached files as files.<name>", async () => {
    const files: DbObject[] = [t("memory.files", "order_items", "view")];
    const p: MetaProvider = {
      kind: "duckdb",
      schemas: () => [
        { name: "memory.main", catalog: "memory", is_default: true },
        { name: "memory.files", catalog: "memory", is_default: false },
      ],
      objects: async (s) => (s === "memory.files" ? files : []),
      cachedObjects: () => files,
      searchTables: async () => files,
      columns: async () => ["order_id", "qty"],
    };
    // Bare name (word-part match): full path inserted.
    expect(await at("select * from item|", p)).toEqual(["files.order_items oi"]);
    // Typed the schema: only the table name is completed after it.
    expect(await at("select * from files.ite|", p)).toEqual(["order_items oi"]);
    expect(await at("select * from files.order_items f where f.q|", p)).toEqual(["qty"]);
  });

  it("knows DuckDB results outputs", async () => {
    const p = provider("duckdb");
    expect(await at("select * from results.|", p)).toEqual(["revenue r"]);
    expect(await at("select to| from results.revenue r", p)).toEqual(["total"]);
  });

  it("writes an alias with each table picked in FROM / JOIN", async () => {
    // Unique within the statement, never a keyword.
    expect((await at("select * from orders o join ord|"))[0]).toBe("public.orders o2");
    expect((await at("select * from orders o, order_items|"))[0]).toBe("public.order_items oi");
    expect((await at("select * from log|"))[0]).toBe("audit.logins l");
    // Not when an alias (or more of the name) already follows, and not for INSERT/UPDATE targets.
    expect((await at("select * from ord| x where x.id = 1"))[0]).toBe("public.orders");
    expect((await at("select * from ord| where 1 = 1"))[0]).toBe("public.orders o");
    expect((await at("insert into ord|"))[0]).toBe("public.orders");
    expect((await at("update ord|"))[0]).toBe("public.orders");
    // Schemas to drill into get no alias.
    expect(await at("select * from aud|")).toContain("audit.");
  });

  it("makes short, unique, non-keyword aliases", async () => {
    const { makeAlias } = await import("./sqlComplete");
    expect(makeAlias("customer_orders", [])).toBe("co");
    expect(makeAlias("OrderItems", [])).toBe("oi");
    expect(makeAlias("ORDERS", ["o"])).toBe("o2");
    expect(makeAlias("order_numbers", [])).toBe("on2"); // `on` is a keyword
    expect(makeAlias("t_2024_sales", [])).toBe("ts");
  });

  it("completes functions, procedures and packages (Oracle)", async () => {
    const routines: DbObject[] = [
      t("APP", "FN_TOTAL", "function"),
      t("APP", "PROC_LOG", "procedure"),
      t("APP", "PKG_SALES", "package"),
      t("SYS", "DBMS_OUTPUT", "package"),
    ];
    const members: Record<string, DbObject[]> = {
      "APP.PKG_SALES": [t("APP.PKG_SALES", "NET", "function"), t("APP.PKG_SALES", "REFRESH", "procedure")],
      "SYS.DBMS_OUTPUT": [t("SYS.DBMS_OUTPUT", "PUT_LINE", "procedure")],
    };
    const p: MetaProvider = {
      kind: "oracle",
      schemas: () => [{ name: "APP", is_default: true }, { name: "SYS", is_default: false }],
      objects: async () => [],
      cachedObjects: () => [],
      searchTables: async () => [],
      columns: async () => undefined,
      routines: async (schema, q) => routines.filter((o) => (!schema || o.schema === schema) && o.name.toLowerCase().includes(q.toLowerCase())),
      packageMembers: async (s, n) => members[`${s}.${n}`] ?? [],
    };
    const opts = async (sql: string) => {
      const pos = sql.indexOf("|");
      return (await completeSql(sql.replace("|", ""), pos, p))?.options.map((o) => `${o.type}:${o.apply ?? o.label}`) ?? [];
    };
    // In SQL expressions: functions and packages, schema-qualified, ready to call.
    const e = await opts("select fn_t| from dual");
    expect(e).toContain("function:APP.FN_TOTAL(");
    expect(await opts("select pkg| from dual")).toContain("package:APP.PKG_SALES.");
    expect(await opts("select proc_| from dual")).not.toContain("procedure:APP.PROC_LOG(");
    // Package members after `pkg.` / `schema.pkg.`; procedures only where they can be called.
    expect(await opts("select pkg_sales.| from dual")).toEqual(["function:NET("]);
    expect(await opts("begin app.pkg_sales.re|")).toEqual(["procedure:REFRESH("]);
    expect(await opts("begin dbms_output.put|")).toEqual(["procedure:PUT_LINE("]);
    // Procedure calls at statement start (BEGIN, CALL, EXEC).
    expect(await opts("begin proc_l|")).toContain("procedure:APP.PROC_LOG(");
    expect(await opts("call proc_l|")).toContain("procedure:APP.PROC_LOG(");
    // schema. lists its routines too, and never in FROM.
    expect(await opts("select app.fn| from dual")).toContain("function:FN_TOTAL(");
    expect((await opts("select * from fn_t|")).some((o) => o.startsWith("function:"))).toBe(false);
  });

  it("stays quiet in strings and comments", async () => {
    expect(await at("select 'ord|' from t")).toEqual([]);
    expect(await at("-- sel|")).toEqual([]);
  });
});

describe("sqlComplete: key column badges", () => {
  it("labels and ranks partition/index columns first", async () => {
    const p = { ...provider(), columnRoles: (_s: string, t: string) => (t === "orders" ? { created_at: "partition key", customer_id: "indexed" } : undefined) };
    const r = await completeSql("select * from orders where ", 27, p, true);
    const cols = r!.options.filter((o) => o.type === "column");
    expect(cols.slice(0, 2).map((o) => [o.label, o.detail])).toEqual([
      ["customer_id", "orders · indexed"],
      ["created_at", "orders · partition key"],
    ]);
    expect(cols.find((o) => o.label === "amount")?.detail).toBe("orders");
  });
});

describe("sqlComplete: very large catalogs", () => {
  it("stays fast with 20k cached tables and caps the options", async () => {
    const objs: DbObject[] = Array.from({ length: 20_000 }, (_, i) => t(`s${i % 50}`, `table_${i}`));
    const schemas: SchemaInfo[] = Array.from({ length: 50 }, (_, i) => ({ name: `s${i}`, is_default: i === 0 }));
    let schemaCalls = 0;
    const p: MetaProvider = {
      kind: "postgres",
      schemas: () => (schemaCalls++, schemas),
      objects: async () => objs,
      cachedObjects: () => objs,
      searchTables: async () => [],
      columns: async () => [],
    };
    const t0 = performance.now();
    for (const typed of ["t", "ta", "tab", "table_1", "table_12"]) {
      const sql = `select * from ${typed}`;
      const r = await completeSql(sql, sql.length, p);
      expect(r!.options.length).toBeLessThanOrEqual(300);
      expect(r!.options[0].label.startsWith(typed)).toBe(true);
    }
    const ms = (performance.now() - t0) / 5;
    expect(ms).toBeLessThan(150);
    expect(schemaCalls).toBe(5); // once per request, not per table
  });
});
