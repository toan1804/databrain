//! What a source type is like, in one place: its query language, how names
//! are quoted and nested, how statements are split, how rows are capped, and
//! (for SQL databases that can be a load target) how DataBrain writes rows
//! into it. Shared code (splitter, classifier, AI row caps, job loads,
//! export, the UI) reads these instead of matching on [`ConnectorKind`].
//!
//! Adding a source type: add its [`ConnectorKind`] variant and one
//! `SourceTraits` constant below (start from the closest existing one), then
//! write the connector crate. Features a source doesn't support follow from
//! these fields: `load: None` hides it as a load target, `explain: None`
//! skips EXPLAIN checks, a non-SQL `language` turns the SQL-only features off.

use serde::Serialize;

use crate::types::ConnectorKind;

/// Query language typed in the editor.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum QueryLanguage {
    Sql,
    /// Cassandra/ScyllaDB CQL (SQL-like, no joins).
    Cql,
    /// Neo4j Cypher.
    Cypher,
    /// MongoDB shell-style JSON commands.
    MongoJson,
}

impl QueryLanguage {
    /// SQL proper: lineage, SQL formatting, EXPLAIN, SQL completion apply.
    pub fn is_sql(self) -> bool {
        self == QueryLanguage::Sql
    }

    /// Scripts are split into statements at `;` (else the text is one command).
    pub fn splits_on_semicolon(self) -> bool {
        self != QueryLanguage::MongoJson
    }
}

/// sqlparser dialect used to classify statements and trace lineage.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SqlParser {
    Generic,
    Postgres,
    Mysql,
    Sqlite,
    Mssql,
    Oracle,
    Snowflake,
    Databricks,
    Bigquery,
    Duckdb,
}

/// How identifiers are quoted.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum IdentQuote {
    /// `"name"`, `""` inside.
    Double,
    /// `` `name` ``, ``` `` ``` inside.
    Backtick,
    /// `` `name` ``, `` \` `` inside (BigQuery).
    BacktickBackslash,
    /// `[name]`, `]]` inside.
    Bracket,
}

/// Levels of names above a table.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum NameLevels {
    /// `schema.table`.
    Two,
    /// `catalog.schema.table`; schema ids are `catalog.schema`.
    Three,
    /// Three levels when the connection has no database set (SQL Server).
    ThreeWithoutDatabase,
}

/// How a query is capped to N rows (AI queries, previews).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RowLimit {
    /// `… LIMIT n`.
    Limit,
    /// `SELECT TOP (n) …`.
    Top,
    /// `… FETCH FIRST n ROWS ONLY` (`ROWNUM` around a subquery when wrapping).
    FetchFirst,
}

/// `$…$` quoting.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum DollarQuotes {
    None,
    /// Only `$$ … $$` (Snowflake).
    Plain,
    /// `$tag$ … $tag$` too (Postgres, DuckDB).
    Tagged,
}

/// How a procedural block containing `;` ends, and which statements start one.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum BlockEnd {
    /// No procedural blocks: `;` ends every statement.
    None,
    /// A line with only `/` (Oracle PL/SQL).
    SlashLine,
    /// A `GO` line (SQL Server).
    GoLine,
    /// `;` after the word `END` (SQLite triggers, BigQuery procedures).
    EndWord,
}

/// Lexical rules the statement splitter and parameter finder use.
#[derive(Debug, Clone, Copy, Serialize)]
pub struct SqlSyntax {
    /// `\'` escapes inside `'…'`.
    pub backslash_escapes: bool,
    /// …and inside `"…"` too.
    pub backslash_in_double_quotes: bool,
    /// `` `…` `` is quoted text.
    pub backtick: bool,
    /// `# comment`.
    pub hash_comment: bool,
    /// `// comment`.
    pub slash_comment: bool,
    /// `[…]` is quoted text.
    pub brackets: bool,
    pub dollar: DollarQuotes,
    /// `'''…'''` / `"""…"""`.
    pub triple_quotes: bool,
    /// `DELIMITER //` lines change the delimiter (MySQL).
    pub delimiter_directive: bool,
    pub block_end: BlockEnd,
    /// First words that start a block (`DECLARE`, `BEGIN`).
    pub block_starts: &'static [&'static str],
    /// `CREATE [OR REPLACE] <object>` that starts a block.
    pub block_objects: &'static [&'static str],
    /// `ALTER <object>` starts a block too.
    pub block_on_alter: bool,
    /// `:new` / `:old` are trigger rows, not parameters (Oracle).
    pub trigger_row_binds: bool,
    /// SQL*Plus substitution variables `&name` / `&&name` (Oracle).
    pub ampersand_vars: bool,
    /// `:name` parameters (DataBrain's own); off where `:` is the language's
    /// syntax (Cypher `(:Label)`, MongoDB `{a: 1}`).
    pub colon_params: bool,
}

