# Design: knowledge learned from query history

Status: draft, for review. No code yet.

## 1. Problem

The AI only knows what the knowledge index holds (`kn_objects`: columns, declared
foreign keys, comments; `kn_notes`: approved notes). Many real databases declare
no foreign keys, so retrieval's one-hop FK expansion finds nothing and the model
guesses join keys from column names.

`query_history` already shows how people actually use the database. A query like

```sql
SELECT … FROM sales.orders o JOIN crm.customers c ON c.cust_no = o.customer_ref
```

run successfully twenty times is strong evidence that `orders.customer_ref`
joins `customers.cust_no`. Today that evidence is never used.

**Goal:** mine history into structured, reviewable knowledge (joins first) and
feed it to retrieval, prompts and tools, under the same AI policy as metadata.

**Non-goals (v1):** embeddings, sending raw history SQL to the model, learning
from data values, cross-connection joins.

## 2. What we learn

| # | Fact | Example | Phase |
|---|---|---|---|
| 1 | **Join edge**: table A cols ↔ table B cols, equi-join | `sales.orders(customer_ref) = crm.customers(cust_no)` | 1 |
| 2 | **Table usage**: how often, how recently a table is queried | `sales.orders`: 340 queries, last 2 days ago | 1 |
| 3 | **Join path**: multi-hop route between two tables (derived from 1 at query time, not stored) | `orders → order_items → products` | 1 |
| 4 | **Common filters**: column + operator, literal only when allowed | `orders.deleted_at IS NULL` in 90% of reads | 2 |
| 5 | **Time column**: the column a table is usually filtered by date on | `orders.created_at` | 2 |
| 6 | **Similar past queries** as few-shot examples | user's own successful query for "revenue by month" | 3 |

Phase 1 is the user's example (joins) plus usage, which also improves ranking.

## 3. Pipeline

```
query_history (success rows, id > cursor)
   │  learner pass (background, incremental, capped per pass)
   ▼
filter by origin + dedupe by SQL fingerprint
   ▼
databrain-lineage::join_pairs(sql, dialect, catalog)   ← new, pure function
   ▼
resolve tables against kn_objects / explorer cache (drop unresolved)
   ▼
record sightings in kn_joins / kn_join_seen / kn_table_seen; advance kn_learn_state
   ▼
retrieval / describe_table / find_join_path / editor completion read them
```

### 3.1 Where it runs

- In `crates/ai` (new module `learn.rs`); `ai` already depends on `lineage` and
  `workspace`. `query-engine` stays unchanged.
- Triggered by a 60 s tick in the app that compares `max(query_history.id)`
  with each connection's cursor (one cheap query), plus after an index run and
  on app start. No hook in `query-engine`, and history written by the
  `databrain-jobs` background runner while the app was closed is picked up the
  same way. Each pass handles at most 500 history rows, then yields.
- First enable on a connection backfills existing history in the same capped
  passes, with progress on the existing `knowledge-event`.
- Cursor per connection: `kn_learn_state.last_history_id`. History rows are
  append-only, so `id > cursor` is exact.

### 3.2 Which history counts

| Origin / status | Weight | Why |
|---|---|---|
| `user`, success | 1.0 | Ground truth of how people query |
| `job`, success | 1.0, but deduped per SQL fingerprint per day | A job every 5 min must not dominate |
| `ai` / `mcp`, success | 0 (counted, not scored) | Avoid a feedback loop where the model learns its own guesses; a query that runs is not proof the join is right |

Decided: the line is **who ran it, not who wrote it**. AI-written SQL that the
user inserts into the editor and runs is recorded as `user` (`Origin::User` is
the editor default) and counts fully: the user looked at it and chose to run
it. Queries the assistant ran itself through `run_query` (AI panel, MCP, AI job
steps) are only counted in `ai_queries` for the UI. Saving an AI query as a
saved query does not change this; running it from the editor does.
| error / cancelled | ignored | |

Dedupe: fingerprint = hash of the SQL with literals and whitespace normalized
(sqlparser AST with values replaced by `?`). Score uses **distinct fingerprints**
and **distinct days**, not raw run count.

