# Lineage

`crates/lineage` reads SQL and says which columns feed which: every output column of every statement is traced through CTEs, subqueries, joins and set operations back to the table columns it comes from. It parses only (`sqlparser`, in the connection's dialect); it never talks to a database. Three callers build on it, and two of them are not diagrams:

| Caller | Uses | What depends on it being right |
|---|---|---|
| Lineage tab (`crates/app/src/api/lineage.rs`, `ui/src/lib/lineageLayout.ts`) | `analyze` with the explorer cache as catalog | what the user sees |
| **PII masking** of AI results (`crates/ai/src/pii.rs` `masked_columns`) | `analyze` with `NoCatalog` | **a safety check**: a column the trace misses can reach the AI unmasked |
| Learning joins from history (`crates/ai/src/learn.rs`) | `join_pairs`, `fingerprint` | the joins the AI is told about |

A change to how a column is traced is therefore a change to PII masking. Test it there too (`crates/ai/src/pii.rs` tests), with a masked and an unmasked case (AGENTS.md, safety checks).

Main files (`crates/lineage/src/`):

| File | What it holds |
|---|---|
| `lib.rs` | The public surface: `analyze`, `join_pairs`, `fingerprint`, the model |
| `model.rs` | `Lineage` (the graph), `Node`, `Column`, `Edge`, `EdgeKind`, `TableFlow`, `StatementInfo`, `Warning`, `Catalog` / `CatalogTable` / `NoCatalog`, `JoinPair`, `JoinKind` |
| `builder/mod.rs` | `build` (the one entry both `analyze` and `join_pairs` use), `Builder`, nodes and edges, spans, `mark_used`, the token fallback for SQL that doesn't parse |
| `builder/statement.rs` | One arm per statement kind: `write_into`, `update`, `delete`, `merge`, `insert_clauses`, `other` |
| `builder/query.rs` | `query`, `with_ctes`, `select`, `set_expr`, `projected`, `rows`, `expand_star`, `expand_view` |
| `builder/from.rs` | `FROM` and joins: `factor` (tables, CTEs, subqueries, `UNNEST`, everything else), `add_joins` |
| `builder/resolve.rs` | Which source a column reference means: `resolve_source`, `column_of`, `mark_refs` |
| `builder/joins.rs` | Equalities between sources, turned into `JoinPair`s when the script is done |
| `refs.rs` | `Refs` (the columns, aggregates and subqueries of one expression), `Source`, `Env`, small AST helpers |
| `flows.rs` | `flows`: the table-to-table summary |
| `fingerprint.rs` | `fingerprint` |
| `tests.rs`, `join_tests.rs` | Unit tests |

## What the crate owns, and what it doesn't

Owns: parsing a script statement by statement, the graph, name resolution against a `Catalog`, the `used` flags, the flows, join pairs and fingerprints.

Doesn't own:

- **Table columns.** They come from the caller's `Catalog`. The app's is the explorer cache (`Workspace::meta_get` + `kn_column_names`); with "Check with database" it describes unknown tables and reads view DDL in rounds, then calls `analyze` again after each round (up to ten times per request). PII masking passes `NoCatalog`.
- **Statement splitting and dialects** (`connector-core`: `split_statements`, `dialect_for`).
- **Offsets for the UI.** Ranges are byte offsets into the text given; `api/lineage.rs` turns them into UTF-16 and masks `:name` parameters with same-length stand-ins first.
- **The database's opinion.** "Check with database" (`EXPLAIN`, reads only) is the app's.
- **Layout and export** (`ui/src/lib/lineageLayout.ts`, `lineageExport.ts`, shared with the ER diagram: see CHANGE_IMPACT.md).

## The graph

- **Nodes** (`NodeKind`): `Table` (a database table or view, or one the script writes; also table functions and `PIVOT`, see below), `Cte`, `Subquery` (also `VALUES` and `UNNEST`), `SetOp`, `Result` (what a `SELECT` statement returns). Ids are `n0`, `n1`, … in creation order. A recursive CTE leaves an emptied placeholder node (no name, no columns); the UI skips it.
- **Table nodes are shared by the whole script**, keyed by the lower-cased qualified name the catalog gives (else the name as written). That is what links statements into a chain: the table one statement writes is the node a later one reads. A table the script writes named once bare and once with a schema is one node.
- **Columns.** A table node holds every column the catalog knows (`columns_known`), else only the ones the SQL names. Other nodes hold their own outputs; duplicate output names get `_2`, `_3`.
- **Edges** go from a column (or a whole node, for a `*` over unknown columns) to a column, or to a whole node for `Filter` and `Join` (they decide rows, not values). `EdgeKind`: `Direct` (a bare column), `Transform`, `Aggregate` (the expression calls a function named in `refs::AGGREGATES` without `OVER`), `Filter` (`WHERE`, `HAVING`, `QUALIFY`, `PREWHERE`, `MERGE … WHEN … AND`), `Join` (`ON`, `USING`). Identical edges are added once. Filter and join edges carry the clause as `detail`.
- **Spans.** Nodes, columns, edges and warnings carry the byte range they came from. Nothing inside a view's definition gets one (`in_view`): that SQL is not the editor's.
- **`partial`**: some statement didn't parse. Its tables are still found by tokens (after `FROM`, `JOIN`, `INTO`, `UPDATE`, `TABLE`) and drawn without lineage.
- **`unknown_tables`**: table nodes without known columns that the script didn't create. The app describes these.
- **Warnings** are how the crate says "I couldn't place this": unknown or ambiguous columns, unknown aliases, a `*` over unknown columns, set operations of different widths, statements it doesn't trace. Each message is reported once.

## How a statement is traced

`build` splits the script, parses each statement alone and hands it to `Builder::statement`:

| Statement | What happens |
|---|---|
| `SELECT` | `query` → a `Result` node |
| `CREATE TABLE … AS`, `CREATE VIEW`, `SELECT … INTO` | `write_into` with `defines`: the table's columns become exactly the query's (renamed by a column list) |
| `INSERT … SELECT / VALUES` | `write_into`: by the column list, else the table's own column order; then `ON CONFLICT`, `ON DUPLICATE KEY UPDATE`, `RETURNING` (`insert_clauses`) |
| `UPDATE` (with `FROM` or MySQL joins), `DELETE` (with `USING`), `MERGE` | the target is a source too; assignments feed its columns, conditions filter or join its rows; edges from the target into itself are dropped |
| `WITH … INSERT/UPDATE/DELETE/MERGE` | the same, reading the CTEs |
| `CREATE TABLE` (columns, `LIKE`) | defines the node's columns |
| `ALTER TABLE … ADD COLUMN`, `TRUNCATE`, `DROP` | the column, `written`, nothing |
| anything else | `other`: its tables without lineage, and a warning |

Inside a query:

- **`with_ctes`** analyzes each CTE in order and adds it to `Env::ctes`. A CTE that reads its own name is recursive, with or without `RECURSIVE`: a placeholder with the anchor's column names stands in while it is analyzed, then is merged into the real node.
- **`select`** builds its sources from `FROM` (`factor`), then the projection (`projected`: one output per item, edges from every column `Refs` finds, scalar subqueries linked through their first column), `*` (`expand_star`: the source's columns, or one `alias.*` column and a warning when they are unknown), the filters (`rows`), and finally `mark_refs` for `GROUP BY`, `DISTINCT ON`, `WINDOW` and `ORDER BY` (used, but nothing flows).
- **`factor`**: a one-part name that is a CTE in scope is that CTE; any other table goes through `table_node` and `expand_view`; a derived table is a `Subquery` node (`LATERAL` sees the sources before it); `UNNEST` is a one-column node fed by its argument; table functions, `PIVOT`/`UNPIVOT`, `JSON_TABLE` and the like become a `Table`-kind node named after their text, without columns.
- **`resolve_source`** places a reference. Qualified: the alias, else a source whose table name ends with the qualifier, first among the query's own sources, then the enclosing ones; if no source matches, the first part is tried as a column (struct or JSON access). Unqualified: the source that has the column (first one and a warning when several do); else the only table with unknown columns; with several such tables it warns and resolves nothing. Names in `refs::PSEUDO` (`current_date`, `user`, `level`, `rownum`, …) are never columns.
- **Views** (`expand_view`): when the catalog gives `view_sql`, the definition is traced into the view's columns, once per view, five views deep. A view the script itself writes is its own definition.

## `used` and `used_by`

Table nodes keep every known column so `*` and unqualified names resolve; the diagram lists only the used ones. `mark_used` starts from the columns the SQL names (`Builder::uses`), every column that feeds a filter or join, and every `Result` column, then walks value edges backwards. So a `*` uses all of a table's columns only when they are all used further on, and a view's definition uses only what the columns read from the view need. In a script of several statements each column also lists the statements using it (`used_by`), which the tab trims by.

## Flows

`flows` walks back from every table and result to the tables that feed it, through the nodes in between, following only the columns that reach it. One `TableFlow` per pair, with the strongest kind (`aggregate` > `transform` > `direct`; `filter` / `join` when the table only decides rows) and the statements it passes through. The Chain view of a script is drawn from these.

## Join pairs and fingerprints

- **`join_pairs`** runs the same build. While it runs, every `a.x = b.y` conjunct of an `ON`, `USING`, `MERGE … ON` or `WHERE` that links two different sources is recorded (`equi_joins`; nothing under an `OR`, nothing inside a view's definition). At the end each side is followed back along `Direct` edges to a database table column (`base_column`); a computed, unioned, ambiguous or unknown side, or a table the script creates, drops the whole condition, and so does one part of a composite key that can't be placed. A table joined to itself on the same column says nothing and is left out.
- **`fingerprint`** tokenizes, replaces literals and placeholders by `?` (a list of them by one), drops whitespace and comments, lower-cases unquoted words and hashes with FNV-1a (64 bits, hex). The tokens are printed by `sqlparser`, so an upgrade of it can change fingerprints; history learned before would then count as new queries once.

## Rules to keep

- **Warn, don't guess.** A name that can't be placed gets a warning and no edge. PII masking fails closed only when a result column has *no* source at all, so a guessed or partial trace is worse than none there (see the gaps in TODO.md).
- **A new `TableFactor` or `SetExpr` arm** must say what it does with columns it can't see: today the fallback is a `Table`-kind node with the SQL text as its name, which no PII rule and no catalog lookup matches.
- **Table nodes are matched case-insensitively**, like the explorer cache and the AI scope.
- **Everything is rebuilt per call.** There is no cache; cost is the caller's to bound (the app calls `analyze` on the async runtime).

## Tests

`tests.rs` (20 tests: statements, CTEs, views, `used`, flows, spans, warnings) and `join_tests.rs` (22: join pairs through CTEs, composite keys, what is left out). PII masking's tests (`crates/ai/src/pii.rs`) and the app's (`crates/app/src/api/tests.rs`) cover the callers. There is no fuzz target and no corpus for this crate.

Known gaps are in [TODO.md](TODO.md) under "Lineage".
