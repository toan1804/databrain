# Design: one query over several connections

Status: proposal (2026-11-25). Nothing is implemented. Open questions are in §12.

## 1. Problem

DataBrain can already join data from different databases: run a query on each
connection, name the results, then query `results.<name>` on the "Results
(DuckDB)" connection (`OutputTables`, see [QUERY_ENGINE.md](../QUERY_ENGINE.md),
"Outputs"). It works, but it is three tabs and three runs: the join isn't one
thing that can be saved, rerun, scheduled or handed to `dbr`.

**Goal:** one statement, in one editor tab, that reads from several connections
and joins the rows locally:

```sql
WITH
  orders    AS (@"Sales PG" SELECT id, customer_id, total FROM orders WHERE day >= :since),
  customers AS (@crm_mysql  SELECT id, name, country FROM customers)
SELECT c.country, sum(o.total) AS revenue
FROM orders o JOIN customers c ON c.id = o.customer_id
GROUP BY 1
```

**Non-goals (v1):**

- Pushing filters or joins of the outer query down to the servers (§7).
- Writes: no part and no outer statement may change anything (§6). Loading the
  joined rows somewhere stays "Load into…" on the result.
- Scanning a remote database from DuckDB directly (its `postgres` / `mysql`
  extensions): that would hand DuckDB the credentials and skip the
  connection's rules (read-only, production, the AI policy, SSH tunnels).

## 2. The statement

A **combined query** is one DuckDB `SELECT` whose `WITH` list has at least one
**part**: a CTE whose body starts with `@connection`.

```
combined  := WITH cte ("," cte)* select          -- the outer query: DuckDB SQL
cte       := name [ "(" columns ")" ] AS "(" body ")"
body      := "@" connection part-sql             -- a part: runs on that connection
           | duckdb-select                       -- an ordinary CTE: stays in the outer query
connection := identifier | "quoted name"         -- a saved connection's name (any case) or id
```

- **Why `@`:** no dialect DataBrain speaks accepts `@name` as the first token of
  a statement (MySQL and SQL Server variables, Snowflake stages and Oracle
  links all need something before it). If a combined query ever reached a
  server unsplit, it fails to parse; it can never run as something else. A
  keyword form (`FROM CONNECTION a (…)`) reads well but looks like SQL a server
  might one day accept, and needs a second keyword to end the name.
- **The connection** is found as `dbr -c` finds it (`find_connection`): the
  exact name in any case, or the id; a typo is refused with the close names,
  never matched to another connection. Names with spaces or punctuation are
  quoted with `"`.
- **The part's SQL** runs from after the connection to the CTE's closing
  parenthesis, and is one statement in that connection's own dialect. The
  closing parenthesis is found with that connection's lexer (`split_with` and
  its `SourceTraits` already know each dialect's strings, comments and dollar
  quotes), so a `)` in a string or a MongoDB / Cypher body doesn't end it.
- **Column lists** on a part (`orders(id, cust)`) rename the part's columns,
  as in a CTE.
- **Parameters** (`:name`) are one set for the whole statement: the same value
  wherever the name is used, in parts and in the outer query.
- **Where it runs:** on any tab. The tab's own connection plays no part; a tab
  with no connection works. (§12, Q1: whether a bare `@` may mean the tab's
  connection.)
- A part may not read another part: parts are independent (that is what lets
  them run together). The outer query reads them by name.
- Statements around it (a script with several statements) behave as today: a
  combined query is one statement of the script.

Recognised in `connector-core/src/sql` next to the splitter (a new
`combined.rs`): `parse_combined(text) -> Option<Combined { parts: Vec<Part {
name, connection, sql, span }>, outer_spans }>`. Not a combined query: `None`,
and nothing changes for the statement. It runs for every statement at run time
and per keystroke for highlighting, so it gets a `crates/bench` path.

## 3. How it runs

`QueryEngine::run` (`run.rs`), for a statement that is a combined query:

1. **Resolve and check every part before anything runs** (§6): the connection
   exists, the part is one statement, and it is a read there. One refusal
   refuses the whole statement, saying which part.
