import { describe, expect, it } from "vitest";
import { asFolder, groupListing, isReadable, isRemote, parentUrl, storageOf } from "./storage";

const duck = (options: Record<string, string>) => ({ config: { kind: "duckdb", options } }) as never;

describe("object storage helpers", () => {
  it("tells URLs from local paths", () => {
    expect(["s3://b/x.parquet", "r2://b/", "gs://b/x", "https://h/x.csv"].every(isRemote)).toBe(true);
    expect(["/tmp/x.csv", "C:\\x.csv", "data/x.csv", ""].some(isRemote)).toBe(false);
  });

  it("reads a connection's storage and bucket root", () => {
    expect(storageOf(duck({}))).toBeNull();
    expect(storageOf({ config: { kind: "postgres", options: { storage: "s3" } } } as never)).toBeNull();
    expect(storageOf(duck({ storage: "s3", storage_bucket: "s3://my-bucket/raw/" }))?.root).toBe("s3://my-bucket/raw/");
    expect(storageOf(duck({ storage: "gcs", storage_bucket: "lake" }))?.root).toBe("gs://lake/");
    expect(storageOf(duck({ storage: "r2" }))?.root).toBeNull();
  });

  it("walks folders up", () => {
    expect(parentUrl("s3://b/a/c/")).toBe("s3://b/a/");
    expect(parentUrl("s3://b/a/x.csv")).toBe("s3://b/a/");
    expect(parentUrl("s3://b/a/")).toBe("s3://b/");
    expect(parentUrl("s3://b/")).toBeNull();
    expect(asFolder("s3://b")).toBe("s3://b/");
  });

  it("groups a recursive listing into one level", () => {
    const o = (path: string, size = 10) => ({ path, size, modified: null });
    const v = groupListing([o("s3://b/d/x.csv"), o("s3://b/d/p=1/a.parquet"), o("s3://b/d/p=1/b.parquet"), o("s3://b/d/p=2/c.parquet"), o("s3://b/other.csv")], "s3://b/d/");
    expect(v.files.map((f) => f.name)).toEqual(["x.csv"]);
    expect(v.folders).toEqual([
      { url: "s3://b/d/p=1/", name: "p=1", files: 2, size: 20 },
      { url: "s3://b/d/p=2/", name: "p=2", files: 1, size: 10 },
    ]);
  });

  it("knows readable file names", () => {
    expect(["a.parquet", "a.csv.gz", "a.JSONL", "b.xlsx"].every(isReadable)).toBe(true);
    expect(isReadable("_SUCCESS")).toBe(false);
  });
});

describe("Azure storage and bucket nouns", () => {
  it("knows Azure containers", async () => {
    const { bucketNoun, usesSecret } = await import("./storage");
    expect(storageOf(duck({ storage: "azure", storage_bucket: "lake/raw" }))?.root).toBe("az://lake/raw/");
    expect(bucketNoun("azure", true)).toBe("containers");
    expect(bucketNoun("s3")).toBe("bucket");
    expect(["keys", "sas", "connection_string", "service_principal"].every((a) => usesSecret(a as never))).toBe(true);
    expect(usesSecret("chain") || usesSecret("anonymous")).toBe(false);
    expect(parentUrl("az://lake/raw/")).toBe("az://lake/");
    expect(isRemote("abfss://c@acct.dfs.core.windows.net/x/")).toBe(true);
  });
});