/// Column types for `CREATE TABLE` from Arrow types.
#[derive(Debug, Clone, Copy, Serialize)]
pub struct TypeNames {
    pub boolean: &'static str,
    pub integer: &'static str,
    pub float: &'static str,
    pub decimal: &'static str,
    pub date: &'static str,
    pub timestamp: &'static str,
    pub timestamp_tz: &'static str,
    pub time: &'static str,
    pub binary: &'static str,
    pub text: &'static str,
}

/// Text literal style.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum TextLiteral {
    /// `'it''s'`.
    Plain,
    /// `N'it''s'` (SQL Server).
    NPrefix,
    /// `'it''s'` with `\\` doubled (MySQL, Databricks, BigQuery).
    BackslashEscaped,
}

/// `CREATE TABLE` that does nothing when the table exists.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CreateIfMissing {
    IfNotExists,
    /// `IF OBJECT_ID(…) IS NULL CREATE …` (SQL Server).
    ObjectIdCheck,
    /// `BEGIN EXECUTE IMMEDIATE …` ignoring "already exists" (Oracle).
    PlsqlBlock,
}

/// How the empty staging table of Update/Merge is created.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum StageTable {
    /// `CREATE TEMP TABLE s AS SELECT … WHERE 1 = 0`.
    Temp,
    /// `CREATE TEMPORARY TABLE …`; dropped with `DROP TEMPORARY TABLE` (MySQL).
    TemporaryDropTemporary,
    /// `CREATE TEMPORARY TABLE …` (Snowflake).
    Temporary,
    /// `#name` via `SELECT … INTO` (SQL Server).
    HashSelectInto,
    /// A regular table next to the target, dropped after (`… WHERE 1 = 0`).
    Regular,
    /// A regular table, `… LIMIT 0` (BigQuery).
    RegularLimit0,
}

/// How Update/Merge by key is written.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum MergeStyle {
    /// `MERGE INTO t AS tg USING s AS src ON …`.
    Merge,
    /// `MERGE INTO t tg USING s src ON (…)` (Oracle).
    MergeNoAs,
    /// `MERGE t AS tg …`, unqualified SET targets (BigQuery).
    MergeNoInto,
    /// `MERGE …;` (SQL Server needs the semicolon).
    MergeSemicolon,
    /// `UPDATE t AS tg SET … FROM s AS src WHERE …` + `INSERT … WHERE NOT EXISTS`.
    UpdateFrom,
    /// `UPDATE t AS tg JOIN s AS src ON … SET …` + `INSERT … WHERE NOT EXISTS` (MySQL).
    UpdateJoin,
}

/// Transactions around a load step.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct LoadTransactions {
    pub begin: &'static str,
    pub commit: &'static str,
    pub rollback: &'static str,
    /// DDL commits at once, so a step with DDL runs without one (MySQL).
    pub ddl_commits: bool,
}

