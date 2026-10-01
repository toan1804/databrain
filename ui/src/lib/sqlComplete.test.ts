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
    expect(r.slice(0, 2)).toEqual(["public.orders", "public.order_items"]);
    expect(r).not.toContain("select");
    expect(p.searched).toEqual(["ord"]);
    expect(await at("select * from log|", p)).toEqual(["audit.logins"]);
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
    expect(await at("select * from audit.|")).toEqual(["logins"]);
    expect(await at("select * from public.orders where public.orders.am|")).toEqual(["amount"]);
  });

  it("handles three-level catalogs (Databricks)", async () => {
    const p = provider("databricks");
    expect(await at("select * from dev.|", p)).toEqual(["lab."]);
    expect(await at("select * from dev.lab.|", p)).toEqual(["orders_test"]);
    // Tables in the default catalog need only schema.table; others are fully qualified.
    expect(await at("select * from cust|", p)).toEqual(["crm.customers"]);
    expect((await at("select * from orde|", p))[0]).toBe("sales.orders");
    expect(await at("select * from orders_t|", p)).toEqual(["dev.lab.orders_test"]);
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
    expect(await at("select * from item|", p)).toEqual(["files.order_items"]);
    // Typed the schema: only the table name is completed after it.
    expect(await at("select * from files.ite|", p)).toEqual(["order_items"]);
    expect(await at("select * from files.order_items f where f.q|", p)).toEqual(["qty"]);
  });

  it("knows DuckDB results outputs", async () => {
    const p = provider("duckdb");
    expect(await at("select * from results.|", p)).toEqual(["revenue"]);
    expect(await at("select to| from results.revenue r", p)).toEqual(["total"]);
  });

  it("stays quiet in strings and comments", async () => {
    expect(await at("select 'ord|' from t")).toEqual([]);
    expect(await at("-- sel|")).toEqual([]);
  });
});
