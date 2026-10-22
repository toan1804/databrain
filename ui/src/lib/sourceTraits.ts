// What each source type is like (quoting, name levels, row limits, query
// language, formatter…), as the backend declares it (`SourceTraits` in
// crates/connector-core/src/traits.rs). The app replaces the built-in copy
// with the backend's list at start (`setSourceTraits`); the copy keeps pure
// helpers and tests working without a backend. A Rust test keeps the copy in
// sync (`UPDATE_SOURCE_TRAITS=1 cargo test -p databrain-connector-core traits`).
import builtIn from "./sourceTraits.json";
import type { ConnectorKind } from "./types";

export type QueryLanguage = "sql" | "cql" | "cypher" | "mongo_json";

export interface SourceTraits {
  name: string;
  language: QueryLanguage;
  dialect_name: string;
  quote: "double" | "backtick" | "backtick_backslash" | "bracket";
  upper_folding: boolean;
  levels: "two" | "three" | "three_without_database";
  /** A qualified name is quoted as one (BigQuery `` `p.d.t` ``). */
  whole_path_quotes: boolean;
  /** Tables keep their catalog even inside the session's (BigQuery). */
  always_qualify_catalog: boolean;
  catalog_noun: string;
  row_limit: "limit" | "top" | "fetch_first";
  explain: string | null;
  cloud: boolean;
  local: boolean;
  network: "host_port" | "https_host" | "unknown";
  /** The connection form shows the SSL mode list. */
  ssl_setting: boolean;
  formatter: string;
  /** How rows are written into it; null = not a load target. */
  load: { max_rows: number; default_rows: number; max_bytes: number; default_bytes: number } | null;
}

const FALLBACK: SourceTraits = {
  name: "this database",
  language: "sql",
  dialect_name: "SQL",
  quote: "double",
  upper_folding: false,
  levels: "two",
  whole_path_quotes: false,
  always_qualify_catalog: false,
  catalog_noun: "database",
  row_limit: "limit",
  explain: null,
  cloud: false,
  local: false,
  network: "unknown",
  ssl_setting: false,
  formatter: "sql",
  load: null,
};

let table: Record<string, SourceTraits> = builtIn as unknown as Record<string, SourceTraits>;

/** Use the backend's traits (from `list_connectors`). */
export function setSourceTraits(infos: { kind: ConnectorKind; traits?: SourceTraits }[]): void {
  const next = { ...table };
  for (const i of infos) if (i.traits) next[i.kind] = i.traits;
  table = next;
}

/** Traits of a source type (a neutral SQL default for an unknown kind). */
export function traitsOf(kind: ConnectorKind | string | undefined | null): SourceTraits {
  return (kind && table[kind]) || FALLBACK;
}

/** Kinds whose schema ids may carry a catalog (`catalog.schema`). */
export function kindMayHaveCatalog(kind: ConnectorKind | string): boolean {
  return traitsOf(kind).levels !== "two";
}

/** Kinds that always show catalogs first. */
export function kindIsThreeLevel(kind: ConnectorKind | string): boolean {
  return traitsOf(kind).levels === "three";
}

/** Short engine name for messages and tooltips (`SQL Server`). */
export function engineName(kind: ConnectorKind | string | undefined | null): string {
  return (kind && table[kind]?.name) || kind || FALLBACK.name;
}
