# Changelog

Notable changes to DataBrain, newest first. The format follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and versions follow [Semantic Versioning](https://semver.org/). Dates are release tags (`git tag`).

## Unreleased

### Changed

- Results larger than memory are no longer cut: past the memory a result may keep (or when the computer runs low on free memory), the rows continue on disk, in compressed files in DataBrain's folder, and the query keeps going. The first rows show at once and any page reads from disk as you scroll; sorting and filtering apply to the rows loaded in the grid, in memory, so they're quick (the footer says how many: scroll further first to include more, or use SQL on `results.<name>` for every row); find and column statistics of the whole result read every row, and `results.<name>`, export and "Load into…" see every row. The footer shows "on disk" and the files' size; they're deleted with the result. A result stops early only when the disk is nearly full (under 5% free, 1–4 GB). A chart of a result on disk is drawn from the rows loaded in the grid and says so; pinning it (as an output or a chart on a dashboard) is refused as too large, with the reason; the assistant's local aggregate and statistics explain that they'd read every row from disk (a long-running query) and use `results.<name>` instead.
- The memory query results may use follows the computer instead of a fixed 1 GB: a quarter of its memory, between 512 MB and 16 GB (2 GB on an 8 GB laptop, 16 GB on a 64 GB workstation), or a size chosen in Settings → Queries → Memory for results. One result still keeps at most half. A result also stops early, keeping the rows it has, when the computer runs low on free memory (under a tenth of it, at least 500 MB), so other programs aren't pushed into swap. The Outputs panel shows the memory used of the budget. DuckDB gets the memory left over for its own work (at most its default 80%), so the two can't promise more than the computer has.

### Fixed

- DuckDB work bigger than memory (a large sort, join or `GROUP BY` over a big file or Iceberg table) failed in the installed app with "Failed to create directory .tmp: Read-only file system": in-memory DuckDB spills into its current directory, which is `/` for an app started from Finder. It now spills into DataBrain's own folder (`duckdb_spill`), sandboxed AI sessions included.
- Opening an Excel workbook with many sheets is much faster (40 sheets: 12 s → 0.5 s) and no longer stalls the app while it reads. Sheets are read as a stream, so a sheet with a million rows opens in about a second and a half instead of ten, without holding its whole XML in memory; DuckDB reads every row only when a column mixes numbers and text. Comparing one sheet of a workbook reads only that sheet.
- Results from DuckDB (local files included) and Databricks were measured at many times their real size, so wide results stopped far too early with "they fill the 512 MB a result may keep": a 40-column result after a few thousand rows, a 1,000,000-row Excel sheet at 824,347. Each buffer is now counted once, however many columns share it; results and outputs keep what really fits.
- DuckDB results stream: a query without `LIMIT` (a whole file, a big table) shows its first rows as soon as they're read instead of after DuckDB has built the entire result, and stopping at the row limit doesn't read the rest.
- Importing a file loaded only its first 1,000 rows (the preview's `LIMIT`). An import now reads the whole file.
- "Load into…" on a result that stopped at the row limit or the memory a result may keep can load every row: it runs your query again and writes the rows as they arrive, so none of the limits apply and the rows are never all in memory (the default for a cut result; "Only the rows of the result" keeps the old behavior). Only your own read queries run again, not ones the AI or a client ran.
- An Excel workbook whose first sheet is empty (a blank cover sheet) opens its first sheet with data instead of failing with "No rows found in xlsx file".
- Excel: a title above the table ("Quarterly report" over the real header) is no longer taken for the header, and a stray value far from the table (e.g. in `KO5001`) no longer turns the sheet into hundreds of empty columns. The SQL notes the cells it leaves out (`-- The table reads A4:D30 of A1:KO5001 …`).

## 0.4.1 - 2026-11-13

### Added

- Keep a result: **Keep** on a result's toolbar keeps it in the tab when you run another query, as a tab of its own (with a pin) before the new results, so you can switch between them. It stays in memory, and after a restart, until you stop keeping it (× on its tab, or **Kept** on the toolbar) or close the editor tab.

### Fixed

- Safety: a Snowflake procedure called as a table function with spaces or a comment around the dot in its name (`table(myschema . proc())`) is no longer taken for a read.
- Passwords kept out of history and logs inside function and `DO` bodies (`$$ … identified by 'pw' … $$`, which 0.4.0 stopped reading) and inside statements run from a string (`EXECUTE 'alter role … password ''pw'''`, `EXECUTE IMMEDIATE '…'`).

## 0.4.0 - 2026-11-13

### Changed

- The AI uses only the tables you allow. Knowledge → Index now chooses what the AI may use (schemas, with single tables excluded or allowed), and "Read columns" reads their structure; a table outside that isn't searched, described, sampled, joined, or named by another table's foreign key or a note, and the AI can't re-allow one you excluded (the SQL it runs is still limited by the connection's table access rules). Existing connections keep exactly the tables their index had. Table metadata is now kept once, shared by the explorer, completion, lineage and the AI, so browsing a table also keeps what the AI knows of it current and a read for the AI makes the explorer instant. A table the access rules deny now reads as unavailable to the AI, like one that doesn't exist. The workspace moves to schema version 20.
- Snowflake, Databricks and DuckDB notice new functions, procedures, macros and sequences when the explorer checks for changes.

### Fixed

- Safety: statements that hide a write are no longer taken for reads, so a read-only connection refuses them and a production connection asks first: MySQL/MariaDB executable comments (`/*! … */`, `/*M! … */`), a BigQuery or ClickHouse quoted name ending in ``\` `` with a statement after it, a Snowflake `USE ROLE` with a comment in it, and a Snowflake procedure called as a table function (`select * from table(proc())`; your own table functions now ask too on production and are refused on read-only Snowflake connections, as DataBrain can't tell them from procedures).
- Query parameters: a negative value after a minus sign (`1-:p` with `-1`) no longer turns the rest of the line into a comment.
- Open transactions are followed in more cases, so an idle session holding one isn't closed: `COMMIT AND CHAIN`, SQL Server `IMPLICIT_TRANSACTIONS ON` and Snowflake `AUTOCOMMIT = FALSE`, a batch that commits and begins again, `//` and `#` comments before `BEGIN`, and a `ROLLBACK` followed by a comment. A table named `Transaction` no longer looks like one.
- Passwords and keys kept out of history and logs in more places: after a string ending in a backslash, behind a comment (`identified by -- c` then the password), `Bearer` tokens in HTTP headers, Azure SAS signatures and `privateKey`.
- The AI index no longer clears the explorer's cached columns and foreign keys of a table it couldn't read the columns of. Choosing the schemas to index keeps schema exclusions; a note the AI writes about a table is saved against the table the AI meant.
- Load into a table: on a production connection, "Load into…" and test data ask before writing (job load steps don't: the job was saved to do it). Before/after SQL that commits, rolls back, begins a transaction or turns autocommit on or off (also `XA`, MySQL `LOCK TABLES`, Snowflake `ALTER SESSION SET AUTOCOMMIT`) is refused, so a dry run can't keep its sample rows and a failed load can't claim nothing changed when something was committed.
- Grid edits made inside an open transaction: a conflict on one change now undoes the earlier changes of that apply (a savepoint) instead of leaving them pending for your next Commit. Commit, Rollback, edits and loads use one set of transaction statements per database.
- A tab can't be used by two things at once: Plan, applying grid edits, Commit/Rollback and an export from a tab with an open transaction now wait for each other and for a run, instead of one landing in the middle of another. Plan explains one statement at a time.
- Stop works while a run is still connecting or signing in; disconnecting a connection stops what runs on it; a run that fails unexpectedly no longer leaves its tab "already running".
- A result stops at half the memory results may use (512 MB), even without a row limit, and says so: very wide rows no longer exhaust memory. A job's export or load step refuses a capped result (its own or one its SQL reads) instead of writing part of it.
- SQL Server: a Plan whose `SHOWPLAN` couldn't be turned off closes the tab's session (its later statements would not have run). Oracle: the plan table is cleaned up before the session is used again. Pinning a result that can't be saved to disk no longer shows it as pinned. The AI's sample rows and deleted chats free their results' entries in the outputs list.
- Query parameters: `:name` inside a Postgres `E'…'` or Oracle `q'[…]'` string is text, not a parameter (the parameter finder now reads strings exactly as the statement splitter does).
- Scripts split where the database does: Postgres and DuckDB `E'…'` strings with `\'` inside, Oracle `q'[…]'` strings and Snowflake Scripting blocks (`BEGIN … END;`, `DECLARE … BEGIN … END;`) written without `$$` are no longer cut at their inner `;`.
- Results: dates and times before year 1 (Postgres `BC`) show instead of NULL; date-times with a UTC offset after fractional seconds (`13:04:05.25+02:00`) are read at the right instant on the cloud connectors; integers too large for a floating-point column are kept rounded and the result says so. Parameters: a value typed as an unfinished string (`'abc''`) is quoted instead of kept as SQL.
- SQL Server and Oracle: values that don't fit their column's type still show as NULL, but the result now says how many, as the other databases do.
- Transactions on DuckDB and Snowflake: a transaction you open with `BEGIN` is now seen, so the session isn't closed when idle or on the explorer's Refresh (which lost the uncommitted work), the Commit/Rollback badge shows, and manual commit is offered on those connections.
- Passwords and keys kept out of history, job logs and the AI audit in more places: `SET PASSWORD FOR …`, the old password in `ALTER USER … REPLACE`, `IDENTIFIED BY VALUES`, unquoted passwords with `#` or `$`, Redshift `SECRET_ACCESS_KEY`, Snowflake `SECRET_STRING`, DuckDB `BEARER_TOKEN` and `ENCRYPTION_KEY`, `AccountKey=` in connection strings, and secrets passed by position to ClickHouse `s3`, `mysql`, `postgresql`, `remote`, `azureBlobStorage` and SQL Server `sp_addlinkedsrvlogin`, `sp_addlogin`, `sp_password`, `sp_setapprole`.
- Safety: MySQL text with a backslash before a quote that would split into more statements on a server running `NO_BACKSLASH_ESCAPES` is asked about instead of taken for a read. Lineage's "Check with database" checks the statement with its parameter values filled in, so a raw value can't add a statement to the EXPLAIN. Neo4j: `INSERT` counts as a write and `TERMINATE TRANSACTIONS` as an admin statement.
- Safety: statements that hide a second statement or run SQL elsewhere are no longer taken for reads, so a read-only connection refuses them and a production connection asks first: SQL Server statement words written where a column or table alias goes, SELECTs DataBrain can't parse, Snowflake's `->>` chaining, remote-query functions (`OPENQUERY`, `OPENROWSET`, `OPENDATASOURCE`, DuckDB's `postgres_query`/`mysql_query`, BigQuery's `EXTERNAL_QUERY`), function names written with Unicode escapes, and a MySQL `SET` that changes several settings, each of which is now checked.
- Explorer: columns cached from completion, lineage or grid edits in schemas never opened in full are read again after a day (nothing else checked them). A slow server no longer holds up saving other explorer listings. A schema whose check was put off is checked when you open it. The explorer keeps at most 3,000 tables' columns in memory.
- Explorer: tables created, altered or dropped from the editor show in the tree (and in completion) without a Refresh click. One check runs after the script, with the schema fingerprint where the database has one; `CREATE SCHEMA` and similar also update the schema list.
- Completion, lineage and the AI use the newer of the explorer's cached columns and the AI index (it used to prefer the index even when the explorer had read the table since). Foreign keys are now cached with a table's columns, so test data's instant column list has them. Lineage with "Check with database" reads tables whose schema changed since they were cached again before tracing.
- Explorer: pointing a connection at another server (host, database, user…) now also forgets the AI's index of the old server's tables (your notes stay), so completion, lineage and the AI no longer offer tables that aren't there. Clicking a schema's Refresh while it is still loading no longer shows it empty. An explorer shown offline checks the server again as soon as a query on that connection succeeds, without a Refresh click.
- Generate test data: "Generate values with AI" and "Suggest patterns with AI" work with Kiro. With only Kiro set up, the dialog said "Add an AI provider" although one was there. Each batch is one prompt to kiro-cli with no tools offered, so it is slower than a model provider.
- Generate test data opens at once: the columns come from the AI index or the explorer's cache while the table is checked with the database in the background (it used to wait for the column read and the DDL, slow on big catalogs). Your ticks are kept when the check updates the list; if the database can't be reached, the dialog says the columns are as last known.
- SSH tunnels: russh updated to 0.65.1 (RUSTSEC-2026-0495). The advisory is in russh's SSH agent server, which DataBrain doesn't use (it only talks to your agent as a client), so this is housekeeping that keeps the dependency audit green.
- Generate test data: unique values are compared ignoring case and trailing spaces, as case-insensitive unique indexes do (MySQL `_ci`, SQL Server), so `Ann@x.com` and `ann@x.com` no longer collide on load. Many rows from a few AI rows stay fast: numbered variants go on from the last one instead of trying 2, 3, 4… again (100,000 rows no longer block).
- Generate test data: parent keys are read in key order, so the same seed picks the same foreign key values on another run; date-times printed with an offset (`2024-05-01 10:00:00+07`, Postgres `timestamptz`) are read correctly, so new keys continue after the largest and foreign keys get their parent's values.
- Generate test data: a primary key of several columns whose first column is a foreign key (`order_id, line_no`) counts up the other column, so rows don't repeat a key; a key of foreign keys only says that rows may repeat.
- Generate test data: computed columns (SQL Server `AS (…)`, `GENERATED ALWAYS AS (…)`, ClickHouse `MATERIALIZED` / `ALIAS`, `rowversion`) can't be ticked, since inserting into them always fails.
- Generate test data: decimals wider than 38 digits (Postgres `numeric(60,2)`, BigQuery `BIGNUMERIC(50,20)`, ClickHouse `Decimal256`) take patterns and keys; ClickHouse `DecimalN(S)` columns get decimals with that scale; Oracle `NUMBER(10,-2)` keys are refused with the reason (the database rounds them together).
- Reading the next value of an Oracle (`seq.nextval`) or SQL Server (`NEXT VALUE FOR seq`) sequence is no longer treated as a read: it asks first on production connections and is refused on read-only ones, as Postgres `nextval()` already was.
- Passwords in connection URLs are now also hidden in history, logs and AI transcripts when they contain `)`, `(`, quotes or `<`, or are digits followed by `?` (`postgres://u:1234?a=b@h/db`).
- AI usage counts inline completions cancelled by typing and test-data batches stopped half way (estimated from the text sent and received, as the provider bills them); they counted 0.
- The knowledge index hint above the chat reads the index state again when reading it fails, instead of staying hidden until the index changes.
- Lineage lists columns used only in a named `WINDOW`, `DISTINCT ON`, `RETURNING`, or `INSERT … ON CONFLICT … DO UPDATE … WHERE` as used, not under "+N unused columns"; a view's `ORDER BY` no longer counts its columns as used; and the lineage of one column of a script lists, in each table, the columns its own statement uses.

## 0.3.5 - 2026-11-12

### Added

- In-app updates: from this version on, the update banner installs a new version and restarts DataBrain, instead of linking to the download page (macOS, Windows and the Linux AppImage; `.deb` and `.rpm` installs keep the link). Updates are checked against a signature before they install. Install 0.3.5 itself by hand once; later versions then update in place.

## 0.3.4 - 2026-11-12

### Changed

- Smaller download (about half): the app, the MCP server and the jobs runner are one program (`databrain`, also `databrain mcp` and `databrain jobs …`) instead of three that each carried a full copy of DataBrain. `databrain-mcp` and `databrain-jobs` stay, as small launchers of it, so MCP client settings, scheduled background jobs and scripts keep working unchanged (also through a link to them, e.g. in `/usr/local/bin`).

### Added

- The assistant says when the knowledge index doesn't cover the chat's connection: when none of its tables are indexed (so it can only guess table and column names), or when far fewer are indexed than the explorer lists (under half, of 20 or more; system schemas such as `information_schema` and schemas outside the connection's chosen index scope don't count). "Index in Knowledge" opens Knowledge → Index on that connection; the hint shows indexing progress while it runs, and can be dismissed per connection. It isn't shown where the connection doesn't use the index (AI off, or schema metadata not shared).
- Column patterns in Generate test data: a Pattern column sets the rule each column's values follow: ranges for numbers, dates and times (`18..90`, `1950-01-01..2005-12-31`, `08:00..18:00`), `{a|b|c}` for one of a list, and templates for text (`%@gmail.com`, `INV-#####`, `+84 ### ### ###`, `%d/%m/%Y`, where `#` is a digit, `?` a letter and `%` a word that suits the column). Columns get sensible defaults from their names (`age` 18..90, `price` 1..1000, `rating` 1..5, `birth_date` 1950–2005, latitude and longitude), "Suggest patterns with AI" asks the AI for patterns from the table's structure, and AI-written values must follow them. Patterns are remembered per table, and "Pattern guide" in the dialog shows how to write them with examples to click. Ranges and `{a|b}` values must fit the column's type (integer sizes and signs, decimal precision and scale, MySQL `TIMESTAMP` 1970–2038, SQL Server `smalldatetime` and `datetime`), defaults stay within it, and only the ticked columns' patterns are used; keys, foreign keys and columns the database fills take none. Unique columns and unique keys of several columns stay unique when a pattern or reused AI rows would repeat a value: a text column gets a numbered variant, a number or date another made-up value, and rows that still can't be unique are reported.
- Generate test data with AI: "Generate values with AI" in the Generate test data dialog has the AI panel's model write realistic values (names, emails, addresses, descriptions that belong together in a row) instead of random words. By default only text columns, which keeps the cost low; untick "Only text columns" to have it write every column it can. The AI writes up to 500 rows (100 by default) in batches of 25 and more rows reuse them, and only the table's structure is sent (column names, types, limits and their CHECK constraints), never its rows. Keys and foreign keys never come from the AI, and every value is checked against its column (type, range, length, allowed values, unique constraints and indexes) and made up as usual when it doesn't fit; when reused AI rows would repeat a unique value or a unique combination of several columns, a text column gets a numbered variant (`Ann Lee 2`, `ann+2@x.com`); the same seed still repeats the other columns. The choices are remembered; it is off when the connection doesn't allow AI or doesn't share schema metadata, and it uses only the providers the connection allows. Its tokens count in the provider's usage (tokens today and this month).
- The provider's usage (tokens today and this month) also counts inline SQL completion; it counted only AI panel conversations.

### Security

- More passwords are hidden from query history, job logs and the assistant's transcripts and audit log: a connection URL password that starts with `/`, `?` or `#` (`postgres://u:/pw@h/db`, or digits then one of them, `u:12/ab@h`, which read as a port), a password after an email address as the user name (`postgres://me@x.com:pa/ss@h/db`), a password with a space in it (`postgres://u:pa ss@h/db`), and Postgres escape strings (`ALTER ROLE r PASSWORD E'sec\'ret'`) were kept in clear. When a URL is ambiguous the text is hidden: `https://h:8443/a@b` keeps only `https://h:***@b`.
- A `SELECT` that calls a function that acts is no longer a read: `select pg_terminate_backend(…)` (PostgreSQL's `KILL`), `pg_cancel_backend`, `pg_reload_conf`, `setval`/`nextval`, `set_config`, `lo_export`, `dblink_exec`, `lowrite`, replication-slot functions and Snowflake `system$` calls such as `system$cancel_query`, `system$abort_session` and `system$send_email` ran on read-only connections and on production without the confirmation. On Snowflake, which has no read-only session mode, every `system$` call counts as acting except known reads (`system$clustering_information`, `system$typeof`, `system$pipe_status`, …).

### Fixed

- Lineage lists only the columns of a table or view that the SQL uses (selected, computed, joined, filtered, grouped or ordered on, written), not every column the table has. A `*` uses all of the table's columns when it returns them, but in a CTE or subquery only those used further on; a view traced through its definition shows the columns read from it and, in the tables underneath, only what those need. "+N unused columns" under a table lists the rest, and "hide unused columns" hides them again. ER diagrams still list every column of each table.
- Format keeps `:name` parameters as written: it refused queries with parameters on most engines (MySQL, MariaDB, BigQuery, Snowflake, Databricks and generic SQL), and on SQL Server, DuckDB, ClickHouse and Trino turned `= :id` into `=: id`, which is no longer a parameter. Parameters are found as running finds them (not in strings, comments or `::` casts, Oracle `&name` too) and put back exactly.
- Cost preview follows the tab's connection: switching a tab to another connection without editing (BigQuery dev → prod) kept the old connection's bytes and maximum-bytes-billed warning, a reply still on its way for the old connection showed as current, and a connection without cost preview (PostgreSQL) kept the old bar. An estimate is now of one connection, statement and set of parameters; the bar clears when the tab moves, and BigQuery checks again on the new connection.
- Compare plans no longer reports a full scan that is still there as removed: a rewrite that adds a selective `WHERE` (a PostgreSQL Seq Scan estimated at 50,000 rows, then 200) showed "No longer: Seq Scan orders" and "Full scans 1 → 0" in green, because the scan's row estimate is the rows after its filter. A full scan of a table that the other plan scans in full now counts on both sides, and with Analyze the rows a filter dropped count as read.
- Compare plans explains both statements again when compared again with new parameter values, and says the tab's connection no longer exists instead of spinning.
- Generate test data on SQL Server leaves identity, computed and `timestamp`/`rowversion` columns to the database: they were filled, so the load failed (error 544). SQL Server's table DDL now shows `IDENTITY(…)` and computed `AS (…)` columns.
- Generate test data: the seed shown after generating makes the same rows again. It was larger than a JavaScript number keeps exactly, so entering it made other rows; a seed above 9007199254740991 is now refused.
- Generate test data on SQLite fills a foreign key written without columns (`REFERENCES customers`) with the parent's primary key values; it was left NULL. Foreign keys of such tables also show the parent's key columns elsewhere.
- Generate test data and Load into… write BigQuery NUMERIC and BIGNUMERIC values as `NUMERIC '…'`/`BIGNUMERIC '…'`, which BigQuery accepts.
- Generate test data no longer repeats primary keys: numbers past 2^53 are read exactly (and a key at the 64-bit limit is refused with a message); text keys fit the column's length and continue after the rows already there, or the dialog says the column is too short; date, date-time, time, float and byte keys count up instead of being random.
- Generate test data fills PostgreSQL `bit(1)` columns with `'0'`/`'1'`; `TRUE` failed.
- Generate test data refuses a primary key whose type can't hold the new keys (a `smallint` key at 32,700 with 100 rows, a `tinyint unsigned` past 255, `numeric(4,2)` past 99, MySQL `TIMESTAMP` past 2038), as it already did for 64-bit integers, text and dates; the rows were made and the load failed.

## 0.3.3 - 2026-11-11

### Added

- Plan comparison for rewrites: "Compare plans" on the assistant's proposed edit (pairs with "Improve with AI") shows the original and rewritten statement's plans side by side, with the change in estimated cost, rows, steps, full scans and (with Analyze) time, the steps only in one plan, and on BigQuery, Snowflake and Databricks the bytes each would scan.
- Cost preview before a query runs, next to the slow-query tips: "Cost" in the editor toolbar shows what the statement would scan without running it: bytes from a BigQuery dry run (also checked as you type, with a warning when over the connection's maximum bytes billed), micro-partitions and bytes from Snowflake `EXPLAIN USING JSON`, and the tables' size from Databricks `EXPLAIN COST`.
- Generate test data for a table (right-click a table → "Generate test data…"): up to 100,000 rows made up from its column types and names, with a seed to repeat them. Foreign key columns take existing keys of the parent tables, the primary key counts up past the table's maximum, and columns the database fills are left out. Preview the rows as an output, or dry-run and load them into the table through the load code.

### Security

- SQLite is updated to 3.54.1 (rusqlite 0.41) for CVE-2026-119375 / RUSTSEC-2026-0459: SQLite before 3.54 could return stale memory when opening a database whose `-wal` file has another page size, e.g. a crafted pair of files opened as a SQLite connection. Building DataBrain now needs Rust 1.89.
- A production connection asks before admin and server-wide statements too, not only data and schema changes: `KILL`, `SHUTDOWN`, `RESTORE`, `VACUUM FULL`, `REFRESH MATERIALIZED VIEW`, `FLUSH TABLES WITH READ LOCK`, `CHECKPOINT` and `SET GLOBAL …` ran without the confirmation. Reads, `BEGIN`/`COMMIT` and the harmless `USE`/`SET` a read-only connection allows still run at once. The assistant already treated these as writes.
- `USE`/`SET` and transaction control followed by more statements without a `;` (a T-SQL batch such as `USE master` then `DROP DATABASE prod`, or `set nocount on` then `kill 52`) are no longer classified by their first word: they ran on a production connection without the confirmation, and a read-only connection ran them. Session statements the SQL parser can't read ask first and are refused on a read-only connection. `BEGIN`/`START`/`END` count as transaction control only in their transaction forms, so blocks (`BEGIN DELETE FROM t END`, Oracle `BEGIN pkg.proc; END;`) and `START REPLICA` ask first and are refused on a read-only connection.
- MongoDB commands that return whole documents of a collection with a PII rule under another name (`$lookup … as: 'u'`, `{x: '$$ROOT'}`, `$push: '$$ROOT'`, `$graphLookup`, `$facet`) now have every column masked; the documents went to the assistant with the rule's fields in clear.
- PII rules that name a table (`users.email`) now mask MongoDB results: `db.users.find({})` returned `email` in clear because the rule was checked without the collection. The collections a command reads are used, nested fields match by their last part (`contact.email`), and when the collections can't be told a column named like any rule's column is masked.
- PII rules with a schema (`public.users.email`) now mask SQL that leaves the schema out (`select email from users`); names are compared part by part from the right, so `sales.users` still isn't masked and `users.email` no longer matches `superusers`.
- Whole-row expressions no longer get past PII masking: `row_to_json(u)`, `to_jsonb(users)`, `json_agg(u)`, `u::text` and `SELECT u FROM users u` carry every column of the table, so they are masked when a rule applies to that table, also when the row name is ambiguous in a join.
- Passwords with `/`, `?` or `#` in a connection URL (`postgres://u:pa/ss@h/db`) and dollar-quoted passwords (`ALTER ROLE r PASSWORD $$secret$$`) are redacted in query history, job logs and the AI audit log; the URL password was cut at the first `/` and the rest kept in clear.
- The assistant's `query_result` (SQL over a stored result) stops after 30 s and when you press Stop. It ran SQLite inside the AI turn with no limit, so a recursive CTE or a triple self-join hung the turn and held one of the app's worker threads; it now runs on a thread of its own.

### Fixed

- Loads and SQL exports write NaN and infinity as NULL instead of the text 'NaN', which SQL Server and MySQL refused and SQLite stored as text; Postgres, DuckDB and ClickHouse float columns keep them as NaN/Infinity.
- SQL INSERT exports write dates and timestamps as typed literals where the database needs them (Oracle: `DATE '…'`, `TIMESTAMP '…'`), as loads do, instead of text Oracle refused with ORA-01861.
- "Excel safe" CSV/TSV exports also escape the header row, and escape values like `-inf` and `+NaN` that spreadsheets would read as formulas.
- Update, Merge and Sync by key match rows whose key is NULL, so a Merge no longer inserts those rows again on every run; keys the table declares NOT NULL still match with `=`, so indexes are used.
- Editing a grid row whose key ends in a newline updates that row instead of the row keyed without the newline; text values keep trailing newlines as typed.
- Credentials vault: the app, databrain-mcp and databrain-jobs saving passwords at the same time no longer lose each other's changes, and two of them creating the vault at once can no longer leave it unreadable.
- AI: a reply the provider blocked or returned empty (for example a Gemini safety stop) no longer breaks the conversation with errors on every later turn; DataBrain now says why nothing came back, and conversations already affected work again.
- Result grid: adding a row with no values typed now works on MySQL and Oracle (and Snowflake, BigQuery, Databricks, ClickHouse), and a cell set to NULL in a new row is inserted as NULL instead of the column default.
- Result grid: a computed column named like a table column (`SELECT *, 0 AS price`) is read-only instead of writing to the real column; when it isn't certain which column a result column shows, it stays read-only.
- ⌘W / Ctrl+W asks before closing a tab with unsaved changes, like the tab's close button.
- Notebook cells no longer keep their run state in memory after the cell is deleted or the notebook is closed.
- Query results no longer stay in memory when a result stream ends early or a tab is closed while its query runs.
- Result grid: exact decimals (e.g. Oracle `NUMBER(38)` ids past 2^53) sort, filter and summarize exactly instead of as floating point, so `= 9007199254740993` no longer matches 9007199254740992; `1.50` and `1.5` count as one value, and `-0.000` equals `0`.
- TSV files (`.tsv`, `.tsv.gz`) are read by tab: DuckDB guessed the delimiter and took `,` when values contained commas (`Smith, John`), splitting the columns wrong. `.psv` files are read by `|`.
- Results of a script with many statements (15 results in a tab) no longer run off the edge of the result bar: the bar scrolls sideways (wheel, the ‹ › arrows, ←/→/Home/End on a result), keeps the result shown in view, and "All results" lists every result to find one by its number, SQL or output name.

## 0.3.2 - 2026-11-09

### Added

- Settings → Editor: inline AI completion can use its own provider and model instead of the AI panel's, e.g. a local Ollama model while the assistant uses Kiro (which can't complete inline). A model chosen there is used over the provider's fast model.
- Documented joins: when you tell the assistant, or a document or data dictionary says, which columns two tables join on, it records the join with its source (`add_knowledge_join`, also over MCP). Tables and columns are checked against the index; the join is used by `find_join_path` and `describe_table` without being seen in queries, and stays when learning is off or forgotten. The assistant's joins wait in Knowledge → Learned under "To review" (approve or discard on the line) unless the connection saves AI notes directly.
- Knowledge tab: Index, Notes and Learned are separate tabs (Notes shows a badge for notes the AI proposed). Learned lists one join per line with a status dot, a filter by table or column and by status (given to the AI, waiting, rejected), nothing to confirm (the AI gets a join from use: 2 queries or 2 days), reject a wrong one (or undo) on the line, and the details, queries and "Make note" when you open it; a schema shared by every table is said once. Most used tables have their own list.
- Compare tab: "Show hidden characters" shows changed cells character by character, so a no-break space, a tab, a zero-width space, a BOM, an accent written apart or a badly decoded `�` can be seen; the characters that differ are highlighted and named on hover. Values that only differ in such characters are flagged "unseen".
- Compare outputs: one column can be compared with several (`debit_amt = debit_amt`, `credit_amt = credit_amt` and `debit_amt = credit_amt`); pairs typed under the columns are added next to the matched ones instead of replacing them, each with its own result column (`debit_amt → credit_amt`). Each compared pair has a ✕ to leave it out.
- Compare tab: the Before card is red and the After card green, the same colours as the old → new values in changed cells, which now have a light red or green background.
- Compare outputs: time-zone timestamps (Databricks, Snowflake, BigQuery…) compare and show in UTC, as the grid shows them, so `2026-11-04 00:00:00+00:00` equals an Oracle `2026-11-04 00:00:00` instead of becoming your computer's local time. Text with an offset (`…+07:00`, `…Z`) is read the same way.
- Compare data without writing queries: a side of a compare can be a table on any SQL connection or a CSV, TSV, Parquet, JSON or Excel file, read when you pick it and again on "Compare again". Right-click a table → "Compare data with…" (the same table on another connection; both sides open filled in and nothing is read until "Read tables", so a slow or failing table isn't queried before you're ready), or "Compare with…" in a result's toolbar. Key columns are suggested (the table's primary key, else columns unique on both sides).
- Compare outputs opens a compare tab instead of a raw SQL query: counts of rows that are the same, changed, only before and only after (each one a filter), changed rows per column, changed cells shown as before → after, "Only columns that differ", a warning for duplicate keys, and Keys & columns, Compare again, Open SQL and As output.
- Compare outputs: options "Ignore spaces at the ends", "Ignore case", "Empty text = NULL" and a number tolerance, and a "Compare as" choice per column (date/time, date only, number, text, boolean, exact) showing both columns' types.
- The AI learns how tables join from query history (Edit connection → AI → "Learn from query history", on by default): join keys from queries you and your jobs ran, including tables without declared foreign keys, and which tables are used most. A join is given to the AI after 2 different queries or 2 days of use; the AI's own queries never count. Review, reject or forget them in Knowledge → Learned.
- `find_join_path` tool (assistant and MCP): the shortest chains of joins between two tables over declared foreign keys and learned joins.
- Clear history → "Also forget what the AI learned from history".

### Changed

- Settings → "MCP / Kiro" is now "MCP": the MCP server works with any agent that supports MCP (Claude Code, Cursor, Claude Desktop, Windsurf, Kiro CLI, …); the page says where common agents keep their MCP settings and gives the command for agents with another format.

### Fixed

- Lineage, ER diagram and DDL tabs: with the AI panel open, the toolbar buttons (Fit, Refresh, Export, Open in editor…) no longer run off the edge; like the editor's, they show only their icons when the tab is narrow (the name stays in the tooltip).
- Compare outputs reported equal values as different when the two databases typed them differently: an Oracle `DATE` (`2026-11-11 00:00:00`) against `'2026-11-11'` as text, `10.50` against `'10.5'`, padded `CHAR` against `VARCHAR`, `1` against `'1'` as a key. Without keys every such row showed as removed and added; with keys, as changed on every column. Values now compare by meaning.
- DuckDB: a tab or notebook opening a database file just as the last session on it closed could open a second instance on the file and miss tables written just before ("Table … does not exist") or fail with "No more data remaining in MetadataReader" (Linux). It now waits for the closing instance.

## 0.3.1 - 2026-11-04

### Changed

- MCP: with "Accept the MCP client's approval" on, the client's own tool approval is enough for what DataBrain would ask about, with any "AI may run queries" setting. Before, it only counted for clients without elicitation, so a client that declared elicitation was still asked (or refused when it didn't show the question), and reads needed "Automatically for read-only queries".
- MCP: a call refused for want of the user's approval tells the agent how the user allows it (Edit connection → AI → "Accept the MCP client's approval").
- MCP: `create_job`, `update_job` and `run_job` are no longer offered to MCP clients. Jobs are created, changed and run in DataBrain's assistant; `list_jobs` and `get_job` still work over MCP.
- Large source files are split into modules (connector `lib.rs` files, `tools`, `jobs`, `sql`, `databricks`, the DuckDB trampolines) with no change in behaviour, and all clippy warnings are fixed.

### Added

- `docs/ADDING_A_CONNECTOR.md`: a guide for developers and AI agents on adding a database type so that every feature works for it or says why not, with a support matrix of the current connectors. `AGENTS.md` and a Kiro steering file point to it.

### Security

- A Databricks CLI profile's token is sent only to the profile's own host; a connection to another host no longer uses it.
- Query history, job logs and job run errors redact secrets like the AI audit log does (seven common password forms: `PASSWORD =`, `ATTACH 'postgres://u:pw@h'`, `MASTER_PASSWORD=`, `pwd:`, `@rmtpassword=` and others), including what was stored before.

## 0.3.0 - 2026-11-03

### Security

- query_outputs (and run_query on DuckDB) mask an output's sensitive columns as DuckDB loads `results.<name>`, as query_result already did. They only refused SQL that named a sensitive column, so `SELECT *` under a CTE column list could rename one, and a failing cast quoted its value in the error sent to the model. Now the session only sees `***`, the SQL may name the column, and an output that wasn't checked is refused. In a DuckDB file other sessions share, outputs with sensitive columns are refused (use query_outputs).
- async-executor 1.14.0 (through zbus) was yanked: updated to 1.14.1. RUSTSEC-2026-0429 (zip `extract()` aborting on a huge declared size) only reaches zip copies used on trusted archives (DuckDB's build script, signed updates), so it is listed with the other zip exceptions.
- Rust 1.88 is now the minimum (was 1.87), which lets in tiberius 0.13 (SQL Server TLS on rustls 0.23 and rustls-webpki 0.103), time 0.3.55, and zip 9 through rust_xlsxwriter 0.102. Seven advisory exceptions are gone from `deny.toml`; the three zip ones remain for zip copies DataBrain doesn't choose (DuckDB's build script, the updater). CI checks the workspace still builds on 1.88.
- Lineage and ER diagrams only use appearance colours that are plain CSS colours (hex, `rgb()`/`hsl()`, a colour name). A custom colour containing quotes or markup went into the SVG unescaped; it now falls back to the default colour.
- Sign-in and Kiro terminal launchers are written to a private folder in your cache folder, with a new name each time and never over an existing file. They had fixed names in the shared temp folder, where another user could plant a script of that name first. Old launcher scripts are removed after a day.
- Credential vault: on Windows, `vault.json` and `vault.key` get an explicit access list (your account and SYSTEM, nothing inherited) before anything is written to them. The key, the decrypted file and the cached passwords are wiped from memory when no longer used, and encryption happens in place so no copy of the plaintext is left behind in freed memory.
- Exports write to a new temporary file created beside the chosen file (never an existing file or link of that name, which was truncated or written through before) and rename it when done. A failed or cancelled export leaves the previous file whole.
- MCP server: a message from the client is limited to 8 MiB. A longer line is answered with an error and skipped as it arrives, instead of being collected in memory without limit.
- Job export steps no longer overwrite files they didn't write. If the file name is taken by another file (or a link), the rows are saved as `name (2)`, `name (3)`… and the run says so; a file an earlier run of the same job wrote is still updated. The file is written next to its destination and moved into place, so a failed or cancelled export leaves the previous file whole.
- Oracle: the downloaded thin driver is checked against its SHA-256 every time DataBrain starts it, not only when it was installed. A file changed afterwards (or a link put in its place) is not run, and is downloaded again.
- Paths sent by the app window are checked before a file is read or written: absolute paths only, no hidden files or folders (except for importing from `.pgpass`), no writing through a symbolic link, and each command accepts only its own file types (an export writes `.csv`, `.parquet`…, a query tab never saves a `.command` or `.dylib`). Loading a DuckDB library needs a file named like one (`libduckdb.dylib`, `libduckdb.so`, `duckdb.dll`) and a confirmation in a native dialog, and the saved library path and Oracle driver URL can no longer be changed through the generic settings command.
- Sign-in redirects: the local listener only answers requests addressed to itself (a page on another name resolving to 127.0.0.1 is refused) and only pages of origins the sign-in allows (Snowflake's SSO page); it no longer sends `Access-Control-Allow-Origin: *`. OpenRouter sign-in uses a free port and a random callback path instead of port 3000 without a state.
- With "Allow write statements" on, production still refuses and confirms: a query calling a function that could write needs the user's confirmation on Prod, and elsewhere asks instead of running automatically. T-SQL like `SELECT 1 EXEC xp_cmdshell 'whoami'` (no separator) no longer counts as a read, and each statement of a script is classified by its own keyword.
- Kiro: kiro-cli runs in a private folder of the user's (owner-only, under the user's cache folder) instead of a fixed folder in the shared temp directory, which another user could create first on a multi-user computer.
- `:name` values, load INSERTs and SQL export share one text escaper. On MySQL a value used `\'`, which `NO_BACKSLASH_ESCAPES` reads as a backslash and the end of the string; quotes are now doubled and backslashes doubled. Snowflake loads now escape backslashes, and BigQuery/Databricks loads use `\'` (they read `''` as two strings). A typed literal containing a backslash is quoted as a value instead of kept.
- ClickHouse and BigQuery identifiers escape backslashes inside backticks: a table or column name ending in `\` could close its quotes and add SQL.
- SSH: an SSH server's key is trusted only after you see its fingerprint and choose Trust; it was pinned silently on first use, and an imported connections file could pin an attacker's key in advance (imported fingerprints are now dropped).
- Oracle Instant Client is loaded only from a folder you chose, the one saved for connections, or the one DataBrain installed. A copy found in Downloads, the home folder or the library search path is shown with "Use the one found", not loaded and trusted on its own.
- run_query on a DuckDB connection (the Results connection too) checks every `results.<name>` it reads like query_outputs does: outputs from a connection with AI off, not shared with MCP, or not sharing result rows were readable there.
- query_result masks sensitive columns' values before the local query runs: a CTE column list (or any rename) returned them in clear.
- A query calling a non-built-in function needs the user's confirmation wherever the AI may not write, also on Postgres, MySQL, SQLite, DuckDB and ClickHouse when the connection isn't read-only (`select public.write_fn()`, `dblink_exec(…)` and `set_config('search_path', …)` ran unasked). On read-only connections, functions that act outside the read-only session (`dblink*`, `set_config`, server and file functions) still need it.
- MongoDB: commands are classified, and their collections found for table rules, with the shell parser that runs them. A `$out` or `$merge` stage written as `{ $out : 'copy' }` passed as a read and could replace a collection.
- `EXPLAIN` is classified by the statement it explains: `EXPLAIN (ANALYZE) DELETE …` and other option lists counted as a read, so the AI could run them (also on a production connection with auto-read), and so could the Plan tab's Analyze. A plain `EXPLAIN` of a write now counts as that write too.
- SQL from the AI, MCP and AI-run jobs runs on DuckDB in a sandbox (no file, network or extension access unless the connection allows it). Files attached to a DuckDB connection stay readable in it. Job steps whose SQL the AI wrote stay sandboxed when the job runs by hand, on its schedule or in the background, not only when the AI starts the run; a step you change yourself, or mark with "Trust this SQL", becomes yours. Jobs the AI made before this are found in the audit log and marked once.
- Jobs the assistant creates, changes or runs follow the connection's AI policy, including load steps and their before/after SQL.
- Functions that could write (e.g. `SELECT dbo.write_fn()`) need the user's confirmation wherever the AI may not write, not only on read-only connections.
- Read-only connections are enforced beyond the write classifier.
- PII masking holds under aliases, expressions and `SELECT *`.
- Passwords in connection strings go to the secret store; secrets in SQL, job logs, AI chat transcripts and the audit log are redacted, including what was stored before.
- Oracle Instant Client: the download is checked against Oracle's SHA-256, and the folder's libraries against a SHA-256 manifest at every launch, before they load.
- MCP: what DataBrain would ask the user about is asked through the client (MCP elicitation). Clients without elicitation are refused unless the connection turns on "Accept the MCP client's approval".
- Key-pair JWT signing moved from the `rsa` crate (RUSTSEC-2023-0071) to aws-lc-rs. Dependencies are checked with cargo-deny in CI.

### Added

- Fuzzing and coverage: cargo-fuzz targets for the statement splitter and classifier and for identifier quoting (`fuzz/`), checking that spans cover the text in order, that a comment or whitespace never turns a write into a read, and that a quoted name always reads back as one name. A seeded test runs the same checks on generated input in every `cargo test`. A weekly workflow (also manual) fuzzes each target and measures Rust and UI test coverage (cargo-llvm-cov, Vitest v8).
- Releases include a `SHA256SUMS` file for every file attached, and each installer has a GitHub build provenance attestation (`gh attestation verify <file> --repo toan1804/databrain`).
- SECURITY.md (how to report a vulnerability privately), CONTRIBUTING.md and a Dependabot configuration for Rust, npm, GitHub Actions and the Oracle helper's Go modules.
- Parameter sets: save `:name` values under a name and apply them in any tab or notebook; jobs take parameters (a set and values of their own) that fill their steps on every run, can run once with another set, and take `--set`/`--param` from `databrain-jobs run`.
- Explorer: rows and size of a table on hover, from the database's statistics, on every engine (size where the engine keeps it).
- Load mode "Sync (mirror) by key" for job load steps and "Load into…": merge by key, then delete the table's rows that are no longer among the rows; the report counts the deletions, and a step without rows is refused. The assistant can use it in jobs.
- History: search by several words in any order across SQL, errors and connection names; filter by connection and status; run an entry again, or on another connection.
- Per-connection connect timeout and idle disconnect (General section), overriding Settings → Queries for that connection.
- ClickHouse is a load target for job load steps and "Load into…": new MergeTree tables with `Nullable` columns, Insert, Delete rows then insert (`TRUNCATE`), and Replace. Update and Merge by key are not offered there.
- Lineage: with "Check with database", views the SQL reads are traced through their definitions into the tables they read (views of views too).
- Amazon Redshift through the Postgres connector, with IAM sign-in through the AWS CLI.
- ClickHouse over its HTTP interface.
- ER diagram per schema.
- Inline AI ghost completion and SQL snippets in the editor.
- Saved grid views and per-table quick filters.
- Result grid: click a header to select the column, double-click to sort; the check Edit makes is shown before editing.
- Lineage: zoom controls and layout direction.
- Jobs: simpler step panels with outcomes; resume a failed run.
- Import connections from DataGrip; save imported connections without a user.
- Load into a table accepts table names in another case.
- Explorer arrow keys (tree navigation).
- Diagnostic log (`tracing`, daily rolling file).
- Developer ID signing, notarization and in-app updates for signed releases.

### Fixed

- ClickHouse: when a result breaks off partway (ClickHouse 25.x does that when the query fails after sending rows, sometimes without the error message), the error says so, with how many rows arrived and the query_id to look up in `system.query_log`, instead of only "error decoding response body".
- DuckDB: opening a database file right after its last session closed (a reconnect, closing a tab and reopening it) could fail with "Cannot open file …wal": the new session opened a second instance while the old one was still closing the file (on Windows: "File is already open"). A query's task finishing after its session ended could also keep the file open past its close. The file now closes after its last connection, and a new session waits for that.
- The app's code is no longer one 1.9 MB file: React, the editor and the grid are separate files, and the DDL, job, lineage, schema compare and ER diagram tabs (with the SQL formatter) load when first opened. Code read at startup is about 1.6 MB, and the build warns again above 800 kB per file.
- Job logs shown live in the app keep each run's latest 5,000 lines and the latest 10 runs; they grew for as long as the app stayed open.
- A table whose name contains a dot (`"x.y"`) is quoted as one name when editing rows, opening related rows and counting rows on SQLite; it was split into two parts.
- The per-tab lock used while connecting is removed once nobody waits on it; one was kept for every tab and job step ever opened.
- A very large idle-disconnect time (minutes) no longer overflows when turned into seconds; it means practically never.
- Two runs started at the same moment in one tab (or on one session) could both start; checking for a running query and claiming the tab now happen together.
- Jobs: run logs are pruned. Each job keeps the full log of its latest 20 runs, within the history retention (Settings → Queries); older runs keep their summary.
- The window no longer goes blank on an unexpected error or a bad value kept in the window's storage: values are checked (and dropped when they don't fit), and an error shows what happened with Reload and "Reset window layout and reload".
- Assistant: an approval shows Approved or Declined only once DataBrain took the answer; if the assistant had already stopped waiting it says so, and a failed answer shows an error and can be sent again.
- Assistant: Stop takes effect while it waits for your approval (or for an editor change), instead of after the ten-minute wait.
- Oracle: loading timestamps with a time zone works; they were sent as text, which Oracle rejected, and are now `TIMESTAMP '… +02:00'` literals.
- Jobs: a scheduled run that fails to start no longer uses up its slot; it is tried again on the next check instead of waiting for the next period.
- Workspace: an older DataBrain refuses a workspace saved by a newer one (it could drop fields it doesn't know when saving) and says why; before updating an existing workspace, DataBrain keeps a copy beside it (`workspace.db.bak-before-v<N>`).
- Assistant: streamed answers no longer show � when a character (Vietnamese text, emoji) is split across network chunks, and tool-call arguments are no longer corrupted that way.
- Jobs no longer run twice when the computer sleeps past a run's lease: a run that lost its lease to another runner (the background runner) stops before its next step, and the lease renewal stops it too.
- Assistant: stopping it during a batch of tool calls no longer breaks the conversation (every later question failed at the provider). Calls that didn't run get a "not run" result, and conversations already affected are repaired when they continue.
- Lineage: recursive CTEs without a column list resolve the recursive step's columns (the anchor's names are used), and CTEs that read themselves without `RECURSIVE` (SQL Server, Oracle) are recognised instead of shown as database tables.
- Lineage: `WITH … UPDATE`, `WITH … DELETE`, `WITH … INSERT` and `WITH … MERGE` are traced through their CTEs instead of shown as not traced.
- MCP: a call the client cancels (`notifications/cancelled`) stops at once: DataBrain no longer keeps waiting for the user's answer or running its query.
- Opening a session on a server (a tab, the explorer, the assistant, a job) gives up after the connect timeout with a message, as "Test connection" does, instead of waiting for ever on a server that never finishes the handshake. The Oracle thin driver's first download is not counted.
- MCP: a query the client never answers no longer looks like a hang. DataBrain's question waits 2 minutes instead of 5, tells the client it is waiting (progress), then ends the call with a message naming the fixes (auto-read, or "Accept the MCP client's approval"), and asks no further question in that session. The connection's AI settings explain this when MCP is on with "Ask me every time".
- A password or key read from the OS keychain that waits on a permission prompt (another binary, such as databrain-mcp, reading an item the app saved) no longer blocks the app or the MCP server: the read runs off the async runtime and gives up after the connect time limit with a message that says what to allow.
- Refresh on a connection signs in again before listing, so rights granted since the login show in the explorer (Oracle applies roles to new sessions only). Tabs running a statement or in a transaction keep their session.
- ⌘↵ in a text field (dialogs, AI prompt, cell editor) no longer runs the active tab's SQL.
- Find in results: a slow answer for an older search no longer replaces the current one.
- Dialogs keep Tab inside, give focus back on close, and are named by their title; the first field gets focus instead of the Close button.
- The AI chat no longer jumps to the bottom while you read further up.
- Job steps no longer wait forever after their upstream ended.
- DuckDB: one shared database per file between sessions; cancel keeps the connection alive.
- Reserved words are quoted; no global shortcuts behind dialogs; no system context menu outside text fields.

### Changed

- Streaming answers re-render only the message that changed.
- The UI has ESLint (React hooks rules), component tests in jsdom and Playwright smoke tests.
- Large files were split by concern (store, explorer, AI panel, results, notebook, job editor, connection form, query engine, app API); query tabs and notebook cells share one editor setup.
- The UI's backend types are generated from Rust (ts-rs).
- CI moved to Node 22 and macOS 15 runners.

## 0.2.1 - 2026-10-25

### Added

- Scheduled jobs can run while DataBrain is closed (opt-in OS timer).
- Windows and Linux builds in CI, a release workflow and an update check.
- Charts: pie and scatter, and dashboards of pinned charts.
- Schema compare with a migration script.
- Connections: import from DBeaver, pgpass or a file; export without secrets; idle sessions disconnect; MongoDB and Neo4j.
- Result grid: value panel, follow foreign keys, edit cells and rows with a SQL preview, Load into a table, import files into tables, selection summary, transposed rows.
- Editor: open and save `.sql` files, query plans (EXPLAIN / EXPLAIN ANALYZE), a transaction indicator with Commit/Rollback and manual-commit tabs, next/previous statement keys.
- Completion finds tables and columns by their comments.
- Reopen closed tabs (⇧⌘T); whole-word find in results.
- Oracle: SQL*Plus substitution variables (`&name`, `&&name`); retry on TTC errors.
- Lineage follows chains across statements; load checks the target path first.
- AI: several conversations at once, auto-approve mode, fixes for connection errors, per-connection table read/write rules, guidance on how indexing works.
- MCP tool annotations (`readOnlyHint`, `destructiveHint`, `idempotentHint`).
- Source traits: what each database type is like, declared once per connector.

### Fixed

- SQL Server: TLS through rustls (macOS's TLS broke off handshakes with some servers); "Select top 100 rows"; databases listed.
- Export: typed Postgres arrays, exact decimals, PostGIS, Excel-safe CSV; a full export reads through the tab's open transaction.
- Statement splitting: procedure bodies, `CASE` in blocks, `WITH FUNCTION`, scripts without semicolons.
- Large results show in the grid; Oracle BLOB/CLOB type names with the thin driver.
- Runs stop at the row limit without losing session state.
- MCP was blocked by DataBrain.

## 0.2.0 - 2026-10-17

### Added

- AI and jobs: the assistant creates, manages, edits and fixes jobs, works in the Jobs tab and uses several connections at a time.
- AI budget and usage display; the AI panel is draggable.
- A build without the DuckDB engine (uses the DuckDB installed on the computer).
- Command palette lists jobs.
- History: shows the latest 50 queries, delete single entries, auto-delete settings; AI and MCP queries are hidden by default.
- Result grid shows each column's data type.
- Explorer shows functions and procedures by default; the explorer shows the cache first and refreshes after.
- Editor folding; Format on DDL.
- Oracle thin driver can be installed from a downloaded file and reconnects after a lost connection.
- Choose the terminal used for sign-ins.

### Fixed

- Jobs: Stop works, and a step that can't connect no longer hangs the run.
- Fix with AI fixes the statement that failed.
- AI remembers the conversation; the AI picker works like the model picker.
- The Downloads folder is no longer asked for at startup.
- Notebook layout and folder issues.

## 0.1.1 - 2026-10-13

### Added

- Exports run in the background.

### Fixed

- The model list re-rendered in the background and overflowed the window.

## 0.1.0 - 2026-10-08

First release.

- Connections to PostgreSQL, MySQL, SQL Server, Oracle (thin driver through go-ora, no Instant Client needed), SQLite, DuckDB, Snowflake, Databricks and BigQuery, with folders and a metadata cache.
- Editor with completion (aliases, functions, packages, procedures), formatting, `:name` parameters and tips for slow queries.
- Result grid with find, filters and export, including columns over 2 GB.
- Outputs: every result has a handle and can be queried with DuckDB as `results.<name>`, compared and pinned.
- Notebooks.
- Column and table lineage.
- Jobs scheduled while DataBrain runs.
- DDL for tables, views and functions.
- AI assistant with providers including Kiro, a knowledge index, notes and `@mentions` of outputs.
- Themes.
