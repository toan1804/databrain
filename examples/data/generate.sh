#!/bin/sh
# Regenerate the sample dataset: CSV + SQLite (Python stdlib) and Parquet (bundled DuckDB).
set -e
here=$(cd "$(dirname "$0")" && pwd)
python3 "$here/generate.py"
cargo run -q -p databrain-connector-duckdb --example sample_parquet -- "$here"
rm -rf "$here/_staging"