/// How DataBrain writes rows into this database (job load steps, export as
/// INSERTs). `None` on [`SourceTraits::load`] = not a load target.
#[derive(Debug, Clone, Copy, Serialize)]
pub struct LoadTraits {
    pub types: TypeNames,
    pub default_rows: usize,
    pub max_rows: usize,
    pub rows_note: &'static str,
    pub default_bytes: usize,
    pub max_bytes: usize,
    pub bytes_note: &'static str,
    /// `TRUE`/`FALSE` (else `1`/`0`).
    pub bool_words: bool,
    /// Binary literal, `{}` = hex digits.
    pub binary_literal: &'static str,
    pub text_literal: TextLiteral,
    /// `DATE '…'` / `TIMESTAMP '…'` for dates (Oracle).
    pub typed_datetime_literals: bool,
    /// `INSERT ALL INTO … SELECT 1 FROM DUAL` (Oracle) instead of multi-row VALUES.
    pub insert_all: bool,
    pub create_if_missing: CreateIfMissing,
    /// `DROP TABLE` in a PL/SQL block ignoring "does not exist" (else `IF EXISTS`).
    pub drop_in_plsql_block: bool,
    /// Probe for a table with `LIMIT 0` (else `WHERE 1 = 0`).
    pub probe_limit0: bool,
    pub stage: StageTable,
    pub merge: MergeStyle,
    /// `None` = statements commit one by one.
    pub transactions: Option<LoadTransactions>,
}

/// How "first rows of a table" is written (explorer preview, AI samples).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Preview {
    /// `SELECT * FROM t` with the row limit syntax.
    Sql,
    /// `db.getSiblingDB("db").getCollection("c").find({}).limit(n)`.
    MongoFind,
    /// `MATCH (n:Label) RETURN n LIMIT n` (schema `Nodes`) or
    /// `MATCH ()-[r:TYPE]->() RETURN r LIMIT n` (schema `Relationships`).
    Cypher,
}

/// Where the first network hop of a connection goes (the quick DNS + TCP
/// check before connecting).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum NetworkTarget {
    /// `host` + `port` (or the default port).
    HostPort,
    /// `host` is an https URL or host name; port 443.
    HttpsHost,
    /// Not known up front (files, API endpoints from an account id).
    Unknown,
}

/// Static facts about a source type. Served to the UI with each connector.
#[derive(Debug, Clone, Copy, Serialize)]
pub struct SourceTraits {
    /// Short name in messages (`SQL Server`).
    pub name: &'static str,
    pub language: QueryLanguage,
    pub parser: SqlParser,
    /// Language name for AI prompts.
    pub dialect_name: &'static str,
    pub quote: IdentQuote,
    /// Unquoted names fold to upper case (Oracle, Snowflake).
    pub upper_folding: bool,
    pub levels: NameLevels,
    /// A qualified name is quoted as one (`` `project.dataset.table` ``, BigQuery).
    pub whole_path_quotes: bool,
    /// Tables are written with their catalog even inside the session's (BigQuery).
    pub always_qualify_catalog: bool,
    /// Explorer folder for tables (`Collections`, `Labels`) and views.
    pub table_folder: &'static str,
    pub view_folder: &'static str,
    /// What the top level is called (`catalog`, `database`, `project`).
    pub catalog_noun: &'static str,
    pub row_limit: RowLimit,
    pub preview: Preview,
    /// Prefix that checks a query without running it; `None` = no check.
    pub explain: Option<&'static str>,
    /// Reached over HTTPS APIs (no SSH tunnel).
    pub cloud: bool,
    /// A local file or in-process engine (no SSH, no network check).
    pub local: bool,
    pub network: NetworkTarget,
    /// The connection form shows the SSL mode list (Postgres, MySQL).
    pub ssl_setting: bool,
    /// sql-formatter language used by the UI's Format.
    pub formatter: &'static str,
    pub syntax: SqlSyntax,
    pub load: Option<LoadTraits>,
}

const KB: usize = 1024;
const MB: usize = 1024 * KB;
const APP_ROWS: &str = "DataBrain's limit per statement";

pub const STANDARD_SYNTAX: SqlSyntax = SqlSyntax {
    backslash_escapes: false,
    backslash_in_double_quotes: false,
    backtick: false,
    hash_comment: false,
    slash_comment: false,
    brackets: false,
    dollar: DollarQuotes::None,
    triple_quotes: false,
    delimiter_directive: false,
    block_end: BlockEnd::None,
    block_starts: &[],
    block_objects: &[],
    block_on_alter: false,
    trigger_row_binds: false,
    ampersand_vars: false,
    colon_params: true,
};

