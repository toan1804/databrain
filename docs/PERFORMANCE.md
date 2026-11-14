# Performance

`crates/bench` times DataBrain's critical paths: what runs on every keystroke, before every statement, for every result page, export and history entry. It is a plain binary with its own small harness (no benchmark dependency) and is not part of the app.

```sh
cargo run --release -p databrain-bench                  # every path, compared with the baseline
cargo run --release -p databrain-bench -- redact sql/   # only paths whose name contains one of these
cargo run --release -p databrain-bench -- --list        # the paths and what each one stands for
cargo run --release -p databrain-bench -- --check       # exit 1 when a path is over its budget (CI)
cargo run --release -p databrain-bench -- --max-slowdown 1.3   # exit 1 when a path is 30% slower than the baseline
cargo run --release -p databrain-bench -- --save        # write crates/bench/baseline.json
```

Always `--release`: a debug build is 10 to 100 times slower and says nothing about the app. `--quick` samples each path for 0.1 s instead of 0.5 s.

## Reading the output

```text
path                                     time                 rate     budget  baseline
redact/script_1000_statements         6.96 ms            12.1 MB/s  100.00 ms     1.00x
results/sort_text_200k               13.71 ms        14.6 M rows/s  300.00 ms     1.00x
```

- **time**: the median of one iteration (a warm-up, then at least 5 samples; fast paths run many times per sample).
- **rate**: what that is per second, where the path handles an amount (rows, statements, MB of text).
- **budget**: the most the path may take on any machine. It is 15 to 20 times the time on the laptop it was written on, so a slow CI runner passes and a path that became quadratic, or parses a statement many times, does not. `--check` fails on a path over its budget; CI runs it on every push (the `bench` job).
- **baseline**: this run against `crates/bench/baseline.json`, shown only when the file is from the same machine (OS, CPU architecture and core count). `1.10x` is 10% slower. Two runs of the same build differ by about 3%; take 10% or more as real, and run again before believing one number.

## The paths

| Group | What it times | Code |
| --- | --- | --- |
| `sql` | Splitting a script, classifying a statement (per dialect, and text the parser can't read), planning and parsing a 100-statement script, the read-only rule, the function check, table references, parameters | `crates/connector-core/src/sql`, `tables.rs` |
| `redact` | What is kept of a statement, a long script, text with URLs and `name=value` pairs, and three 100 KB texts built to make a rule read to the end each time | `crates/connector-core/src/sql/redact.rs` |
| `ai` | The policy review of the assistant's SQL (a query, on a read-only connection, a 100-statement script) | `crates/ai/src/policy.rs` |
| `lineage` | Lineage of a query and of a script | `crates/lineage` |
| `results` | The grid over 200,000 rows in memory: a page, sorts, a column filter, the quick filter, Find, column statistics | `crates/result-store` |
| `export`, `load` | 100,000 rows to CSV, JSON, SQL, Markdown, Parquet and Excel; a load's INSERT statements | `crates/export` |
| `workspace` | Saving and searching history (20,000 entries), writing and reading the explorer cache (5,000 objects) | `crates/workspace` |
| `engine` | A run end to end on a SQLite file: `select 1` (what DataBrain adds to a statement), 100,000 rows into the result store, a 200-statement script | `crates/query-engine` |

The numbers of the last release are in `crates/bench/baseline.json` (the machine is named in the file).

## What the first run showed (0.4.1, Apple M-series, 8 cores)

- Every per-statement check costs one parse, 55 to 65 µs for a 20-line query: classification, the function check, table references, the AI review (which does all of them from one parse) and redaction. Saving to history (76 µs) is mostly redaction (66 µs).
- Redaction reads text at 5 to 12 MB/s (six readings of each text); the texts built to be slow stay linear.
- The grid: a page is 45 µs; sorting 200,000 rows takes 3 ms on numbers and 14 ms on text; the quick filter and Find read about 10 M rows/s; column statistics are the slowest view (55 ms, 3.7 M rows/s).
- Export: Parquet 5.4 M rows/s, CSV 2.8 M, JSON, Markdown and SQL about 1.2 M, Excel 0.3 M rows/s: the slowest by far.
- A run adds about 0.14 ms to a statement (0.09 ms each in a script); 100,000 rows reach the result store in 28 ms.

## When to run it

- **Changing a path in the table**: run its group before and after (`-- sql/`), and say in the commit what changed when it is more than noise.
- **Before a release** ([RELEASING.md](RELEASING.md)): the whole suite on the machine the baseline is from, with `--max-slowdown 1.3`. Explain or fix what is slower, then `--save` and commit the baseline with the release.
- **Adding a critical path** (something that runs per keystroke, per statement, per row or per page): add it in `crates/bench/src/paths.rs` with a sentence on what the user is doing, a budget of about 15 to 20 times its time, and a row here.

## Limits

- Databases are not timed, only SQLite on a local file: a driver's network time would hide DataBrain's own.
- The UI is not timed (rendering the grid, the editor).
- The baseline compares runs on one machine. On another, only the budgets hold.
