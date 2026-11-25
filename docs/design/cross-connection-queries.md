# Design: one query over several connections

Status: phase 1 in progress (2026-11-25): the parser is in (`connector-core/src/sql/combined.rs`); the run, `dbr` and the result tabs follow. The decisions are in §12; nothing is open.

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

- Pushdown in the first version: phase 1 runs each part as written. Phase 2
  pushes the outer query's filters and column list into the parts (§7); phase 1
  is built so that it only has to fill in a step that is already there.
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
  closing parenthesis is found with that connection's lexer (`skip_token`, the
  splitter's: each dialect's strings, comments and dollar quotes), read from
  after the connection's name, so a `)` in a string or comment doesn't end it.
- **Column lists** on a part (`orders(id, cust)`) rename the part's columns,
  as in a CTE.
- **Parameters** (`:name`) are one set for the whole statement: the same value
  wherever the name is used, in parts and in the outer query.
- **Where it runs:** on any tab. The tab's own connection plays no part; a tab
  with no connection works. Every part names its connection: there is no bare
  `@` for "this tab's connection" (the statement would mean something else
  pasted into another tab).
- **By id too:** `@id:<connection id>` names a connection that keeps working
  after a rename (the name is in the text of saved queries).
- **SQL connections only in phase 1.** A part on MongoDB or Neo4j is refused
  with a message saying so (phase 3: their bodies need their own lexers to find
  the part's end, their own read check, and rules for nested documents as
  columns).
- A part may not read another part: parts are independent (that is what lets
  them run together). The outer query reads them by name.
- Statements around it (a script with several statements) behave as today: a
  combined query is one statement of the script.

Recognised in `connector-core/src/sql` next to the splitter (a new
`combined.rs`): `parse_combined(text, resolve) -> Result<Option<Combined {
parts: Vec<CombinedPart { name, connection, kind, sql, sql_span, body_span }> }>>`,
where `resolve` gives each named connection's kind (its lexer finds the part's
end) or why there is none. Not a combined query: `Ok(None)`, and nothing
changes for the statement; once a part is found, what doesn't read as one is
an error naming the part. `looks_combined` is the cheap test before it
(`WITH` first and an `@` in the text). It runs for every statement at run time
and per keystroke for highlighting, so it gets a `crates/bench` path.

## 3. How it runs

`QueryEngine::run` (`run.rs`), for a statement that is a combined query, works
from a **plan**, never from the text directly:

```rust
struct CombinedPlan { parts: Vec<PartPlan>, outer_sql: String }
struct PartPlan {
    name: String,
    connection_id: String,
    user_sql: String,      // the part as written
    sent_sql: String,      // what the server is asked: user_sql in phase 1
    pushed: Vec<Pushed>,   // what phase 2 added, for the UI and the history
    span: Range<usize>,    // the part in the editor text (error positions)
}
```

`plan_combined(parsed, connections) -> CombinedPlan` is the one place that
decides what each server is asked. In phase 1 it copies `user_sql` to
`sent_sql`. Everything after it (the checks, the runs, the outputs, the reuse of
§5, the events, the history) reads `sent_sql`, so phase 2 changes that function
and nothing downstream.

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
  open only if the tab's connection is one of its parts (§12, 5).
- **Cost preview** (BigQuery, Snowflake, Databricks): `estimate_cost` per part,
  shown per part and summed.
- **History:** one entry for the combined statement (on Results), plus one per
  part on its connection, linked by a group id, so a connection's history
  still shows everything that ran on it.

## 4. Size: no silent truncation

A join over a truncated part is a wrong answer that looks right. So:

- Parts are read **whole**. The tab's row limit applies to the outer query's
  result only.