2. **Run the parts**, at most four at a time, each as an ordinary run on its
   connection: its own session (a new `SessionRole::Part { tab, name }`, closed
   when idle like any tab's), `origin` as the statement's, the statement's
   parameters, `base_offset` set to the part's place in the editor so an
   error's position lands on the right characters.
3. Each part's rows become an **output** (`OutputRegistry::add`), tied to the
   tab like any result, with the part's name shown as its label. They are not
   named outputs: `results.orders` of the user is never taken or replaced.
4. **Run the outer query** on the Results (DuckDB) connection with each part's
   CTE body replaced by a read of its output's handle:
   `orders AS (SELECT * FROM results.r41)`. Outputs already reach DuckDB with
   their types (`sql_ready_all`: decimals as `DECIMAL`), from memory or from
   their Parquet files when large.
5. The tab shows the outer query's result first, then one result tab per part
   (rows, time, the connection it came from).

- **Stop** stops every part and the outer query. A part that fails stops the
  others; the error names the part and its connection.
- **Sessions:** a part never runs in the tab's manual-commit transaction (it has
  its own session); a combined query is refused while the tab has a transaction
  open only if the tab's connection is one of its parts (Q2).
- **Cost preview** (BigQuery, Snowflake, Databricks): `estimate_cost` per part,
  shown per part and summed.
- **History:** one entry for the combined statement (on Results), plus one per
  part on its connection, linked by a group id, so a connection's history
  still shows everything that ran on it.

## 4. Size: no silent truncation

A join over a truncated part is a wrong answer that looks right. So:

- Parts are read **whole**. The tab's row limit applies to the outer query's
  result only.
