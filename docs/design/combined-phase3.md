# Design: combined queries, phase 3 — writing them comfortably

Status: approved (2026-11-27): revision 2 after a review and a debate, with
the final review's points folded in. Parent: [cross-connection-queries.md](cross-connection-queries.md)
§5, §8, §9, §11. Phases 1 and 2 are built, with their leftovers (errors in
place, pushdown that asks less, the per-tab Pushdown box, the cost preview
per part); what stays open is in docs/TODO.md.

Phase 3 makes a combined query pleasant to write and usable where SQL is
written elsewhere. Nothing here widens what a run may do: every part is one
read on its connection, the outer query a read in DuckDB (parent §6).

## 0. First: what the assistant sees of a combined statement (a leak today)

Ghost text sends the text around the cursor to the AI after checking only
the tab's connection (`ghostExt.ts:120-132`, `ai/src/ghost.rs:90`). In a
combined statement that text carries parts on connections where the AI is
off (their SQL and connection names).

- Every feature that sends editor text to the AI: ghost text, inline AI edit
  (`inlineAi.ts`), Ask AI and the chat's "current editor SQL" context, Fix
  error, explain and plan. Inside a combined statement: the window
  is clipped to the part body the cursor is in and checked against that
  part's connection (AI enabled, provider allowed); in the outer query, the
  statement's parts must all be on such connections, else nothing is sent
  ("ghost text is off here: a part's connection doesn't allow the AI").
- Every combined statement the text sent overlaps must pass, not only the
  one at the cursor (ghost text's window, Ask AI's context may hold several).
- The backend checks it too: `ai/src/ghost.rs` (and the other entry points)
  resolve the parts in the text they receive and refuse when any named
  connection doesn't allow the AI or the provider; the UI's clipping is not
  the only gate.
- Tests both ways (AGENTS.md): allowed (all parts AI-enabled), refused (one
  part's connection off; one with another provider; a second combined
  statement in the window), in the UI and in the backend.

## 1. The parts, from the backend

The UI doesn't parse parts itself (a second reader would drift from
`parse_combined`, and Format and Run this part act on the spans):

- `combined_parts(text, kind)` → per combined statement its span and parts
  `{ name, body, sql, connection: { id, name, kind, env, read_only, color }
  | unknown(ref) }` (UTF-16), from `parse_combined` with the workspace
  resolver (`find_connection` exactly). Called debounced (200 ms) only when
  the text has `WITH` and `@`, like `sql_parameters`; between answers the
  spans move with the edits (`ChangeSet.mapPos`). Actions that act on spans
  (Format, Run this part) ask once more at the moment they run.
- The editor's statement at the cursor already reads parts in their dialects
  (`split_for_editor`).

## 2. What the editor shows

- **Parts marked:** each part body gets a faint tint and a gutter mark in its
  connection's colour; after `@name` an inline badge with the environment
  (and read-only). An unknown connection is a red diagnostic on `@crm`
  ("no connection named crm; did you mean …", the run's message).
- **A summary row** under the toolbar for a combined statement at the cursor:
  one compact chip per connection (production in danger colours, read-only
  marked); clicking one selects that part.
- **Per-dialect highlighting** (CodeMirror `parseMixed`): deferred. The outer
  lexer decides where a `Parens` ends, so a part with `'it\'s)'` or `$$)$$`
  would be mounted wrongly, and the gain over the tint is small. In TODO.md.
- **After a run**, the part tab says what was read: "read 2.3 M rows, nothing
  pushed" (instead of a hint while typing, which couldn't know what
  pushdown will push).
- **Lineage tab:** a combined statement says why it isn't traced as a whole
  yet, and offers each part's lineage on its connection (today's analysis).

## 3. Format