### 3.3 Extraction (`databrain-lineage`)

New public function, no database access:

```rust
pub struct JoinPair {
    pub left: TableRef,  pub left_cols: Vec<String>,
    pub right: TableRef, pub right_cols: Vec<String>,
    pub kind: JoinKind,          // inner, left, right, full, semi/anti, implicit
    pub statement: usize,
}
pub fn join_pairs(sql: &str, dialect: Dialect, catalog: &dyn Catalog) -> Vec<JoinPair>;
```

Rules:

- `ON` expressions: split on `AND`; keep `x.a = y.b` where each side is a
  plain column (casts like `x.a::text` unwrapped, functions dropped). Multiple
  equalities between the same pair of tables form one composite key.
- `USING (c)` → `(c) = (c)`.
- Implicit joins: `FROM a, b WHERE a.id = b.a_id` (WHERE conjuncts linking two
  FROM sources), kind `implicit`.
- Aliases, CTEs and subqueries are resolved with the existing builder scope;
  a column is kept only when it traces **directly** to a base table column
  (`Direct` edge), so `JOIN (SELECT id*10 AS k …)` is not learned.
- Self-joins are kept (`employees.manager_id = employees.id`).
- Skipped: non-equi joins, `OR` conditions, literals (`a.x = 5`), anything the
  builder warns it could not place. Unknown means not learned, never guessed.
- MongoDB: `$lookup` `localField`/`foreignField` map to the same `JoinPair`.
- Engines without a SQL dialect in lineage: no learning; the UI says why
  (per AGENTS.md, every feature works or explains).

Unqualified tables are resolved like `access::check_sql` / note targets:
knowledge index first, then explorer cache, using the default schema. If a
name is ambiguous or missing, the pair is dropped.

### 3.4 Canonical form

An edge is stored once regardless of direction: order the two sides by
`(schema.table, cols)`. `kind` counts are kept per edge
(`{"inner":12,"left":5}`), which also hints optionality (mostly `LEFT` → the
right side may be missing).

## 4. Storage (workspace migration v16)

Rows are **sightings**: one per join (or table), query fingerprint and day.
Counts are computed when read, so they are always distinct queries and
distinct days, and a job running every 5 minutes is one sighting a day.

```sql
CREATE TABLE kn_joins (            -- one row per join, canonical side order
    id            INTEGER PRIMARY KEY,
    connection_id TEXT NOT NULL,
    left_table    TEXT NOT NULL,   -- schema.table
    left_cols     TEXT NOT NULL,   -- JSON array, ordered
    right_table   TEXT NOT NULL,
    right_cols    TEXT NOT NULL,
    status        TEXT NOT NULL DEFAULT 'learned', -- learned | confirmed | rejected
    declared      INTEGER NOT NULL DEFAULT 0,      -- matches a declared FK
    first_seen    INTEGER NOT NULL,
    UNIQUE (connection_id, left_table, left_cols, right_table, right_cols)
);
CREATE TABLE kn_join_seen (
    join_id     INTEGER NOT NULL REFERENCES kn_joins(id) ON DELETE CASCADE,
    fingerprint TEXT NOT NULL,
    day         INTEGER NOT NULL,  -- UTC day number
    ai          INTEGER NOT NULL,  -- run by the assistant itself: shown, not counted
    kind        TEXT NOT NULL,     -- inner | left | right | full | semi | anti | where
    history_id  INTEGER NOT NULL,  -- latest run: the evidence
    at          INTEGER NOT NULL,
    PRIMARY KEY (join_id, fingerprint, day, ai)
);
CREATE TABLE kn_table_seen (       -- user and job runs only
    connection_id TEXT NOT NULL, table_name TEXT NOT NULL,
    fingerprint TEXT NOT NULL, day INTEGER NOT NULL, at INTEGER NOT NULL,
    PRIMARY KEY (connection_id, table_name, fingerprint, day)
);
CREATE TABLE kn_learn_state (
    connection_id TEXT PRIMARY KEY, last_history_id INTEGER NOT NULL,
    learned_at INTEGER NOT NULL, error TEXT
);
```