- A per-part cap (setting `combined_part_rows`, default 5,000,000 rows) **fails**
  the statement when a part passes it ("part `orders` has more than 5,000,000
  rows: filter it in the part, or raise the limit"), instead of cutting it.
- Large parts go to disk as results already do (the outputs budget); the outer
  query reads their Parquet files.
- Progress shows each part's rows as they arrive, so a part that is reading far
  more than expected is seen while it runs.

## 5. Running it again

Fetching every part again to change one line of the outer query is slow and,
on a warehouse, costs money. A tab keeps each part's last output with what
produced it (connection, SQL, parameter values).

- **Run** (⌘↵) reads everything again: the answer is as fresh as a normal run.
- **Run the join only** (a second action) reuses parts whose connection, SQL
  and parameters are unchanged, and says how old each reused part is. Parts
  that changed are read again.
- **Run this part** (cursor inside a part) runs just that part, as a normal
  query on its connection: for writing and checking one part at a time.

## 6. Safety

A combined query is refused unless everything in it is a read. Each rule needs
tests for the allowed and the refused case (AGENTS.md).

| Check | Rule |
|---|---|
| A part | One statement, classified `Read` on its own connection's dialect. `Dml`, `Ddl`, `Session`, `Transaction`, `Other` and `Unknown` are refused (not asked: a block or call whose effect can't be told doesn't belong in a join). |
| The outer query | One statement, `Read` in DuckDB. No `COPY`, `ATTACH`, `INSTALL`, `CREATE`. |
| Read-only and production connections | Reads are allowed on both, as for a normal run; nothing else can be in a part. |
| The AI (`origin` Ai / Mcp) | Each part passes that connection's AI policy as a run on it would: AI enabled, the table rules, approval where the policy asks (one approval listing every part and its connection), the row cap. The outer query runs in the DuckDB sandbox with `output_masks` for each part's PII columns, the mechanism that already protects `results.<name>`. A part on a connection the AI may not use refuses the whole statement. |
| Credentials | Each part connects as its connection does today; nothing about one connection (host, user, secret) reaches another server or DuckDB. |
| Where rows go | Rows of connection A never go to connection B: parts are independent reads, and the join happens in the local DuckDB. |

`docs/CHANGE_IMPACT.md` gets a row: classification, the AI policy, PII masking
and the outputs budget all gain a caller.

## 7. What it costs: no pushdown

Each part runs as written. A filter in the outer query (`WHERE o.day = …`) is
applied after the part's rows are here; it never reaches the server. The
statement makes this visible rather than hiding it:

- the part's SQL is what the server runs, exactly;
- a part without `WHERE` or `LIMIT` on a table the explorer knows to be large
  gets an editor hint ("reads the whole table: about 40 M rows");
- the per-part cap (§4) stops a mistake.

Pushdown of simple outer filters into parts is possible later (phase 4), but
it means rewriting the user's SQL for another dialect, with every risk that
brings. Not before the plain form has been used for a while.

## 8. The editor

- **Highlighting and folding:** each part's body in its connection's dialect,
  the rest as DuckDB (CodeMirror mixed-language parsing over the spans
  `parse_combined` gives).
- **Completion:** after `@`, the connection names. Inside a part, that
  connection's tables and columns (`sqlAssist.ts` with the part's connection
  instead of the tab's). In the outer query, the part names and their columns:
  from the part's last output when there is one, else from a `LIMIT 0`
  describe of the part on demand.
- **Errors:** positions are exact (each run's `base_offset`); the message says
  which part.
- **Connection chips** above the editor list the connections the statement
  uses, with their environment badges: a production connection is visible
  before Run.
- **Format SQL:** each part with its dialect, the outer query as DuckDB.
- **Lineage tab:** phase 3: table and column lineage through the parts.

## 9. Other entry points

| Where | Phase | Notes |
|---|---|---|
| Query tabs | 1 | The feature. |
| `dbr -i "<combined query>"` | 1 | No `-c` needed; `-c` with a combined query is refused. Printing, exit codes and the production question as today. Without `-i`, the app opens it in a tab. |
| Notebook SQL cells | 2 | A cell may be a combined query whatever the notebook's connection. |
| Jobs | 2 | A job can already read several connections in steps and join them in a Results step; a combined query as one query step saves the wiring. The step's parts count as reads on each connection for the Usage tab (`table_refs`). |
| The AI assistant and MCP | 3 | `run_query` accepts it under §6. The prompt says when to use it (a join across the user's connections) and that parts are read whole. |
| Saved queries, history search, parameters | 1 | Text like any other. |

## 10. What changes where

| Area | Change |
|---|---|
| `connector-core/src/sql/combined.rs` (new) | `parse_combined`, the rewrite of the outer query; unit tests per dialect (strings, comments, nested parentheses, dollar quotes, MongoDB and Cypher bodies) |
| `query-engine/src/run.rs` | A combined statement: check, run parts, run the outer query; cancellation; the reuse of §5 |
| `query-engine` events | Per-part progress and results (`JobEvent` gains the part's name) |
| `workspace` `SessionRole` | `Part { tab, name }` |
| `crates/ai` | Policy per part, masks for the outer query, the approval text |
| `crates/app/src/cli/query.rs` | `dbr` without `-c` |
| `ui` | Mixed-dialect editor, completion, part result tabs, connection chips, "Run the join only", "Run this part" |
| Docs | FEATURES, QUERY_ENGINE (life of a combined run), CHANGE_IMPACT |

No new dependency: the splitter, the outputs, the DuckDB session and the AI
masks exist. App size: unchanged apart from the code.

## 11. Phases

1. **Run it:** `parse_combined`, §3, §4, §6 for the user's own runs, part result
   tabs, exact error positions, `dbr`. The editor highlights parts as plain SQL.
2. **Write it comfortably:** mixed-dialect highlighting and completion, §5,
   connection chips, hints for unbounded parts, notebooks, jobs.
3. **Around it:** the AI and MCP, lineage across parts, history grouping in the
   UI.
4. **Maybe:** pushdown of simple filters; `@conn.schema.table` as a shorthand
   for a whole-table part (Q4).

Each phase is its own PR or several, reviewed as usual.

## 12. Open questions

1. **A bare `@`** for the tab's own connection (`AS (@ SELECT …)`)? Handy in a
   tab that already has a connection; but the statement then means something
   else when pasted into another tab. Proposed: no, always name it.
2. **A part on the tab's connection while the tab has a transaction open:** run
   the part in its own session (it won't see uncommitted changes) or refuse?
   Proposed: refuse, saying why.
3. **Renamed connections** break saved combined queries (the name is in the
   text). Accept `@id:<connection id>` too, and have Rename offer to update
   saved queries? Proposed: support the id form in phase 1, the offer later.
4. **Shorthand for a whole table** (`FROM @sales.public.orders`): tempting, and
   an easy way to read 40 M rows by accident. Proposed: not before pushdown.
5. **The default per-part cap** (5,000,000 rows) and whether it is per part or
   in bytes like the outputs budget.
6. **Non-SQL connections** (MongoDB, Neo4j) as parts: their bodies aren't SQL,
   but they return rows. Proposed: allowed from phase 1 if `parse_combined` can
   find the body's end with their lexer; otherwise phase 2.

## 13. Alternatives considered

- **Keep it as tabs plus `results.<name>`.** Works today; not one runnable,
  saveable, schedulable thing. The combined query is that workflow written
  down.
- **`-- @connection` comments above each CTE.** Survives being sent to a
  server by mistake too well: it would run, on the wrong connection, as an
  ordinary query.
- **DuckDB scanners (`ATTACH … (TYPE postgres)`).** Real pushdown for a few
  databases, none for the rest, and the connection's rules and credentials
  move out of DataBrain's hands (§1, non-goals).
- **A federation engine (Trino-style planner).** Far beyond the app's size
  budget and scope.