A combined statement is formatted part by part, from `combined_parts`: each
body with its connection's formatter language, the outer query as DuckDB with
each body replaced by a unique placeholder (as `protectParams` tags
parameters: a tag that can't occur in the text), parameters protected per
part, then the bodies put back indented under their CTE. A part whose
formatting fails is left as written; MongoDB bodies are left as written.
Today Format runs the tab's dialect over `@"Sales PG"` and may mangle it:
this comes early.

## 4. Completion (never connects)

- After `@` at the start of a CTE body: the connection names (quoted when
  needed), with kind and environment.
- Inside a part body: the provider is the part's connection's
  (`storeProvider(partConnId)`), the window is the body; ghost text as §0.
- In the outer query, a part's columns from, in order: the part's last
  output in this tab; the kept part types (pushdown's cache); the part's own
  select list (`SELECT id, total …`) or, for `SELECT * FROM t`, the explorer
  cache's columns of `t`. Nothing is asked of a server while typing (a
  describe would wake a warehouse or bill a query). An explicit "Describe
  parts" action (context menu) runs the parts' type queries on sessions of
  their own and keeps them.

## 5. Run this part, Run the join only

- **Run this part** (context menu; ⌥⌘↵ inside a part): the part's SQL alone
  on its connection, on the part's own session (auto-commit, as parts run,
  never the tab's session), read whole up to the part cap (failing past it,
  as a part does; not the tab's row limit, so it can be reused), errors marked
  in place (`base_offset`). Its output is the tab's result (shown as any
  run's) and carries `part_of: Some(name)`, a new field apart from `part`
  (which marks a combined statement's own parts, shown under its join);
  `claim_part_output` takes either.
- **Run the join only** (Run's menu): `RunRequest.reuse_parts`. For each part
  the engine claims, atomically under the registry's lock
  (`claim_part_output(tab, name, connection, sql)`), an output of this tab
  for that part that is: on the same connection; read as **bound** (the
  part's SQL with its parameters, no pushdown: its rows are a superset, and
  the outer query applies every term again) or with exactly this run's sent
  SQL; not truncated; without a checked limit; not evicted; and not older
  than the last non-read statement DataBrain ran on that connection's
  database (`same_database`: DML or DDL from any tab). Claimed outputs are
  active again, never dropped by a retried attempt (`drop_attempt` leaves
  them), and write no history row. Parts not claimed run. The notice and the
  part tabs say which parts are reused and how old they are (changes made
  outside DataBrain, and by the background jobs runner, a process of its own
  whose writes this process doesn't see, can't be told: the age is the
  warning; both in the notice and TODO.md).

## 6. Jobs

Moved up: "schedulable" is why combined queries exist.

- A query step may be a combined query (any step connection; the join on
  Results). `plan_combined` accepts `Origin::Job`; the splitter reads parts in
  their dialects for Job runs (a job's user owns its connections).
- **What the assistant may see of a job** (a leak once steps have parts):
  `visible()`, `sources_of()`, `copy_confirms()` and `step_writes()`
  (ai/jobs.rs:1005-1149) resolve a combined step's parts: a job is hidden
  from the assistant and MCP when any part's connection is, and copying a
  combined step's rows asks as for a masked or non-sharing connection. Tests
  both ways.
- **Runs the assistant started:** every step of such a run is `Origin::Ai`
  (jobs.rs:540-543, 1759); a user's combined step then fails as "not from
  the assistant yet" until phase 4. The run's review and `follower_problems`
  say so before it runs.
- A combined step counts as reading outputs (`on_results`): its outer query
  runs on DuckDB and may read `results.*` (the AI-rows and truncated-input
  checks apply).
- **Parameters:** the watermark and run-moment values are passed as typed
  values (not raw literals in the step's dialect), so each part renders them
  in its own dialect.
- `retry_refused`'s read check uses `Checked::combined` and each part's
  `unknown_functions` in its dialect.
- The Usage tab counts parts on their own connections; the browser sign-in
  warning (UI `browserSignInConnections`, backend `sign_in_warning`) covers
  every part's connection.
- **Names that change:** a step's parts are resolved when the job is saved
  (unknown: refused with the run's message); renaming a connection offers to
  update the `@name` references of the job steps that use it; deleting one
  lists those steps; export keeps the references as written.
- Part results count against the job budget (each part reserves a job share;
  TODO.md has the split).
- The step panel shows the parts' chips (§2).

## 7. Notebooks

- A notebook SQL cell may be a combined query: `plan_combined` no longer
  refuses notebook sessions. The cell's connection picker shows "combined
  (Results + n parts)" for it.
- Part sessions are keyed on the cell's run key (`Part { tab: "nb:{id}:{cell}" }`),
  so two cells with a part named `orders` don't share one: `run_part` uses
  the run's tab id for notebooks, not `req.session_key()` (`nb:{id}`). Closing
  the cell (`closeTab(cellKey)`) releases them through `belongs_to` (test).
- The open-transaction rule checks every part's connection against the
  shared notebook session (`in_transaction(part.connection_id, session_key)`),
  not only the tab's connection; query tabs get the same check.
- `runCell` sends a combined cell as is (no redirect to Results for its
  `results.` references: the engine decides).
- Dashboards stay refused (a chart pin holds one connection).

## 8. MongoDB parts

- **Body's end:** the mongosh reading `shell.rs::matching` uses (json5
  arguments: `'…'`, `"…"`, `` `…` `` with backslash escapes), plus `//` and
  `/* */` comments; no regex literals (mongosh's parser takes none).
- **Safety:** classified with `language::classify` (only reads: `find`,
  `aggregate` without `$out`/`$merge`, `countDocuments`, `distinct`). Tests
  both ways, including `runCommand` and the JSON form of `aggregate` with
  `$out`/`$merge`, `mapReduce`, and a pipeline that only reads.
- **Parameters:** `:name` in a MongoDB part is refused ("parameters aren't
  bound in MongoDB parts yet").
- **Rows:** as `RowBatcher` gives them; nested values as JSON text, read in
  the outer query with DuckDB's JSON functions (the bundled DuckDB has the
  extension; the no-DuckDB build needs a library with it: said in the docs).
  The part cap's truncation is detected as for SQL parts (test).
- **Pushdown:** skipped for non-SQL parts explicitly ("asked as written").
- **Neo4j parts:** deferred to TODO.md (no live fixture; few users join
  Cypher with SQL). When built: read access mode sessions, `--` as a
  relationship, `$p` parameters.

## 9. What changes where

| Area | Change |
|---|---|
| `ai` ghost text and editor context | clip to the part, check its connection (§0) |
| `app` | `combined_parts`; jobs: visibility, copy checks, Usage, sign-in, save/rename checks, typed parameters, retry check |
| `connector-core` | MongoDB body scanner; MongoDB parts |
| `query-engine` | notebooks and jobs allowed; per-part transaction check; reuse (`reuse_parts`, `claim_part_output`, last write per database) |
| `ui` | part marks, badges, diagnostic, summary row; Format per part; completion per part; Run this part; Run the join only; notebook cells; job step chips; Lineage tab notice |
| Docs | FEATURES, QUERY_ENGINE, CHANGE_IMPACT (ghost text and editor text sent to the AI; job visibility; the outputs registry's claim), TODO |

No new dependency. App size: code only.

## 10. PRs, in order

0. Ghost text and editor context per part (§0): a fix of today's leak.
1. `combined_parts`; part marks, badges, the unknown-connection diagnostic, the
   summary row; Run this part; the Lineage tab notice.
2. Run the join only (reuse).
3. Jobs (§6).
4. Notebooks (§7).
5. Format per part (§3).
6. Completion per part (§4).
7. MongoDB parts (§8).

Each PR: tests (both ways where a safety check changes), the checks of
AGENTS.md, a code review and a debate of the change, then merge.

## 11. Not in phase 3

Per-dialect highlighting; Neo4j parts; dashboards with combined charts; the
assistant and MCP running combined queries, history grouping (phase 4);
lineage across parts (deferred, see phase 4); a whole-table shorthand.