- Evidence is the `history_id` of sightings, never a copy of the SQL. When
  history is cleared or pruned, the evidence disappears and the counts stay.
- Sightings older than a year are pruned; a `learned` join left with none is
  deleted (confirmed and rejected ones are kept).
- "Forget" deletes a connection's joins and usage (rejections included) and
  moves its cursor to the end of the existing history, so that history is
  not learned again.
- Deleting a connection deletes all of it.
- Learned joins are **not** put in `kn_fts`; they are reached through the
  tables BM25 already found.

## 5. Scoring

```
score = (queries + 0.5·days) · decay(last_seen, half-life 90 d)
```

- **Surfaced to the AI** automatically, with no confirmation step, when
  `status = confirmed`, or `learned` and **either** `queries ≥ 2` (two
  different queries used this join) **or** `days ≥ 2` (the same query run on
  two different days). A single run on a single day is often exploration or a
  mistake (trying the wrong key once); coming back to it is the signal. One
  run is enough to show the join in the Learned tab, where the user can
  Confirm it to surface it at once. Thresholds are constants, not settings,
  in v1.
- **Rejected** edges are never surfaced and never re-learned (counts keep
  updating so the UI can show "still used 40 times").
- **Validated** at use time against `kn_objects`: if a column no longer exists
  after a re-index, the edge is hidden (not deleted) and marked stale in the UI.
- **Declared** FKs that match get `declared = 1`; they render as FKs, and the
  learned count only feeds ranking.
- Conflict signal: when the same pair of tables is joined on different keys,
  all are kept and shown with their counts; the prompt lists the top one first.

## 6. Using it

### 6.1 Retrieval (`knowledge::retrieve`)

- Ranking: BM25 hits get a small usage boost (`log(1+queries)`), so the
  heavily used `orders` beats a stale `orders_old_2019`.
- Expansion: today one hop over declared FKs from the top 4 tables. Add learned
  edges above threshold, ordered by score, inside the same `max_tables + 3`
  cap and character budget.
- Rendering in `render_object`, next to FKs:

  ```
  FK (customer_id) -> crm.customers(id)
  JOIN (customer_ref) = crm.customers(cust_no)  -- learned, 23 queries, mostly LEFT
  ```

- System prompt line: "JOIN lines are learned from this workspace's past
  queries, not declared constraints. Prefer them over guessing; mention when
  you rely on one."

### 6.2 Tools (AI agent and MCP)

- `describe_table`: include learned joins for that table.
- New `find_join_path(from, to, max_hops=3)`: BFS over declared FKs + surfaced
  learned edges, returns the hops with keys and evidence counts. Read-only;
  same table-access checks as `describe_table`.
- `add_knowledge_note` stays; a confirmed join can also be turned into a note.

### 6.3 Editor (phase 2)

- Completion after `JOIN customers c ON ` suggests the learned condition with
  the right aliases. Ghost completion gets learned joins in its small context.

### 6.4 UI

Knowledge panel gets a **Learned** tab per connection:

- Joins: tables, keys, counts, last seen, kinds, status; actions **Confirm**,
  **Reject**, **Show queries** (opens evidence from history if still there),
  **Make note**.
- Most used tables.
- Header: "Learned from N queries · last pass 2 min ago · Pause · Forget all".
- A stale or unsupported-engine state is shown with the reason.

## 7. Policy, privacy and safety

Following AGENTS.md: changes to what the AI sees get tests for both the
allowed and the refused case.