pub const STANDARD_TYPES: TypeNames = TypeNames {
    boolean: "BOOLEAN",
    integer: "BIGINT",
    float: "DOUBLE",
    decimal: "NUMERIC",
    date: "DATE",
    timestamp: "TIMESTAMP",
    timestamp_tz: "TIMESTAMP",
    time: "TIME",
    binary: "BLOB",
    text: "TEXT",
};

pub const STANDARD_LOAD: LoadTraits = LoadTraits {
    types: STANDARD_TYPES,
    default_rows: 500,
    max_rows: 10_000,
    rows_note: APP_ROWS,
    default_bytes: MB,
    max_bytes: 64 * MB,
    bytes_note: APP_ROWS,
    bool_words: true,
    binary_literal: "X'{}'",
    text_literal: TextLiteral::Plain,
    typed_datetime_literals: false,
    insert_all: false,
    create_if_missing: CreateIfMissing::IfNotExists,
    drop_in_plsql_block: false,
    probe_limit0: false,
    stage: StageTable::Temp,
    merge: MergeStyle::UpdateFrom,
    transactions: Some(LoadTransactions { begin: "BEGIN", commit: "COMMIT", rollback: "ROLLBACK", ddl_commits: false }),
};

/// Starting point for a new source: standard SQL, two name levels,
/// `LIMIT`, a load target with standard types. Override what differs
/// (`SourceTraits { name: "…", ..traits::STANDARD }`).
pub const STANDARD: SourceTraits = SourceTraits {
    name: "this database",
    language: QueryLanguage::Sql,
    parser: SqlParser::Generic,
    dialect_name: "SQL",
    quote: IdentQuote::Double,
    upper_folding: false,
    levels: NameLevels::Two,
    whole_path_quotes: false,
    always_qualify_catalog: false,
    table_folder: "Tables",
    view_folder: "Views",
    catalog_noun: "database",
    row_limit: RowLimit::Limit,
    preview: Preview::Sql,
    explain: None,
    cloud: false,
    local: false,
    network: NetworkTarget::HostPort,
    ssl_setting: false,
    formatter: "sql",
    syntax: STANDARD_SYNTAX,
    load: Some(STANDARD_LOAD),
};

pub const SQLITE: SourceTraits = SourceTraits {
    parser: SqlParser::Sqlite,
    network: NetworkTarget::Unknown,
    name: "SQLite",
    dialect_name: "SQLite",
    explain: Some("EXPLAIN QUERY PLAN "),
    local: true,
    formatter: "sqlite",
    syntax: SqlSyntax {
        backtick: true,
        brackets: true,
        block_end: BlockEnd::EndWord,
        block_objects: &["TRIGGER"],
        ..STANDARD_SYNTAX
    },
    load: Some(LoadTraits {
        types: TypeNames { integer: "INTEGER", float: "REAL", ..STANDARD_TYPES },
        default_bytes: 1_000_000,
        max_bytes: 1_000_000,
        bytes_note: "SQLite's default SQLITE_MAX_SQL_LENGTH is 1,000,000 bytes",
        bool_words: false,
        ..STANDARD_LOAD
    }),
    ..STANDARD
};

pub const POSTGRES: SourceTraits = SourceTraits {
    parser: SqlParser::Postgres,
    ssl_setting: true,
    name: "PostgreSQL",
    dialect_name: "PostgreSQL",
    explain: Some("EXPLAIN "),
    formatter: "postgresql",
    // SQL-standard bodies: CREATE FUNCTION … BEGIN ATOMIC …; END;
    syntax: SqlSyntax { dollar: DollarQuotes::Tagged, block_end: BlockEnd::EndWord, block_objects: &["FUNCTION", "PROCEDURE"], ..STANDARD_SYNTAX },
    load: Some(LoadTraits {
        types: TypeNames { float: "DOUBLE PRECISION", timestamp_tz: "TIMESTAMPTZ", binary: "BYTEA", ..STANDARD_TYPES },
        binary_literal: "'\\x{}'::bytea",
        ..STANDARD_LOAD
    }),
    ..STANDARD
};

