# Known gaps to fix later

Found in reviews and left for a later release on purpose. Each says what goes wrong; remove an entry when it is fixed.

## Lineage

- Columns used only in these places are listed as unused ("+N unused columns") because the builder doesn't walk them:
  - a named `WINDOW` clause: `select sum(x) over w from t window w as (partition by status order by created_at)` (`status`, `created_at`);
  - `DISTINCT ON (customer_id)` when the column isn't also in `ORDER BY`;
  - `UPDATE … RETURNING customer_id`;
  - `INSERT … ON CONFLICT … DO UPDATE SET … WHERE archive.created_at < now()`.
- `mark_refs` (`crates/lineage/src/builder/resolve.rs`) counts `GROUP BY` / `ORDER BY` columns inside a view's definition as used, unlike `ensure_col` (over-listing, not hiding).
- With a multi-statement script and one statement selected, `used` is computed across the whole script, so a table may list columns only other statements use.

## Test data

- Uniqueness checks are case- and trailing-space-sensitive: AI values such as `Ann@x.com` and `ann@x.com` can collide under a case-insensitive unique index (MySQL `_ci`, SQL Server default collation).
- Fixing repeats of a unique key of several columns tries numbered variants from 2 each time: 100,000 rows from 100 AI rows is about 5×10⁷ attempts, blocking and not stoppable.
- Parent keys are read with `SELECT DISTINCT … LIMIT 1000` without `ORDER BY`, so the same seed may pick different foreign key values on another run.
- Postgres `timestamptz` maxima or parent keys printed with an offset (`+07`) may fail `parse_ts`: keys would start at 2020-01-01 and a NOT NULL foreign key would get NULL (depends on how `query_text` formats them; unconfirmed).
- A composite primary key whose first column is a foreign key: the second key column gets random values and composite keys aren't treated as unique groups, so rows can collide (a note says so).
- A computed column the user ticks to fill always fails to load (SQL Server error 271); it should be refused in the dialog.
- Oracle `s.nextval` and SQL Server `next value for s` stay reads in the classifier while Postgres `nextval()` is `Other` (low; affects the production prompt only).

## AI

- Inline completions cancelled by typing record 0 tokens (usage arrives at the end of the stream); in-flight test-data batches cancelled by Stop are not counted either.

## Single program (verify on Windows before relying on it)

- An update while an MCP client is connected: the installer may end the MCP session (`databrain.exe mcp`), or hit "file in use" on the `databrain-mcp.exe` launcher.
- If an MCP client kills `databrain-mcp.exe` without closing its stdin, the child `databrain.exe mcp` may keep running (no job object ties them).
- `databrain-jobs run` / `list` output in a terminal goes through the GUI-subsystem launcher's console attach; check it prints in cmd and PowerShell.

## Redaction

- A URL password containing `)`, `'`, `"` or `<` (`postgres://u:pa)ss@h/db`) or digits then `?` and a parameter (`postgres://u:1234?a=b@h/db`) is not hidden.