| Concern | Rule |
|---|---|
| Opt-out | New `AiPolicy.learn_from_history: bool`, default **on** (history and learned facts never leave the machine unless sent to the model). Off stops learning and hides learned facts. |
| What reaches the model | Learned joins and usage are metadata: sent only when `share_metadata`. Learned literals (phase 2 filters) also need `share_sample_values`, and never for PII columns. Raw history SQL is never sent in phase 1. Phase 3 examples need `share_sample_values` and go through the existing redaction. |
| Table access rules | Retrieval, `describe_table` and `find_join_path` drop any edge or hop touching a table outside `read_allow` / inside `read_deny`. A path through a denied table is not returned. |
| AI-generated history | Not scored by default (§3.2), so a wrong guess that happened to run doesn't become "knowledge". |
| Clearing history | "Clear history" gets a checkbox "also forget what the AI learned". Retention pruning keeps aggregates. Per-connection "Forget all" in the panel. |
| Export | Confirmed joins go into `.databrain-notes.json` export/import; learned and rejected ones don't. |
| Cost | Parsing runs off the UI thread, capped per pass, SQL over 200 KB skipped. |

## 8. Feature checklist (AGENTS.md)

| Feature | Effect |
|---|---|
| Connection settings | New toggle "Learn from query history" in AI policy |
| Explorer | Optional: learned-join badge on columns (phase 2) |
| Editor | Join completion (phase 2) |
| Result grid / plan / export / schema compare / jobs | Unaffected; jobs feed history like today |
| Lineage | Gains `join_pairs`; existing output unchanged |
| AI | Retrieval, prompt, `describe_table`, `find_join_path` |
| MCP | Same tools, same policy |
| Engines | All engines whose dialect lineage parses, plus MongoDB `$lookup`; others show "not available for this database type" |

## 9. Tests

- `lineage`: `join_pairs` for ON, composite keys, USING, implicit WHERE joins,
  CTE pass-through, derived columns (refused), OR / non-equi / literal (refused),
  self-join, aliases, each supported dialect's quoting.
- `workspace`: migration, canonical upsert, evidence cap, rejected never
  resurfaces, connection delete cascade, clear-history option.
- `ai::learn`: origin weights, job dedupe, AI origin not scored, cursor resume,
  unresolved tables dropped, threshold.
- `ai::knowledge`: learned edge rendered when `share_metadata`, not rendered
  when off or `learn_from_history` off, dropped when a side is denied by
  table rules (allowed and refused cases), stale column hidden.
- `find_join_path`: multi-hop, hop limit, denied table blocks path.

## 10. Implementation plan (phase 1)

Each step is one commit that passes every check in AGENTS.md (fmt, clippy,
MSRV 1.88, workspace tests, UI lint/tsc/vitest), with regenerated bindings
committed alongside.

**Step 1: `feat(lineage): join pairs from SQL`**
- `crates/lineage/src/joins.rs` (new): `pub fn join_pairs(sql, kind: ConnectorKind, catalog: &dyn Catalog) -> Vec<JoinPair>`,
  same signature style as `analyze`. Reuses the builder's source/alias/CTE
  scope; walks `ON`, `USING` and WHERE conjuncts linking two FROM sources.
- `model.rs`: `JoinPair`, `JoinKind`.
- Tests in `crates/lineage/src/tests.rs` (cases in §9), on Postgres, MySQL,
  MSSQL, Oracle, BigQuery and DuckDB quoting.
- `fingerprint(sql, kind)` (literals → `?`, normalized) lives here too.

**Step 2: `feat(workspace): store learned joins and table usage`**
- `migrations.rs`: v16 with the four tables in §4.
- `crates/workspace/src/learned.rs` (new): `record_join`, `record_table_use`,
  `learned_joins`, `set_join_status`, `join_evidence`, `table_usage`,
  `learn_state` / `set_learn_state`, `history_since`, `connections_to_learn`,
  `forget_learned`, `prune_learned`.
- `connections.rs`: delete learned rows with the connection;
  `AiPolicy.learn_from_history` (serde default `true`, so existing profiles
  turn on). The UI's `DEFAULT_POLICY` gets the field too.
- History clearing is unchanged: evidence is read through `query_history`,
  so cleared entries simply stop showing. "Also forget what the AI learned"
  (step 5) calls `forget_learned`.
- Tests: canonical order (A↔B = B↔A, kind flipped), distinct queries/days,
  assistant runs not counted, rejected stays rejected, evidence after
  clearing history, cursor, forget, prune, cascade, policy default.

