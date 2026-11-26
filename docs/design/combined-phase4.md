# Design: combined queries, phase 4 — the assistant, MCP, history

Status: approved (2026-11-27): revision 2 after a review (§1 rejected as
first written) and a debate, with their final points folded in. Parent: [cross-connection-queries.md](cross-connection-queries.md)
§6, §9, §11. Phase 3: [combined-phase3.md](combined-phase3.md).

Phase 4 lets the assistant run combined queries under every part's
connection's AI rules (MCP after it), and groups a combined run's history.
Lineage across parts is deferred (phase 3 offers each part's lineage).

## 1. Why the assistant should run them

It can already run on other connections and join outputs (`query_outputs`),
but `run_query` caps rows at 1,000: two capped parts joined give a wrong
answer. Only a combined run, with parts read whole, is right. The cheap first
step comes first: a **Run in tab** button on a combined SQL block in the chat
(a user run in a query tab), after which the assistant reads the join's
output with the existing checks. The button opens the SQL in a new tab with
its parts' summary row showing, and runs it at once only when no part is on
a production connection; otherwise the user presses Run. (The assistant's
text can be steered by what it reads; a user run can only read, so the risk
is load and cost, which the summary row and the production rule keep in
view.)

## 2. The assistant's run (`run_query` with a combined statement)

### 2.1 Which connections it may name

- `crates/ai` resolves each part's reference only among the connections this
  caller may use (`ai_enabled`, and for MCP `mcp_enabled`: the set
  `list_connections` shows it), with `find_connection` over that set only, so
  its "did you mean" and "several are named X: give one's id" messages never
  name an out-of-scope connection. Any other reference, existing or not, gets
  the same message: "no connection the assistant may use is named \"crm\"".
- It rewrites each reference it resolved to `@id:<id>` and passes the ids as
  `RunRequest.part_scope` (`#[serde(skip)]`, `#[ts(skip)]`: only in-process
  callers set it, never the UI). For Ai/Mcp origins `plan_combined` accepts
  only `@id:` parts within the scope (else refused as today, before any
  lookup), and the splitter reads only those in their dialects.
- The approval card shows each part's resolved connection and environment.
- The assistant is shown the rewritten text as the SQL that ran (its error
  positions and spans count in it). `plan_from` gets the scope too (not only
  the origin), so the splitter reads only scoped parts in their dialects.

### 2.2 The checks, per part and for the outer query

The combined branch of `run_query` comes before its `results.*` reroute to
`query_outputs` (query.rs:68-72). Per part:

- `learn_defaults` and `provider_allowed` for the part's connection.
- `policy::review_sql(&part_connection, &part_sql)`: AI enabled, its
  `run_query` mode (Never refuses the statement), table rules, unknown
  functions, read-only and production rules. A part whose review would be
  `Confirm`, or an Ask because it could write, is **refused** (a part is a
  read or nothing, parent §6), with why ("part `x` calls `f`, which DataBrain
  can't tell only reads: run it from a tab (Run in tab)"); a plain read on a
  production connection is allowed (tested).
- A part on a DuckDB connection (Results, a DuckDB file, MotherDuck) is
  refused for the assistant: it could read `results.<x>` without masks.
- The outer query: `review_sql` on Results as DuckDB SQL, `check_output_sources`
  (only the parts and outputs the assistant may read), the sandbox.

Then:

- **Always one approval** (v1), whatever each connection's mode: an AutoRead
  assistant never could cause a warehouse-sized read before. One card lists
  each part (name, connection, environment, its SQL) and the outer query.
- **Edited in the card:** the whole statement is parsed, resolved and
  reviewed again (its parts may have changed).
- **No pushdown for the assistant** (v1): the reviewed SQL is what runs
  (pushdown builds the sent SQL later, from a describe, and can't push a
  masked column right); `pushdown = Some(false)`.
- **A smaller part cap** for the assistant: 100,000 rows per part (setting
  `ai_combined_part_rows`), past it the part fails as for the user.
- **Masks:** `RunRequest.part_masks` (serde skip): a callback the engine calls
  when a part's output exists, returning that output's masked columns
  (`pii_mask` on the part's SQL and connection); the engine merges them with
  the masks of any `results.*` the outer query names before running it. It
  fails closed: an error, a panic or no answer for a part's output stops the
  statement before the outer query runs (never "no masks"; tested). The view
  definitions `pii_mask` traces (`read_view_definitions`, asynchronous) are
  read for each part after the approval and before the run, so the callback
  needs none; a view whose definition can't be read masks its part whole.
- **Rows to the model:** the join's rows are shared only when every part's
  connection shares result rows (or the user @-mentioned that part's output:
  `check_output_tables` checks `allowed_result` per part at depth > 0, so
  mentioning the join alone doesn't unlock a non-sharing part; tested);
  otherwise the summary gives the row count and columns
  only (not an error after the run). `max_rows_to_model` and sample values
  follow the strictest part's policy.
- **Audit:** the entry lists every part's connection.

### 2.3 Tests (allowed and refused)

A part on an AI-disabled connection, an existing and a made-up name (same
message, no "did you mean" outside the scope); a part on a DuckDB connection;
a part reading a denied table; a part whose review would confirm; a part
whose provider isn't allowed; an edited statement in the card adding a part;
masked columns `***` in the join; rows not shared when one part doesn't share
(count only); the outer query reading an output the assistant may not read;
`part_scope` not settable from the UI (deserialized request ignores it).

### 2.4 The prompt

"Run it with run_query: each part is checked against its connection's rules,
you ask the user once, parts are read whole (at most 100,000 rows each)."

## 3. MCP (its own PR, after §2)

The same branch. Approval: every part's connection whose decision is Ask must
allow client approval (`mcp_client_approves`), else elicitation, else
refused; any part needing the user's own confirmation refuses the call.

**Before phase 4 (a fix of today's code):** `gate()` reads
`mcp_client_approves` from the chat's connection (gate.rs:36), not from the
connection `run_query`'s `connection` argument points at, so an MCP client
may be approved on a connection that never allowed it. A small PR fixes it
now (the decision is read from the connection the query runs on), with tests
both ways; §3 extends it to every part.

## 4. History of a combined run

- Migration v28: `query_history.group_id TEXT`, indexed. A combined
  statement's join and its parts are written with `"{job}:{index}"`; parts of
  an attempt that was discarded (a retry) aren't written: a part's history
  row is held with its held `PartFinished` event until the attempt is final
  (today `run_part` writes it as the part finishes); reused parts (phase 3)
  write none.
- `list_history` stays flat (paging and search as today) and returns
  `group_id`; the panel folds entries of one group under the join (parts
  fetched by `group_id IN (…)` when the page has only some), each part with
  its connection and sent SQL. A connection filter keeps a group when any of
  its entries is on that connection; "show AI" applies to the join.
- "Run again" on a group runs the combined statement; a part's "Open in a new
  tab" opens it on its connection.
- The `history/` bench group runs (AGENTS.md).

## 5. Deferred

- Lineage across parts (the cost: a catalog and checks per connection, which
  may wake warehouses or ask for sign-in; masks already follow the join into
  its parts at run time). Phase 3 offers per-part lineage.
- AI-written combined job steps (unattended, scheduled, several connections:
  the riskiest combination; needs per-part `ai_rules_now` and the masks hook
  in `run_sql`).
- Pushdown and a larger cap for the assistant, once v1 has run.

## 6. What changes where

| Area | Change |
|---|---|
| `crates/ai` | Run in tab button; combined branch of `run_query` (scope, per-part review, approval, masks callback, rows sharing, audit); MCP approval per part; the prompt |
| `query-engine` | `RunRequest.part_scope`, `part_masks` (serde skip); Ai/Mcp origins in `plan_combined` for scoped `@id:` parts; masks merged before the outer query |
| `workspace` | v28 `group_id`; history writes |
| `ui` | Run in tab on chat SQL blocks; approval card with parts; History panel groups |
| Docs | FEATURES, CHANGE_IMPACT (the AI policy, PII masks, history), TODO |

## 7. PRs

0. The MCP approval fix (§3, today's code).
1. Run in tab (chat SQL blocks).
2. The assistant's combined run (§2), with a security review of the policy
   paths and hooks.
3. MCP (§3).
4. History groups (§4).

Each PR: tests both ways, checks, a code review and a debate, then merge.
