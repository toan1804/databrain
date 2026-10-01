DuckDB extension binaries bundled with the app (Excel, Delta, Iceberg, Avro,
httpfs, ICU). Fetched by `node scripts/fetch-duckdb-extensions.mjs`, which runs
automatically before `tauri build` / `tauri dev`. The `v*/` folders are build
output and are not committed.