**Step 3: `feat(ai): learn joins from query history`**
- `crates/ai/src/learn.rs` (new): `learn_pass(ws, conn, limit) -> LearnReport`
  and `learn_all`; `surfaced(join)` (confirmed, or learned with
  `queries ≥ 2 || days ≥ 2`; rejected never). Column staleness is checked
  where joins are used (step 4).
- Tables are resolved strictly (`Workspace::tables_named` over the knowledge
  index and explorer cache): a qualified name must match one schema; a bare
  name must exist in one schema, or one of its schemas must be the
  connection's default (`access::defaults`). Otherwise nothing is learned.
- MongoDB: the connector's analyzer now returns `MongoInfo.joins` from
  `$lookup` `localField`/`foreignField`, only when the field is still the
  aggregated collection's (only `$match`, `$sort`, `$limit`, `$skip`,
  `$sample`, `$unwind`, `$lookup` before it, and not inside an earlier
  lookup's `as`). Learned as kind `left`.
- With learning off nothing is read and the cursor stays (turning it on
  learns the backlog); kinds that can't be learned move the cursor to the end.
- Tests: user run counts, AI-panel/MCP run does not, AI SQL run from the
  editor (origin `user`) counts, job dedupe, other literal values are the same
  query, cursor resume, failed/huge SQL skipped, unresolved and ambiguous
  names dropped, default schema wins, declared FK marked, usage, learning
  off then on, unsupported engine, threshold edges.

**Step 4: `feat(ai): use learned joins in retrieval, prompts and tools`**
- `knowledge.rs`: `usable_joins` (surfaced, not declared, both tables
  readable under the access rules, columns still present; empty without
  `share_metadata` or with learning off), `join_lines` (rendered from the
  table's side, the usual kind flipped to match), `join_paths` (BFS over
  declared FKs + usable joins, ≤ `max_hops`, up to 3 shortest paths, declared
  keys first; denied tables never on a path, naming one is refused).
- `retrieve`: BM25 scores scaled by `1 + 0.2·ln(1 + queries)` of table usage;
  learned partners of the top 4 tables added after FK expansion within the
  same cap; `JOIN` lines appended to each table; `Retrieved.learned_joins`.
- Unlike the rest of the schema context, learned joins are filtered by the
  table access rules (the rules otherwise only apply when SQL runs).
- `agent.rs`: a prompt line when learned joins are shown. `describe_table`
  lists them. New read-only tool `find_join_path` (agent and MCP, audited,
  refused without `share_metadata`).
- Tests (allowed and refused): rendering from both sides, retrieval partners,
  sharing off, learning off, deny and allow lists, rejected / stale / declared
  hidden, confirmed shown, usage ranking, multi-hop paths, hop limit, denied
  table on or at the end of a path, declared keys without learning.

**Step 5: `feat(app): background learner and Learned tab`**
- `crates/app/src/ai_api.rs`: 60 s learner tick + start-up and post-index
  passes, progress on `knowledge-event`; commands `learned_joins`,
  `set_learned_join_status`, `forget_learned`, `learn_now`.
- `ui/src/components/ai/Knowledge.tsx`: Learned tab (joins, counts, kinds,
  last seen, Confirm / Reject / Show queries / Make note, most used tables,
  Forget all, "not available for this database type").
- Connection AI-policy form: "Learn from query history" toggle.
- History "Clear" dialog: "Also forget what the AI learned".
- Notes export/import: confirmed joins included.
- `docs/ARCHITECTURE.md` §10 and §13.5, `docs/FEATURES.md`, and
  `CHANGELOG.md` under Unreleased.

## 10b. Later phases

- Phase 2: common filters, time columns, JOIN completion in the editor,
  learned-join badge in the explorer.
- Phase 3: similar past queries as few-shot examples.

## 11. Decisions

1. `learn_from_history` defaults to **on**.
2. Counts depend on who **ran** the query: user and job runs count, runs the
   assistant made itself don't (§3.2).
3. Threshold: 2 distinct queries, or 1 query on 2 different days (§5).
4. No confirmation before the model sees a learned join; the threshold is
   enough, and Reject is always available.
