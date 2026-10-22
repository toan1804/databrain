// Object storage on DuckDB connections: providers, URLs and listings.

import type { ConnectionView, StorageObject } from "./types";

export type StorageProvider = "s3" | "r2" | "gcs" | "s3_compatible" | "azure";
export type StorageAuth = "keys" | "chain" | "anonymous" | "connection_string" | "sas" | "service_principal";

export const STORAGE_PROVIDERS: { id: StorageProvider; label: string; scheme: string; example: string }[] = [
  { id: "s3", label: "Amazon S3", scheme: "s3", example: "s3://my-bucket/path/file.parquet" },
  { id: "r2", label: "Cloudflare R2", scheme: "r2", example: "r2://my-bucket/path/file.parquet" },
  { id: "gcs", label: "Google Cloud Storage (HMAC keys)", scheme: "gs", example: "gs://my-bucket/path/file.parquet" },
  { id: "s3_compatible", label: "S3-compatible (MinIO, Wasabi, Backblaze B2, DigitalOcean Spaces, Ceph…)", scheme: "s3", example: "s3://my-bucket/path/file.parquet" },
  { id: "azure", label: "Azure Blob Storage / ADLS Gen2", scheme: "az", example: "az://my-container/path/file.parquet" },
];

/** What the storage calls a bucket. */
export function bucketNoun(p: StorageProvider | undefined, plural = false): string {
  return p === "azure" ? (plural ? "containers" : "container") : plural ? "buckets" : "bucket";
}

/** The connection's keychain secret holds the storage secret for this sign-in. */
export function usesSecret(auth: StorageAuth): boolean {
  return auth === "keys" || auth === "connection_string" || auth === "sas" || auth === "service_principal";
}

export function providerOf(id: string | undefined | null) {
  return STORAGE_PROVIDERS.find((p) => p.id === id);
}

/** `s3://…`, `r2://…`, `gs://…`, `https://…` (not `C:\…`). */
export function isRemote(path: string): boolean {
  return /^[a-z][a-z0-9+.-]+:\/\//i.test(path.trim());
}

/** Object storage of a DuckDB connection, or null. */
export function storageOf(c: Pick<ConnectionView, "config"> | undefined | null): { provider: StorageProvider; label: string; scheme: string; root: string | null } | null {
  if (!c || c.config.kind !== "duckdb") return null;
  const p = providerOf(c.config.options?.storage);
  if (!p) return null;
  const bucket = (c.config.options?.storage_bucket ?? "").trim().replace(/^[a-z0-9]+:\/\//i, "").replace(/^\/+|\/+$/g, "");
  return { provider: p.id, label: p.label, scheme: p.scheme, root: bucket ? `${p.scheme}://${bucket}/` : null };
}

/** `s3://bucket/a/b/` → `s3://bucket/a/`; the bucket root has no parent. */
export function parentUrl(url: string): string | null {
  const m = /^([a-z][a-z0-9+.-]*:\/\/[^/]+\/)(.*)$/i.exec(url.trim());
  if (!m) return null;
  const rest = m[2].replace(/\/+$/, "");
  if (!rest) return null;
  const i = rest.lastIndexOf("/");
  return m[1] + (i < 0 ? "" : `${rest.slice(0, i)}/`);
}

/** A folder URL ends with `/` (`s3://bucket` → `s3://bucket/`). */
export function asFolder(url: string): string {
  const u = url.trim();
  return u.endsWith("/") ? u : `${u}/`;
}

export interface Listing {
  folders: { url: string; name: string; files: number; size: number }[];
  files: (StorageObject & { name: string })[];
}

/**
 * One level of a recursive listing under `prefix` (ends with `/`): its
 * sub-folders (with how many files they hold) and its own files.
 */
export function groupListing(objects: StorageObject[], prefix: string): Listing {
  const folders = new Map<string, { url: string; name: string; files: number; size: number }>();
  const files: Listing["files"] = [];
  for (const o of objects) {
    if (!o.path.startsWith(prefix)) continue;
    const rest = o.path.slice(prefix.length);
    if (!rest) continue;
    const slash = rest.indexOf("/");
    if (slash < 0) {
      files.push({ ...o, name: rest });
      continue;
    }
    const name = rest.slice(0, slash);
    const url = `${prefix}${name}/`;
    const f = folders.get(url) ?? { url, name, files: 0, size: 0 };
    f.files += 1;
    f.size += o.size ?? 0;
    folders.set(url, f);
  }
  return {
    folders: [...folders.values()].sort((a, b) => a.name.localeCompare(b.name)),
    files: files.sort((a, b) => a.name.localeCompare(b.name)),
  };
}

/** Files DuckDB reads (by extension; folders are probed by the backend). */
export function isReadable(name: string): boolean {
  return /\.(csv|tsv|txt|psv|parquet|pq|json|ndjson|jsonl|xlsx)(\.(gz|zst))?$/i.test(name);
}
