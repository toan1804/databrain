// Test data and helpers: a mocked backend with a Postgres connection
// ("Shop"), its schema, and an AI provider.
import { readFileSync } from "node:fs";
import { fileURLToPath } from "node:url";
import { expect, type Page } from "@playwright/test";

const traits: Record<string, unknown> = JSON.parse(readFileSync(new URL("../src/lib/sourceTraits.json", import.meta.url), "utf8"));
const MOCK = fileURLToPath(new URL("./tauri-mock.js", import.meta.url));

const connector = (kind: string, display_name: string, default_port: number | null, fields: { key: string; label: string; required?: boolean }[]) => ({
  kind,
  display_name,
  default_port,
  uses_file: kind === "sqlite" || kind === "duckdb",
  auth_methods: kind === "sqlite" ? ["none"] : ["password"],
  capabilities: { transactions: true, cancel: true, schemas: true, read_only_sessions: true, ssh: kind !== "sqlite" },
  traits: traits[kind],
  fields: fields.map((f) => ({ required: false, ...f })),
});

export const CONNECTORS = [
  connector("postgres", "PostgreSQL", 5432, [
    { key: "host", label: "Host", required: true },
    { key: "port", label: "Port" },
    { key: "database", label: "Database" },
  ]),
  connector("sqlite", "SQLite", null, [{ key: "file_path", label: "Database file", required: true }]),
];

export const AI_POLICY = {
  ai_enabled: true,
  allowed_providers: [],
  share_metadata: true,
  share_sample_values: false,
  share_result_rows: false,
  run_query: "ask",
  allow_write: false,
  max_rows_to_model: 50,
  pii_columns: [],
  index_schemas: [],
  index_batch: 20,
  mcp_enabled: false,
  auto_approve_notes: false,
  read_allow: [],
  read_deny: [],
  write_allow: [],
  write_deny: [],
  external_access: false,
};

export const SHOP = {
  connected: true,
  id: "conn-shop",
  name: "Shop",
  config: {
    kind: "postgres",
    host: "db.local",
    port: 5432,
    database: "shop",
    file_path: null,
    auth: { method: "password", user: "app" },
    ssl_mode: "prefer",
    read_only: false,
    options: {},
    ssh: null,
  },
  color: "#818cf8",
  env: "dev",
  folder_id: null,
  has_secret: true,
  ai_policy: AI_POLICY,
  created_at: 1,
  updated_at: 1,
};

const col = (name: string, data_type: string, pk = false) => ({ name, data_type, nullable: !pk, is_primary_key: pk });
const table = (name: string, row_estimate: number) => ({ schema: "public", name, kind: "table", comment: null, row_estimate });

export const OBJECTS = [table("customers", 1200), table("orders", 50_000), { schema: "public", name: "revenue_by_day", kind: "view", comment: null }];

export const DETAILS: Record<string, unknown> = {
  customers: { object: OBJECTS[0], columns: [col("id", "integer", true), col("name", "text"), col("email", "text")], ddl: "CREATE TABLE public.customers (id integer PRIMARY KEY, name text, email text);" },
  orders: {
    object: OBJECTS[1],
    columns: [col("id", "integer", true), col("customer_id", "integer"), col("amount", "numeric(12,2)"), col("created_at", "timestamp with time zone")],
    ddl: "CREATE TABLE public.orders (id integer PRIMARY KEY, customer_id integer REFERENCES public.customers(id), amount numeric(12,2), created_at timestamptz);",
    foreign_keys: [{ columns: ["customer_id"], ref_schema: "public", ref_table: "customers", ref_columns: ["id"] }],
  },
};

export const PROVIDER = {
  has_key: true,
  id: "prov-1",
  kind: "openai",
  name: "OpenAI",
  config: { base_url: null, auth: "api_key", default_model: "gpt-test", fast_model: null, api_version: null, extra_headers: {}, max_output_tokens: null },
  created_at: 1,
  updated_at: 1,
};

/** Default backend answers; tests override any of them. */
export function backend(over: Record<string, unknown> = {}): Record<string, unknown> {
  return {
    list_connectors: CONNECTORS,
    list_connections: [SHOP],
    load_tabs: [{ id: "tab-1", title: "Query 1", sql: "", connection_id: SHOP.id }],
    get_settings: { active_tab: "tab-1", update_check: false },
    list_schemas: [{ name: "public", is_default: true }],
    list_objects: OBJECTS,
    describe_object: null,
    ai_list_providers: [PROVIDER],
    background_jobs_status: { installed: false, running: false, supported: false },
    revalidate_explorer: { refreshed: [], skipped: true, fingerprints: {}, deferred: [] },
    ...over,
  };
}

/** Load the app with the mocked backend (`data`: answers by command). */
export async function openApp(page: Page, data: Record<string, unknown> = backend()) {
  page.on("pageerror", (e) => console.error(`[page error] ${e.message}`));
  await page.addInitScript((d) => {
    (window as unknown as { __DB_MOCK_DATA__: unknown }).__DB_MOCK_DATA__ = d;
    // Answers that depend on the arguments.
    (window as unknown as { __DB_MOCK_DESCRIBE__: unknown }).__DB_MOCK_DESCRIBE__ = d.__details;
  }, { ...data, __details: DETAILS });
  await page.addInitScript({ path: MOCK });
  await page.addInitScript(() => {
    const w = window as unknown as { __DB_MOCK__: { handlers: Record<string, unknown> }; __DB_MOCK_DESCRIBE__: Record<string, unknown> };
    w.__DB_MOCK__.handlers.describe_object ??= (a: { name: string }) => w.__DB_MOCK_DESCRIBE__[a.name] ?? { __error: { kind: "invalid", message: `no ${a.name}` } };
  });
  await page.goto("/");
  await expect(page.getByRole("tablist").first()).toBeVisible();
}

/** Commands the app sent so far (name and arguments). */
export async function calls(page: Page, cmd: string): Promise<Record<string, unknown>[]> {
  return page.evaluate((c) => (window as unknown as { __DB_MOCK__: { callsOf: (c: string) => { args: Record<string, unknown> }[] } }).__DB_MOCK__.callsOf(c).map((x) => x.args), cmd);
}

/** Push a backend event to the app. */
export async function emit(page: Page, event: string, payload: unknown) {
  await page.evaluate(([e, p]) => (window as unknown as { __DB_MOCK__: { emit: (e: string, p: unknown) => void } }).__DB_MOCK__.emit(e as string, p), [event, payload] as const);
}

/** Set a backend answer from now on. */
export async function answer(page: Page, cmd: string, value: unknown) {
  await page.evaluate(([c, v]) => {
    (window as unknown as { __DB_MOCK__: { handlers: Record<string, unknown> } }).__DB_MOCK__.handlers[c as string] = v;
  }, [cmd, value] as const);
}