pub const MYSQL: SourceTraits = SourceTraits {
    parser: SqlParser::Mysql,
    ssl_setting: true,
    name: "MySQL",
    dialect_name: "MySQL/MariaDB",
    quote: IdentQuote::Backtick,
    explain: Some("EXPLAIN "),
    formatter: "mysql",
    syntax: SqlSyntax {
        backslash_escapes: true,
        backslash_in_double_quotes: true,
        backtick: true,
        hash_comment: true,
        delimiter_directive: true,
        // Bodies without DELIMITER: BEGIN … END; (as most clients accept).
        block_end: BlockEnd::EndWord,
        block_objects: &["PROCEDURE", "FUNCTION", "TRIGGER", "EVENT"],
        ..STANDARD_SYNTAX
    },
    load: Some(LoadTraits {
        types: TypeNames { timestamp: "DATETIME(6)", timestamp_tz: "DATETIME(6)", ..STANDARD_TYPES },
        max_bytes: 16 * MB,
        bytes_note: "must also stay under the server's max_allowed_packet",
        text_literal: TextLiteral::BackslashEscaped,
        stage: StageTable::TemporaryDropTemporary,
        merge: MergeStyle::UpdateJoin,
        transactions: Some(LoadTransactions { begin: "START TRANSACTION", commit: "COMMIT", rollback: "ROLLBACK", ddl_commits: true }),
        ..STANDARD_LOAD
    }),
    ..STANDARD
};

pub const MSSQL: SourceTraits = SourceTraits {
    parser: SqlParser::Mssql,
    name: "SQL Server",
    dialect_name: "Microsoft SQL Server (T-SQL)",
    quote: IdentQuote::Bracket,
    levels: NameLevels::ThreeWithoutDatabase,
    row_limit: RowLimit::Top,
    formatter: "transactsql",
    syntax: SqlSyntax {
        brackets: true,
        block_end: BlockEnd::GoLine,
        block_objects: &["PROCEDURE", "PROC", "FUNCTION", "TRIGGER", "VIEW"],
        block_on_alter: true,
        ..STANDARD_SYNTAX
    },
    load: Some(LoadTraits {
        types: TypeNames {
            boolean: "BIT",
            float: "FLOAT",
            timestamp: "DATETIME2",
            timestamp_tz: "DATETIMEOFFSET",
            binary: "VARBINARY(MAX)",
            text: "NVARCHAR(MAX)",
            ..STANDARD_TYPES
        },
        max_rows: 1000,
        rows_note: "SQL Server allows at most 1,000 rows in one VALUES list",
        bool_words: false,
        binary_literal: "0x{}",
        text_literal: TextLiteral::NPrefix,
        create_if_missing: CreateIfMissing::ObjectIdCheck,
        stage: StageTable::HashSelectInto,
        merge: MergeStyle::MergeSemicolon,
        transactions: Some(LoadTransactions {
            begin: "BEGIN TRANSACTION",
            commit: "COMMIT TRANSACTION",
            rollback: "IF @@TRANCOUNT > 0 ROLLBACK TRANSACTION",
            ddl_commits: false,
        }),
        ..STANDARD_LOAD
    }),
    ..STANDARD
};

pub const ORACLE: SourceTraits = SourceTraits {
    parser: SqlParser::Oracle,
    name: "Oracle",
    dialect_name: "Oracle SQL / PL/SQL",
    upper_folding: true,
    row_limit: RowLimit::FetchFirst,
    formatter: "plsql",
    syntax: SqlSyntax {
        block_end: BlockEnd::SlashLine,
        block_starts: &["DECLARE", "BEGIN"],
        block_objects: &["PROCEDURE", "FUNCTION", "PACKAGE", "TRIGGER", "TYPE"],
        trigger_row_binds: true,
        ampersand_vars: true,
        ..STANDARD_SYNTAX
    },
    load: Some(LoadTraits {
        types: TypeNames {
            boolean: "NUMBER(1)",
            integer: "NUMBER(19)",
            float: "BINARY_DOUBLE",
            decimal: "NUMBER",
            timestamp_tz: "TIMESTAMP WITH TIME ZONE",
            time: "VARCHAR2(32)",
            text: "CLOB",
            ..STANDARD_TYPES
        },
        default_rows: 100,
        max_rows: 1000,
        rows_note: "DataBrain's limit for Oracle INSERT ALL (parsing slows down with many rows)",
        bool_words: false,
        binary_literal: "HEXTORAW('{}')",
        typed_datetime_literals: true,
        insert_all: true,
        create_if_missing: CreateIfMissing::PlsqlBlock,
        drop_in_plsql_block: true,
        stage: StageTable::Regular,
        merge: MergeStyle::MergeNoAs,
        transactions: None,
        ..STANDARD_LOAD
    }),
    ..STANDARD
};

