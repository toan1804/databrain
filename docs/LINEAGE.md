# Lineage

`crates/lineage` reads SQL and says which columns feed which: every output column of every statement is traced through CTEs, subqueries, joins and set operations back to the table columns it comes from. It parses only (`sqlparser`, in the connection's dialect); it never talks to a database. Three callers build on it, and two of them are not diagrams:

| Caller | Uses | What depends on it being right |
|---|---|---|
| Lineage tab (`crates/app/src/api/lineage.rs`, `ui/src/lib/lineageLayout.ts`) | `analyze` with the explorer cache as catalog | what the user sees |
| **PII masking** of AI results (`crates/ai/src/pii.rs` `masked_columns`) | `analyze` with `NoCatalog` | **a safety check**: a column the trace misses can reach the AI unmasked, so a column traced only in part says so (`untraced`) |
| Learning joins from history (`crates/ai/src/learn.rs`) | `join_pairs`, `fingerprint` | the joins the AI is told about |

A change to how a column is traced is therefore a change to PII masking. Test it there too (`crates/ai/src/pii.rs` tests), with a masked and an unmasked case (AGENTS.md, safety checks).

Main files (`crates/lineage/src/`):

| File | What it holds |
|---|---|
| `lib.rs` | The public surface: `analyze`, `join_pairs`, `fingerprint`, the model, `invariants` |
| `model.rs` | `Lineage` (the graph), `Node`, `Column`, `Edge`, `EdgeKind`, `TableFlow`, `StatementInfo`, `Warning`, `Catalog` / `CatalogTable` / `NoCatalog`, `JoinPair`, `JoinKind` |
| `builder/mod.rs` | `build` (the one entry both `analyze` and `join_pairs` use), `Builder`, nodes and edges, table lookup (`lookup`, `table_node`, `fold`), spans, `mark_used`, the token fallback for SQL that doesn't parse |
| `builder/statement.rs` | One arm per statement kind: `write_into`, `update`, `delete`, `merge`, `insert_clauses`, session statements (`use_schema`, `set_search_path`), `other` |
| `builder/query.rs` | `query`, `with_ctes`, `select`, `set_expr`, `projected`, `rows_of`, `expand_star`, `lateral_view`, `expand_view` |
| `builder/from.rs` | `FROM` and joins: `factor` (tables, CTEs, subqueries, `UNNEST`, table functions, `PIVOT` / `UNPIVOT`, everything else), `add_joins`, `function_node` |
| `builder/resolve.rs` | Which source a column reference means: `find_source`, `resolve_source`, `column_of`, `mark_refs` |
| `builder/joins.rs` | Equalities between sources, turned into `JoinPair`s when the script is done |
| `refs.rs` | `Refs` (the columns, aggregates and subqueries of one expression; lambda parameters and the dialect's pseudo-columns left out), `Source`, `Env`, small AST helpers |
| `flows.rs` | `flows`: the table-to-table summary |
| `fingerprint.rs` | `fingerprint` |
| `invariants.rs` | `check_lineage`: properties for any input, run by a seeded test and the `lineage` fuzz target |
| `tests.rs`, `join_tests.rs` | Unit tests |

## What the crate owns, and what it doesn't

Owns: parsing a script statement by statement, the graph, name resolution against a `Catalog`, the `used` flags, the flows, join pairs and fingerprints.

Doesn't own:

- **Table columns.** They come from the caller's `Catalog`. The app's is the explorer cache (`kn_column_names`; a bare name several schemas have is the connection's default schema's, `access::defaults`, else `Workspace::meta_get`'s); with "Check with database" it describes unknown tables and reads view DDL in rounds, then calls `analyze` again after each round (up to ten times per request, each on a blocking thread). PII masking passes `NoCatalog`.
- **Statement splitting and dialects** (`connector-core`: `split_statements`, `dialect_for`).
- **Offsets for the UI.** Ranges are byte offsets into the text given; `api/lineage.rs` turns them into UTF-16 and masks `:name` parameters with same-length stand-ins first.
- **The database's opinion.** "Check with database" (`EXPLAIN`, reads only) is the app's.
- **Layout and export** (`ui/src/lib/lineageLayout.ts`, `lineageExport.ts`, shared with the ER diagram: see CHANGE_IMPACT.md).

## The graph

- **Nodes** (`NodeKind`): `Table` (a database table or view, or one the script writes), `Cte`, `Subquery` (also `VALUES`, `UNNEST`, table functions, `PIVOT` / `UNPIVOT`, `LATERAL VIEW`), `SetOp`, `Result` (what a `SELECT` statement returns). Ids are `n0`, `n1`, … in creation order. A recursive CTE leaves an emptied placeholder node (no name, no columns); the UI skips it.
- **Table nodes are shared by the whole script**, keyed by the lower-cased qualified name the catalog gives, else by the name as written folded as the dialect folds it (`fold`: `"Orders"` and `orders` are two tables on Postgres, Oracle and Snowflake, one elsewhere; every name keeps its case on ClickHouse). That is what links statements into a chain: the table one statement writes is the node a later one reads. A table the script writes named once bare and once with a schema is one node; one dropped (`DROP TABLE`) and created again is a new node.
- **Columns.** A node holds every column it is known to have (`columns_known`: a table the catalog knows, a CTE or subquery), else only the ones the SQL names (a table not in the catalog, a table function without a column list). Duplicate output names get `_2`, `_3`. A column is **`untraced`** when something it is made of couldn't be placed: a name that matches no source or several, a qualifier that is no source (read as a struct field, a guess), an argument of a table function that couldn't be placed, or anything inside a `FROM` item the builder doesn't know. Its edges may then not be all of its sources.
- **Edges** go from a column (or a whole node, for a `*` over unknown columns and for the columns a `PIVOT` / `UNPIVOT` passes through) to a column, or to a whole node: always for `Filter` and `Join` (they decide rows, not values), and for values when which column they reach can't be told (a table function without a column list, `INSERT` without a column list into a table whose columns aren't known). An edge into a whole node feeds each of its columns. `EdgeKind`: `Direct` (a bare column), `Transform`, `Aggregate` (the expression calls a function named in `refs::AGGREGATES` without `OVER`), `Filter` (`WHERE`, `HAVING`, `QUALIFY`, `PREWHERE`, `MERGE … WHEN … AND`), `Join` (`ON`, `USING`). Identical edges are added once. Filter and join edges carry the clause as `detail`.
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
| `INSERT … SELECT / VALUES` | `write_into`: by the column list, else the table's own column order (values beyond it are left out, with a warning), else into the table as a whole (columns unknown, with a warning); then `ON CONFLICT`, `ON DUPLICATE KEY UPDATE`, `RETURNING` (`insert_clauses`) |
| `UPDATE` (with `FROM` or MySQL joins), `DELETE` (with `USING`), `MERGE` | the target is a source too; assignments feed its columns, conditions filter or join its rows; edges from the target into itself are dropped |
| `WITH … INSERT/UPDATE/DELETE/MERGE` | the same, reading the CTEs |
| `CREATE TABLE` (columns, `LIKE`) | defines the node's columns |
| `ALTER TABLE … ADD COLUMN`, `TRUNCATE`, `DROP` | the column, `written`, nothing (a later `CREATE` of the name makes a new node) |
| `USE`, `SET search_path`, `BEGIN`, `COMMIT`, `SAVEPOINT`, other `SET`s | kind `session`, no warning. `USE x` / `USE SCHEMA x` (a schema on MySQL, ClickHouse, Databricks and DuckDB; SQL Server's and Snowflake's `USE db` name a database and are left alone) and `SET search_path` / `schema` / `current_schema` choose where later bare names are looked up first (`Builder::search`) |
| anything else | `other`: its tables (not its CTEs) without lineage, and a warning |

Inside a query:

- **`with_ctes`** analyzes each CTE in order and adds it to `Env::ctes`. A CTE that reads its own name is recursive, with or without `RECURSIVE`: a placeholder with the anchor's column names stands in while it is analyzed, then is merged into the real node.
- **`select`** builds its sources from `FROM` (`factor`) and `LATERAL VIEW` (`lateral_view`: a source whose columns are computed from what its expression reads), then the projection (`projected`: one output per item, edges from every column `Refs` finds, scalar subqueries linked through their first column), `*` (`expand_star`: the source's columns, without `EXCLUDE` / `EXCEPT`, with `REPLACE (e AS c)` computed and `RENAME (a AS b)` renamed; over unknown columns one `alias.*` column and a warning, plus the replaced and renamed columns as their own), the filters (`rows_of`: in `HAVING` and `QUALIFY` a select alias that is no source's known column stands for its expression), `START WITH` / `CONNECT BY` (filters), and finally `mark_refs` for `GROUP BY`, `DISTINCT ON`, `WINDOW` and `ORDER BY` (used, but nothing flows).
- **`factor`**: a one-part name that is a CTE in scope is that CTE; any other table goes through `table_node` and `expand_view`; a derived table is a `Subquery` node (`LATERAL`, `CROSS APPLY` and `OUTER APPLY` see the sources before it); `UNNEST` is a one-column node fed by its argument; a table function (`flatten(input => u.email)`, `string_to_table(…) g(v)`, `TABLE(…)`, `JSON_TABLE`, `OPENJSON`) is a node fed by every column its arguments read (`function_node`), with the alias's column list or open columns; `PIVOT` / `UNPIVOT` read their table as a source of their own, pass its columns through whole, and feed the new columns from the aggregated / listed ones. Any other item is a node fed whole by its tables, every column `untraced`.
- **`resolve_source`** places a reference. Qualified: the alias (compared as the dialect folds names), else a source whose table name ends with the qualifier, first among the query's own sources, then the enclosing ones; if no source matches, the first part is tried as a column (struct or JSON access, `untraced`). Unqualified: the source that has the column (first one, a warning and `untraced` when several do); else the only source with open columns; with several such sources it warns and resolves nothing. Values are never columns: `true`, `null`, `current_date` and the like everywhere, and the dialect's own pseudo-columns (`refs::pseudo`: Oracle's `level`, `rownum`, `rowid`, `sysdate`, `user`; Postgres' and SQL Server's `user`); elsewhere `level`, `user` and `rowid` are columns. Lambda parameters (`x -> x + 1`) are not columns either.
- **Views** (`expand_view`): when the catalog gives `view_sql`, the definition is traced into the view's columns, once per view, five views deep, with bare names looked up in the view's schema first. A view the script itself writes is its own definition.

## `used` and `used_by`

Table nodes keep every known column so `*` and unqualified names resolve; the diagram lists only the used ones. `mark_used` starts from the columns the SQL names (`Builder::uses`), every column that feeds a filter or join, and every `Result` column, each with the statement using it, and passes these statement sets backwards along value edges (bit sets, until nothing changes: a column is revisited only when new statements reach it). So a `*` uses all of a table's columns only when they are all used further on, and a view's definition uses only what the columns read from the view need. In a script of several statements each column also lists the statements using it (`used_by`), which the tab trims by.

## Flows

`flows` walks back from every table and result to the tables that feed it, through the nodes in between, following only the columns that reach it. One `TableFlow` per pair, with the strongest kind (`aggregate` > `transform` > `direct`; `filter` / `join` when the table only decides rows) and the statements it passes through. The Chain view of a script is drawn from these.

## Join pairs and fingerprints

- **`join_pairs`** runs the same build. While it runs, every `a.x = b.y` conjunct of an `ON`, `USING`, `MERGE … ON` or `WHERE` that links two different sources is recorded (`equi_joins`; nothing under an `OR`, nothing inside a view's definition). At the end each side is followed back along `Direct` edges to a database table column (`base_column`); a computed, unioned, ambiguous or unknown side, or a table the script creates, drops the whole condition, and so does one part of a composite key that can't be placed. A table joined to itself on the same column says nothing and is left out.
- **`fingerprint`** tokenizes, replaces literals and placeholders by `?` (a list of them by one, a sign with its number, `VALUES` rows of them by one row), drops whitespace and comments, lower-cases unquoted words and hashes with FNV-1a (64 bits, hex). The tokens are printed by `sqlparser`, so an upgrade of it can change fingerprints; history learned before would then count as new queries once.

## Rules to keep

- **Warn, don't guess; when it must guess, say so.** A name that can't be placed gets a warning and no edge, and the column it is part of is `untraced`. PII masking treats an `untraced` column like one with no source (masked when the SQL names a sensitive column at all), so a partial trace must never look complete.
- **A new `TableFactor`, `SetExpr` or `SelectItem` option** must keep an edge from every column it reads, or mark what it returns `untraced`. The fallback arm of `factor` does the second.
- **Names compare as the dialect compares them** (`fold`). The explorer cache and the AI scope still match table names case-insensitively, so a catalog lookup of `"Orders"` may find `orders`.
- **Everything is rebuilt per call.** There is no cache; cost is the caller's to bound (the app traces on a blocking thread; history learning runs on one and skips SQL over 200 kB). Edges and warnings are de-duplicated through sets, so cost grows with the size of the graph; `add_out` still compares a new column with the node's others.

## Tests

`tests.rs` (statements, CTEs, views, `used`, flows, spans, warnings, and one test per case the 2026-11-13 review found), `join_tests.rs` (join pairs through CTEs, composite keys, what is left out, fingerprints) and `invariants.rs` (a seeded test of `check_lineage` over generated scripts in every SQL dialect). PII masking's tests (`crates/ai/src/pii.rs`, a masked and an unmasked case for each way a trace can be wrong) and the app's (`crates/app/src/api/tests.rs`) cover the callers. For longer runs: `cd fuzz && cargo +nightly fuzz run lineage corpus/lineage` (also weekly in `fuzz-coverage.yml`).

Known gaps are in [TODO.md](TODO.md) under "Lineage".