- A per-part cap (setting `combined_part_rows`, default 1,000,000 rows) **fails**
  the statement when a part passes it, instead of cutting it. The user always
  knows: the error names the part and the cap ("part `orders` has more than
  1,000,000 rows: filter it in the part, or run again without the limit") and
  has a "Run again without the limit" action (for that tab; the setting changes
  the default). `dbr` takes `--part-rows N` (0: no limit); a job step has the
  same as a setting of the step.
- The cap is in rows, not bytes: rows are what the user can act on (a `WHERE`,
  a `LIMIT`). Memory is already protected: large parts go to disk as results
  do (the outputs budget), and the outer query reads their Parquet files.
- Disk is checked apart: a part stops, saying so, when the disk holding the
  spill folder is nearly full (`sysmem::disk_space`), whatever its row count.
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

## 7. Pushdown (phase 2; design only)

Phase 1 runs each part as written: a filter in the outer query is applied after
the part's rows are here. Phase 2 lets the planner (§3, `plan_combined`) ask
each server for less. This section is the design; none of it is in phase 1
except the seams listed at its end.

### 7.1 The rule: pushdown may only remove work, never change the answer

Two choices make that hold by construction rather than by care:

1. **Wrap, never rewrite.** The user's part is not edited. What is pushed goes
   around it, in the part's own dialect:

   ```sql
   -- the part as written                     -- what is sent
   SELECT id, customer_id, total, day          SELECT "id", "customer_id", "total"
   FROM orders WHERE day >= :since             FROM (
                                                 SELECT id, customer_id, total, day
                                                 FROM orders WHERE day >= :since
                                               ) AS p
                                               WHERE "customer_id" = 42
   ```

   A filter on the part's result means the same inside the wrapper as it did in
   the outer query, whatever the part contains (its own `LIMIT`, `ORDER BY`,
   `GROUP BY`, window functions): the wrapper filters the part's result, as the
   outer query did. The server's optimizer moves the filter inward where that
   is valid.
2. **The outer query keeps every predicate.** DuckDB applies the full `WHERE`
   and `ON` again to what arrives. A pushed filter is only a pre-filter, so it
   never has to mean exactly what DuckDB means; it has to be **no stricter**
   (never drop a row DuckDB would keep). A server that keeps more rows (a
   case-insensitive collation, blank-padded `CHAR`) costs a little transfer and
   nothing else.

So with pushdown on or off the result is the same rows. That is also the test:
every combined query in the test suite runs both ways and the results must be
equal (unit tests on the planner; live tests per database).

### 7.2 What is pushed

| What | Pushed when | Not pushed |
|---|---|---|
| **Columns** | The outer query names the columns it uses of a part: the wrapper selects only those | The outer query uses `part.*` or `SELECT *` over it |
| **Filters** | A top-level `AND` term of the outer `WHERE` (or of an inner join's `ON`) that reads one part's columns only and compares them with literals or `:name` values: `=`, `<>`, `<`, `<=`, `>`, `>=`, `IN (…)`, `BETWEEN`, `IS [NOT] NULL`, and `OR` / `NOT` of such terms | Terms reading two parts (join conditions), expressions and functions on the column, `LIKE` and regular expressions, subqueries, terms under an outer join's nullable side |
| **Row limit** | The outer query is `SELECT … FROM one_part [WHERE pushed terms only] LIMIT n` with no join, `ORDER BY`, `GROUP BY`, `DISTINCT` or window | Anything else |

Types decide which comparisons are safe as "no stricter":

| Column type (as the part's output reports it) | Pushed |
|---|---|
| Integers, decimals, booleans | every comparison above |
| Dates, timestamps without time zone | every comparison, the literal written in the dialect's typed form |
| Text | `=`, `IN`, `IS [NOT] NULL`. Not ranges (`<`, `BETWEEN`: the server's collation orders text differently from DuckDB, and could drop rows DuckDB keeps), not `<>` (below) |
| Floats | ranges only (`=` on floats differs by engine) |
| Timestamps with time zone, binary, JSON, arrays, everything else | nothing (phase 2) |

`<>` and `NOT` need the opposite guarantee (the server's `=` must be no
*looser* than DuckDB's), which a case-insensitive collation breaks: they are
pushed for numbers, booleans and dates only.

Not pushed, by decision:

- **Join keys from one part into another** ("fetch the 200 customers, then ask
  the other server for `customer_id IN (…200 values…)`"). It is the biggest
  saving for a small-table-to-big-table join, and it sends one connection's
  data to another server, which §6 forbids. If it is ever added it is its own
  proposal, opt-in per statement, never for the AI.
- **Aggregates and joins.** A `GROUP BY` or a join of two parts on the *same*
  connection could run there; writing that SQL for another dialect is the
  rewrite this design avoids. The user can already do it: make it one part.

### 7.3 When a part can't be wrapped

The planner pushes nothing into a part (it runs as written, as in phase 1) when:

- the part's output columns aren't known (no earlier result and no describe):
  it asks the connection to describe the part (`LIMIT 0` / the driver's
  describe) once per text, and without an answer pushes nothing;
- the dialect can't take the part as a derived table: a per-dialect rule in
  `SourceTraits` (e.g. SQL Server refuses a derived table with a top-level
  `ORDER BY` without `TOP`, or one starting with `WITH`; MongoDB and Cypher
  parts are never wrapped);
- the part is not a single plain query the dialect's parser recognises
  (`sqlparser` with that dialect): when unsure, don't wrap;
- two of its output columns have one name.

### 7.4 Safety

- The **sent** SQL is what is checked: classified as a read on its connection
  again after wrapping (§6), and, for the AI, passed through that connection's
  policy again. Wrapping adds no table, so the table rules see the same tables.
- **Nothing the user typed is spliced into another dialect as text.** Column
  names are the part's output names quoted with that dialect's `quote_ident`;
  literals are rebuilt from their parsed values with that dialect's
  `quote_string` or bound as parameters; `:name` values stay parameters.
- **PII:** for the AI, a masked column can't be named in the outer query (the
  DuckDB sandbox's `output_masks`), so no filter on it exists to push. For the
  user's own runs nothing is masked and nothing changes.
- A pushed filter never widens what a connection is asked: it only adds `WHERE`
  terms and removes columns.

### 7.5 What the user sees

- Each part's result tab shows **the SQL that was sent** when it differs from
  what was written, with the pushed terms marked; the history records the sent
  SQL. The editor marks outer-query terms that were pushed (a subtle underline
  with a tooltip: "also applied on Sales PG").
- **Pushdown: on / off** per tab (a toggle next to Run; on by default once
  phase 2 ships) and a setting for the default. Off is phase 1's behaviour,
  for when a server plans the wrapped query badly.
- The cap of §4 counts the rows that arrive, so pushdown makes it rarer.
- **Reuse (§5) keys on the sent SQL.** Changing an outer filter that is pushed
  changes what the part is asked, so "Run the join only" reads that part again
  (and says so); changing a term that isn't pushed reuses it.
- Until phase 2, and wherever a part isn't wrapped: the part's SQL is what the
  server runs, exactly, and a part without `WHERE` or `LIMIT` on a table the
  explorer knows to be large gets an editor hint ("reads the whole table: about
  40 M rows").

### 7.6 What phase 1 builds so that phase 2 only adds

| Seam | In phase 1 | Phase 2 adds |
|---|---|---|
| `plan_combined` and `PartPlan { user_sql, sent_sql, pushed }` (§3) | `sent_sql = user_sql`, `pushed` empty | the planner of §7.2 and §7.3 |
| Checks, runs, outputs, events and history read `sent_sql` | the same text as written | nothing |
| The reuse key of §5 is (connection, `sent_sql`, parameters) | equals the written part | nothing |
| The outer query is rewritten by replacing each part's body span (`Combined::outer_sql`), so its text and error positions stay the user's; it is classified (parsed as DuckDB) before anything runs | used for the rewrite of §3 | the rewritten text parsed (`sqlparser`, DuckDB dialect) for column use and `WHERE` / `ON` terms |
| Part outputs keep their columns' names and database types | shown in the result tab | the type table of §7.2 |
| Error positions map through `PartPlan.span` | 1:1 | positions inside the wrapper map back to the written part; errors in generated text name the part |
| `SourceTraits` (per-dialect facts) | no new field | `derived_table` rules, typed literal forms |

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
- **Lineage tab:** phase 4: table and column lineage through the parts.

## 9. Other entry points

| Where | Phase | Notes |
|---|---|---|
| Query tabs | 1 | The feature. |
| `dbr -i "<combined query>"` | 1 | No `-c` needed; `-c` with a combined query is refused. Printing, exit codes and the production question as today. Without `-i`, the app opens it in a tab. |
| Notebook SQL cells | 3 | A cell may be a combined query whatever the notebook's connection. |
| Jobs | 3 | A job can already read several connections in steps and join them in a Results step; a combined query as one query step saves the wiring. The step's parts count as reads on each connection for the Usage tab (`table_refs`). |
| The AI assistant and MCP | 4 | `run_query` accepts it under §6. The prompt says when to use it (a join across the user's connections) and that parts are read whole. |
| Saved queries, history search, parameters | 1 | Text like any other. |

## 10. What changes where

| Area | Change |
|---|---|
| `connector-core/src/sql/combined.rs` (new) | `parse_combined`, the rewrite of the outer query; unit tests per dialect (strings, comments, nested parentheses, dollar quotes; MongoDB and Cypher parts refused) |
| `query-engine/src/combined/` (new: `plan.rs`, `run.rs`) | `plan_combined` and `CombinedPlan` (§3); check, run parts, run the outer query; cancellation; the reuse of §5. Phase 2: the planner of §7 in `plan.rs` |
| `query-engine` events | Per-part progress and results (`JobEvent` gains the part's name) |
| `workspace` `SessionRole` | `Part { tab, name }` |
| `crates/ai` | Policy per part, masks for the outer query, the approval text |
| `crates/app/src/cli/query.rs` | `dbr` without `-c` |
| `ui` | Mixed-dialect editor, completion, part result tabs, connection chips, "Run the join only", "Run this part" |
| Docs | FEATURES, QUERY_ENGINE (life of a combined run), CHANGE_IMPACT |

No new dependency: the splitter, the outputs, the DuckDB session and the AI
masks exist. App size: unchanged apart from the code.

## 11. Phases

1. **Run it:** `parse_combined`, the plan and its seams (§3, §7.6), §4, §6 for
   the user's own runs, part result tabs, exact error positions, `dbr`. The
   editor highlights parts as plain SQL. Parts run as written.
2. **Pushdown** (§7): columns, filters and the single-part limit, the sent SQL
   shown, the on / off toggle, results equal with it on and off.
3. **Write it comfortably:** mixed-dialect highlighting and completion, §5,
   connection chips, hints for unbounded parts, notebooks, jobs, MongoDB and
   Neo4j parts.
4. **Around it:** the AI and MCP, lineage across parts, history grouping in the
   UI.
5. **Maybe:** `@conn.schema.table` as a shorthand for a whole-table part (§12,
   6), now that a filter on it would be pushed.

Each phase is its own PR or several, reviewed as usual.

## 12. Decisions and open questions

Decided (2026-11-25):

1. **No bare `@`:** a part always names its connection.
2. **Renamed connections:** `@id:<connection id>` is accepted from phase 1;
   Rename offering to update saved queries comes later.
3. **The cap is never silent** (§4): a part past it fails the statement with a
   message and a way to run without it.
4. **MongoDB and Neo4j parts:** phase 3, refused with a clear message before.

5. **A part on the tab's connection while the tab has a transaction open** is
   refused, saying why (it would run in its own session and not see the
   uncommitted changes).
6. **No whole-table shorthand** (`FROM @sales.public.orders`) before pushdown:
   it is an easy way to read 40 M rows by accident.
7. **The cap is in rows, 1,000,000 by default** (§4): it catches the unfiltered
   part, and going past it is one action; a free-disk check covers bytes.

8. **Pushdown is phase 2** (§7), designed now so phase 1's code has its place
   ready (§7.6); it wraps parts and never rewrites them, keeps every predicate
   in the outer query, and never sends one connection's values to another.

Open: none.

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