pub const SNOWFLAKE: SourceTraits = SourceTraits {
    parser: SqlParser::Snowflake,
    network: NetworkTarget::Unknown,
    name: "Snowflake",
    dialect_name: "Snowflake SQL",
    upper_folding: true,
    levels: NameLevels::Three,
    explain: Some("EXPLAIN "),
    cloud: true,
    formatter: "snowflake",
    syntax: SqlSyntax { backslash_escapes: true, slash_comment: true, dollar: DollarQuotes::Plain, ..STANDARD_SYNTAX },
    load: Some(LoadTraits {
        types: TypeNames { decimal: "DECIMAL(38, 10)", timestamp_tz: "TIMESTAMP_TZ", binary: "BINARY", text: "VARCHAR", ..STANDARD_TYPES },
        default_bytes: 1_000_000,
        max_bytes: 1_000_000,
        bytes_note: "kept under 1 MB of SQL text for Snowflake",
        binary_literal: "TO_BINARY('{}', 'HEX')",
        stage: StageTable::Temporary,
        merge: MergeStyle::Merge,
        transactions: None,
        ..STANDARD_LOAD
    }),
    ..STANDARD
};

pub const DATABRICKS: SourceTraits = SourceTraits {
    parser: SqlParser::Databricks,
    network: NetworkTarget::HttpsHost,
    name: "Databricks",
    dialect_name: "Databricks SQL (Spark SQL)",
    quote: IdentQuote::Backtick,
    levels: NameLevels::Three,
    catalog_noun: "catalog",
    cloud: true,
    formatter: "spark",
    syntax: SqlSyntax { backslash_escapes: true, backslash_in_double_quotes: true, backtick: true, ..STANDARD_SYNTAX },
    load: Some(LoadTraits {
        types: TypeNames { decimal: "DECIMAL(38, 10)", binary: "BINARY", text: "STRING", ..STANDARD_TYPES },
        max_bytes: 16 * MB,
        bytes_note: "the warehouse rejects requests past its message size; lower this if large inserts fail",
        binary_literal: "unhex('{}')",
        text_literal: TextLiteral::BackslashEscaped,
        stage: StageTable::Regular,
        merge: MergeStyle::Merge,
        transactions: None,
        ..STANDARD_LOAD
    }),
    ..STANDARD
};

pub const BIGQUERY: SourceTraits = SourceTraits {
    parser: SqlParser::Bigquery,
    whole_path_quotes: true,
    always_qualify_catalog: true,
    network: NetworkTarget::Unknown,
    name: "BigQuery",
    dialect_name: "BigQuery GoogleSQL",
    quote: IdentQuote::BacktickBackslash,
    levels: NameLevels::Three,
    catalog_noun: "project",
    cloud: true,
    formatter: "bigquery",
    syntax: SqlSyntax {
        backslash_escapes: true,
        backslash_in_double_quotes: true,
        backtick: true,
        hash_comment: true,
        triple_quotes: true,
        block_end: BlockEnd::EndWord,
        block_objects: &["PROCEDURE"],
        ..STANDARD_SYNTAX
    },
    load: Some(LoadTraits {
        types: TypeNames {
            boolean: "BOOL",
            integer: "INT64",
            float: "FLOAT64",
            decimal: "BIGNUMERIC",
            timestamp: "DATETIME",
            timestamp_tz: "TIMESTAMP",
            binary: "BYTES",
            text: "STRING",
            ..STANDARD_TYPES
        },
        default_bytes: 1_000_000,
        max_bytes: 1_000_000,
        bytes_note: "BigQuery limits the query text to 1,024 KB",
        binary_literal: "FROM_HEX('{}')",
        text_literal: TextLiteral::BackslashEscaped,
        probe_limit0: true,
        stage: StageTable::RegularLimit0,
        merge: MergeStyle::MergeNoInto,
        transactions: None,
        ..STANDARD_LOAD
    }),
    ..STANDARD
};

