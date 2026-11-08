//! Tools the AI can call. Each tool is described by a JSON Schema (sent to
//! the model and exposed over MCP) and executed here with policy checks.

use serde_json::{Value, json};

use crate::types::ToolSpec;

pub fn specs(include_editor: bool) -> Vec<ToolSpec> {
    let s = |name: &str, description: &str, parameters: Value| ToolSpec { name: name.into(), description: description.into(), parameters };
    let mut v = vec![
        s(
            "search_schema",
            "Search the connection's tables, views, columns and business notes by keywords. Returns the best matching tables as compact DDL. Use this before writing SQL when unsure which tables exist.",
            json!({"type": "object", "properties": {"query": {"type": "string", "description": "Keywords, e.g. 'monthly revenue by customer'"}, "connection": {"type": "string", "description": "Another connection (name from list_connections); default: the current one"}}, "required": ["query"]}),
        ),
        s(
            "describe_table",
            "Get columns, types, keys, comments and related tables for one table or view.",
            json!({"type": "object", "properties": {"table": {"type": "string", "description": "schema.table or table"}, "connection": {"type": "string", "description": "Another connection (name from list_connections); default: the current one"}}, "required": ["table"]}),
        ),
        s(
            "find_join_path",
            "Find how to join two tables: the shortest chains of joins (at most max_hops) over declared foreign keys and join keys learned from past queries, with the key columns and where each came from. Use it when the tables have no direct foreign key.",
            json!({"type": "object", "properties": {
                "from": {"type": "string", "description": "schema.table or table"},
                "to": {"type": "string", "description": "schema.table or table"},
                "max_hops": {"type": "integer", "minimum": 1, "maximum": 4, "description": "Most joins in a path (default 3)"},
                "connection": {"type": "string", "description": "Another connection (name from list_connections); default: the current one"}
            }, "required": ["from", "to"]}),
        ),
        s(
            "index_tables",
            "Add tables or views missing from the knowledge index: reads their columns, keys and comments from the database (after the user approves) and returns them. Use it when search_schema/describe_table can't find a table the user names, instead of asking the user for its structure.",
            json!({"type": "object", "properties": {
                "tables": {"type": "array", "items": {"type": "string"}, "description": "schema.table (catalog.schema.table on three-level engines) or a bare table name; up to 20"},
                "reason": {"type": "string", "description": "One line for the user: why these tables are needed"},
                "connection": {"type": "string", "description": "Another connection (name from list_connections); default: the current one"}
            }, "required": ["tables"]}),
        ),
        s(
            "list_tables",
            "List indexed table names (optionally only in one schema). Use search_schema for relevance ranking.",
            json!({"type": "object", "properties": {"schema": {"type": "string"}, "connection": {"type": "string", "description": "Another connection (name from list_connections); default: the current one"}}}),
        ),
        s(
            "get_sample_rows",
            "Fetch a few sample rows of a table to understand value formats (only if the connection allows sample data).",
            json!({"type": "object", "properties": {"table": {"type": "string"}, "limit": {"type": "integer", "minimum": 1, "maximum": 20}}, "required": ["table"]}),
        ),
        s(
            "run_query",
            "Execute SQL on the current connection, or on another one with `connection` (not for results.<name> outputs: use query_outputs). Write it in that connection's dialect. Reads are row-capped; the user may need to approve. Returns a summary (columns, row count, column stats), a result_id for query_result and an output handle (results.<handle>) to combine results of several connections with query_outputs. Raw rows are only returned if the connection allows it.",
            json!({"type": "object", "properties": {"sql": {"type": "string"}, "connection": {"type": "string", "description": "Optional: name or id of another connection (see list_connections); default the current one"}, "purpose": {"type": "string", "description": "One short sentence shown to the user when asking for approval"}}, "required": ["sql"]}),
        ),
        s(
            "query_result",
            "Run SQLite SQL over one previous result stored locally as table `result` (e.g. SELECT region, sum(amount) FROM result GROUP BY 1). `result_id` may be a result id or an output handle/name (r12, revenue). Nothing is sent to the database.",
            json!({"type": "object", "properties": {"result_id": {"type": "string"}, "sql": {"type": "string"}}, "required": ["result_id", "sql"]}),
        ),
        s(
            "list_outputs",
            "List query outputs available in DataBrain (handle, name, source connection, SQL, columns, row count, whether capped). Outputs can come from different databases.",
            json!({"type": "object", "properties": {"filter": {"type": "string", "description": "Optional text to match in name/SQL/connection"}}}),
        ),
        s(
            "query_outputs",
            "Run DuckDB SQL locally over outputs referenced as results.<handle or name> (e.g. SELECT * FROM results.revenue r JOIN results.r12 c USING (country)). Joins outputs from different databases; nothing is sent to any database.",
            json!({"type": "object", "properties": {"sql": {"type": "string"}, "purpose": {"type": "string"}}, "required": ["sql"]}),
        ),
        s(
            "result_summary",
            "Columns, row count and per-column statistics of a stored result (current grid result if result_id is omitted).",
            json!({"type": "object", "properties": {"result_id": {"type": "string"}}}),
        ),
        s(
            "list_knowledge_notes",
            "List this connection's notes & glossary (id, target, text, status). Check it before adding a note, to update an existing one instead of duplicating it.",
            json!({"type": "object", "properties": {"filter": {"type": "string", "description": "Optional words to match in target or text"}}}),
        ),
        s(
            "add_knowledge_note",
            "Record a durable fact learned while exploring, for future questions: a business rule, the meaning of a code or status value, a join path, a data caveat (e.g. 'active customer = status in (1,2)'). One fact per note; no query results or personal data.",
            json!({"type": "object", "properties": {"target": {"type": "string", "description": "Existing table(s): schema.table or schema.table.column; several joined with & (e.g. join paths: sales.orders & crm.customers). Omit for glossary"}, "note": {"type": "string"}}, "required": ["note"]}),
        ),
        s(
            "update_knowledge_note",
            "Correct or extend an existing note (id from list_knowledge_notes) with the full new text, keeping facts that are still true. Also used to merge imported notes into existing ones.",
            json!({"type": "object", "properties": {"id": {"type": "string"}, "note": {"type": "string", "description": "Complete new text"}, "target": {"type": "string"}}, "required": ["id", "note"]}),
        ),
        s(
            "add_knowledge_join",
            "Record how two tables join, as written in a document, a data dictionary or by the user (not a join you guessed or only tried): the key columns on both sides, in order, and where it is written. It is then used by find_join_path and shown by describe_table, without having to be seen in past queries. Tables must be in the knowledge index (index_tables first).",
            json!({"type": "object", "properties": {
                "left_table": {"type": "string", "description": "schema.table"},
                "left_columns": {"type": "array", "items": {"type": "string"}, "minItems": 1},
                "right_table": {"type": "string", "description": "schema.table"},
                "right_columns": {"type": "array", "items": {"type": "string"}, "minItems": 1, "description": "Same order as left_columns"},
                "source": {"type": "string", "description": "Where it is written, e.g. 'Orders data dictionary §3' or 'the user'"}
            }, "required": ["left_table", "left_columns", "right_table", "right_columns", "source"]}),
        ),
        s(
            "list_connections",
            "List the user's connections the assistant may use (name, engine, environment, read-only, connected), including the Results (DuckDB) connection that combines outputs. Pass a name as `connection` to run_query, list_tables, search_schema and describe_table to work on it; use it to answer across databases and to plan jobs.",
            json!({"type": "object", "properties": {}}),
        ),
        s("list_jobs", "List the user's jobs (steps, schedule, last run status).", json!({"type": "object", "properties": {"filter": {"type": "string"}}})),
        s(
            "get_job",
            "Show one job in full: each step's action, connection, SQL, links (runs after), load/export settings, last run outcome and the errors of its latest run (with the failing SQL), plus its spec as JSON to edit with update_job.",
            json!({"type": "object", "properties": {"job": {"type": "string", "description": "Job name or id"}}, "required": ["job"]}),
        ),
        s(
            "create_job",
            "Create a job (asks the user to approve): SQL steps that run in order, by hand or on a schedule. Steps can run on different connections; a step on \"Results\" reads earlier steps' outputs as results.<step> with DuckDB SQL, to join data across databases. Then load steps write rows into a table of any connection and export steps save a file. Check table names with search_schema / describe_table (connection argument) first.",
            crate::jobs::create_job_schema(),
        ),
        s(
            "run_job",
            "Run an existing job now, or only some of its steps (asks the user to approve). Waits until it finishes (up to `wait_seconds`, default 300) and returns each step's status, rows, errors, and the outputs it produced (results.<step>, readable with query_outputs, e.g. to show the comparison step's rows). Use after create_job/update_job when the user wants the result, or to re-run a job.",
            json!({"type": "object", "properties": {
                "job": {"type": "string", "description": "Name or id of the job"},
                "steps": {"type": "array", "items": {"type": "string"}, "description": "Optional: only these steps (names), not the whole job"},
                "wait_seconds": {"type": "integer", "description": "How long to wait for it (10-1800, default 300)"}
            }, "required": ["job"]}),
        ),
        s(
            "update_job",
            "Change or fix an existing job (asks the user to approve, showing what changes): send the COMPLETE new list of steps in the create_job shape, starting from get_job's spec. Steps whose name is kept keep their place and history; steps left out are removed; use `rename` to rename steps (results.<old> in other steps follows). Omit schedule to keep it. Read the job and its latest errors with get_job first, and check tables with describe_table on the step's connection.",
            crate::jobs::update_job_schema(),
        ),
        s(
            "save_query",
            "Save a query to the user's saved queries (asks for approval).",
            json!({"type": "object", "properties": {"name": {"type": "string"}, "sql": {"type": "string"}, "description": {"type": "string"}}, "required": ["name", "sql"]}),
        ),
    ];
    if include_editor {
        v.push(s(
            "get_editor",
            "Read the SQL in the user's current editor tab, their selection, the last error, and the statement that failed (failed_sql).",
            json!({"type": "object", "properties": {}}),
        ));
        v.push(s(
            "write_editor",
            "Put SQL into the user's editor. mode: replace_selection | insert_at_cursor | replace_all | new_tab. The user sees a diff and accepts or rejects it.",
            json!({"type": "object", "properties": {"sql": {"type": "string"}, "mode": {"type": "string", "enum": ["replace_selection", "insert_at_cursor", "replace_all", "new_tab"]}, "title": {"type": "string"}}, "required": ["sql"]}),
        ));
    }
    v
}