pub const DUCKDB: SourceTraits = SourceTraits {
    parser: SqlParser::Duckdb,
    network: NetworkTarget::Unknown,
    name: "DuckDB",
    dialect_name: "DuckDB SQL (read local files with read_csv/read_parquet/read_json/delta_scan/iceberg_scan)",
    levels: NameLevels::Three,
    explain: Some("EXPLAIN "),
    local: true,
    formatter: "duckdb",
    syntax: SqlSyntax { dollar: DollarQuotes::Tagged, ..STANDARD_SYNTAX },
    load: Some(LoadTraits { types: TypeNames { timestamp_tz: "TIMESTAMPTZ", ..STANDARD_TYPES }, ..STANDARD_LOAD }),
    ..STANDARD
};

/// MongoDB: databases (shown as schemas) → collections (tables). Queries are mongosh-style
/// (`db.orders.find({…})`) or JSON commands (`{"find": "orders"}`).
pub const MONGODB: SourceTraits = SourceTraits {
    name: "MongoDB",
    language: QueryLanguage::MongoJson,
    table_folder: "Collections",
    preview: Preview::MongoFind,
    dialect_name: "MongoDB query (mongosh-style db.<collection>.find/aggregate/countDocuments/distinct/insertOne/updateMany/deleteMany…, or a JSON database command; `use <db>` on the first line picks the database)",
    catalog_noun: "database",
    explain: None,
    network: NetworkTarget::HostPort,
    formatter: "",
    syntax: SqlSyntax { colon_params: false, ..STANDARD_SYNTAX },
    load: None,
    ..STANDARD
};

/// Neo4j: one database; "schemas" are Nodes and Relationships, objects are
/// labels and relationship types. Queries are Cypher.
pub const NEO4J: SourceTraits = SourceTraits {
    name: "Neo4j",
    language: QueryLanguage::Cypher,
    table_folder: "Labels",
    view_folder: "Relationship types",
    preview: Preview::Cypher,
    dialect_name: "Neo4j Cypher (MATCH (n:Label) … RETURN …; labels and relationship types are the tables)",
    quote: IdentQuote::Backtick,
    catalog_noun: "database",
    explain: Some("EXPLAIN "),
    network: NetworkTarget::HostPort,
    formatter: "",
    syntax: SqlSyntax { backtick: true, slash_comment: true, colon_params: false, ..STANDARD_SYNTAX },
    load: None,
    ..STANDARD
};

impl ConnectorKind {
    /// What this source type is like (see [`SourceTraits`]).
    pub fn traits(self) -> &'static SourceTraits {
        match self {
            ConnectorKind::Sqlite => &SQLITE,
            ConnectorKind::Postgres => &POSTGRES,
            ConnectorKind::Mysql => &MYSQL,
            ConnectorKind::Mssql => &MSSQL,
            ConnectorKind::Oracle => &ORACLE,
            ConnectorKind::Snowflake => &SNOWFLAKE,
            ConnectorKind::Databricks => &DATABRICKS,
            ConnectorKind::Bigquery => &BIGQUERY,
            ConnectorKind::Duckdb => &DUCKDB,
            ConnectorKind::Mongodb => &MONGODB,
            ConnectorKind::Neo4j => &NEO4J,
        }
    }
}

/// The query that shows the first `n` rows of `schema.name`.
pub fn preview_query(kind: ConnectorKind, schema: &str, name: &str, n: usize) -> String {
    let t = kind.traits();
    match t.preview {
        Preview::MongoFind => {
            let coll = serde_json::to_string(name).unwrap_or_default();
            let db = serde_json::to_string(schema).unwrap_or_default();
            format!("db.getSiblingDB({db}).getCollection({coll}).find({{}}).limit({n})")
        }
        Preview::Cypher => {
            let label = crate::quote_ident(kind, name);
            if schema.eq_ignore_ascii_case(NEO4J_RELATIONSHIPS) {
                format!("MATCH (a)-[r:{label}]->(b) RETURN a, r, b LIMIT {n}")
            } else {
                format!("MATCH (n:{label}) RETURN n LIMIT {n}")
            }
        }
        Preview::Sql => {
            let full = format!("{}.{}", crate::quote_path(kind, schema), crate::quote_ident(kind, name));
            match t.row_limit {
                RowLimit::Top => format!("SELECT TOP {n} * FROM {full}"),
                RowLimit::FetchFirst => format!("SELECT * FROM {full} FETCH FIRST {n} ROWS ONLY"),
                RowLimit::Limit => format!("SELECT * FROM {full} LIMIT {n}"),
            }
        }
    }
}

/// Neo4j's explorer "schemas".
pub const NEO4J_NODES: &str = "Nodes";
pub const NEO4J_RELATIONSHIPS: &str = "Relationships";

#[cfg(test)]
mod tests {
    use super::*;

    /// The UI's copy (`ui/src/lib/sourceTraits.json`, used by its pure
    /// helpers and tests; the app replaces it with the backend's at start)
    /// must match this table. `UPDATE_SOURCE_TRAITS=1` rewrites it.
    #[test]
    fn ui_copy_is_up_to_date() {
        let map: serde_json::Map<String, serde_json::Value> =
            ConnectorKind::ALL.iter().map(|k| (k.as_str().to_string(), serde_json::to_value(k.traits()).unwrap())).collect();
        let text = serde_json::to_string_pretty(&map).unwrap() + "\n";
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../ui/src/lib/sourceTraits.json");
        if std::env::var_os("UPDATE_SOURCE_TRAITS").is_some() {
            std::fs::write(&path, &text).unwrap();
        }
        // Compared as JSON: key order depends on serde_json features the
        // workspace enables (`preserve_order`), not on the content.
        let on_disk: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(&path).unwrap_or_default()).unwrap_or_default();
        assert!(on_disk == serde_json::Value::Object(map), "ui/src/lib/sourceTraits.json is out of date: run UPDATE_SOURCE_TRAITS=1 cargo test -p databrain-connector-core traits");
    }

    #[test]
    fn preview_queries() {
        assert_eq!(preview_query(ConnectorKind::Postgres, "public", "t", 5), "SELECT * FROM \"public\".\"t\" LIMIT 5");
        assert_eq!(preview_query(ConnectorKind::Mssql, "dbo", "t", 5), "SELECT TOP 5 * FROM [dbo].[t]");
        assert_eq!(preview_query(ConnectorKind::Mongodb, "shop", "orders", 5), "db.getSiblingDB(\"shop\").getCollection(\"orders\").find({}).limit(5)");
        assert_eq!(preview_query(ConnectorKind::Neo4j, "Nodes", "Person", 5), "MATCH (n:`Person`) RETURN n LIMIT 5");
        assert_eq!(preview_query(ConnectorKind::Neo4j, "Relationships", "ACTED_IN", 5), "MATCH (a)-[r:`ACTED_IN`]->(b) RETURN a, r, b LIMIT 5");
    }

    #[test]
    fn every_kind_has_consistent_traits() {
        for k in ConnectorKind::ALL {
            let t = k.traits();
            assert!(!t.dialect_name.is_empty(), "{k:?}");
            assert!(!(t.cloud && t.local), "{k:?}");
            if let Some(l) = &t.load {
                assert!(l.default_rows <= l.max_rows && l.default_bytes <= l.max_bytes, "{k:?}");
                assert!(l.binary_literal.contains("{}"), "{k:?}");
            }
            if t.syntax.block_end == BlockEnd::None {
                assert!(t.syntax.block_objects.is_empty() && t.syntax.block_starts.is_empty(), "{k:?}");
            }
        }
        // Serialisable for the UI.
        let v = serde_json::to_value(ConnectorKind::Mssql.traits()).unwrap();
        assert_eq!(v["levels"], "three_without_database");
        assert_eq!(v["quote"], "bracket");
        assert_eq!(v["language"], "sql");
    }
}
