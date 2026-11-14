//! What a database session is for. The query engine keys sessions (and
//! runs) by a string: a query tab's id, or a key for the app's own work.
//! [`SessionRole`] names those keys, so they're built and read in one place,
//! and what happens to a session (closing it when idle, Refresh, the
//! assistant's sandbox, cleanup) is decided by its role, not by matching
//! strings.

/// What an assistant chat runs, each on a session of its own.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum AiPart {
    /// `run_query`: SQL on the chat's connection.
    Query,
    /// `query_outputs`: SQL over `results.<name>` (DuckDB).
    Outputs,
    /// Sample rows of a table.
    Sample,
}

impl AiPart {
    pub const ALL: [AiPart; 3] = [AiPart::Query, AiPart::Outputs, AiPart::Sample];

    fn prefix(self) -> &'static str {
        match self {
            AiPart::Query => "ai:",
            AiPart::Outputs => "ai-outputs:",
            AiPart::Sample => "ai-sample:",
        }
    }
}

/// The role of a session key.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum SessionRole {
    /// A query tab (its id, as the UI made it).
    Tab(String),
    /// A notebook's shared session (`cell: None`), or one cell's run key:
    /// cells keep their own results and share the notebook's session.
    Notebook { notebook: String, cell: Option<String> },
    /// The explorer's catalog session (also completion and indexing on
    /// local files).
    Meta,
    /// AI indexing's own catalog session on a server database.
    Index,
    /// An assistant chat's sessions (`chat` is `mcp` for the MCP server).
    Ai { chat: String, part: AiPart },
    /// A job step's runs.
    JobStep { job: String, node: String },
    /// Writing rows into a table ("Load into…", a job's load step, or a
    /// dry run checking one), for `id`.
    Load(String),
    /// Running a query again straight to a file, for export `id`.
    Export(String),
    /// A job step's dry run (its own each time: two at once would share,
    /// and end, a transaction).
    DryRun(String),
    /// Generated test data for a table (`{connection}:{schema}.{table}`):
    /// owns the rows' output, runs no SQL.
    TestData(String),
    /// Tests that set up data.
    Tests,
}

const META: &str = "__meta__";
const INDEX: &str = "__index__";
const TESTS: &str = "__tests__";
const LOAD: &str = "__load__:";
const EXPORT: &str = "__export__:";
const DRY_RUN: &str = "dryrun:";
const TEST_DATA: &str = "__testdata__:";

impl SessionRole {
    /// The role of `key` (any key not made by the app is a tab's).
    pub fn parse(key: &str) -> SessionRole {
        let tail = |p: &str| key.strip_prefix(p).map(str::to_string);
        if key == META {
            return SessionRole::Meta;
        }
        if key == INDEX {
            return SessionRole::Index;
        }
        if key == TESTS {
            return SessionRole::Tests;
        }
        if let Some(id) = tail(LOAD) {
            return SessionRole::Load(id);
        }
        if let Some(id) = tail(EXPORT) {
            return SessionRole::Export(id);
        }
        if let Some(id) = tail(TEST_DATA) {
            return SessionRole::TestData(id);
        }
        if let Some(id) = tail(DRY_RUN) {
            return SessionRole::DryRun(id);
        }
        for part in AiPart::ALL {
            if let Some(chat) = tail(part.prefix()) {
                return SessionRole::Ai { chat, part };
            }
        }
        if let Some(rest) = tail("job:")
            && let Some((job, node)) = rest.split_once(':')
        {
            return SessionRole::JobStep { job: job.to_string(), node: node.to_string() };
        }
        if let Some(rest) = tail("nb:") {
            return match rest.split_once(':') {
                Some((notebook, cell)) => SessionRole::Notebook { notebook: notebook.to_string(), cell: Some(cell.to_string()) },
                None => SessionRole::Notebook { notebook: rest, cell: None },
            };
        }
        SessionRole::Tab(key.to_string())
    }

    /// The key of this role (what [`Self::parse`] reads back).
    pub fn key(&self) -> String {
        match self {
            SessionRole::Tab(id) => id.clone(),
            SessionRole::Notebook { notebook, cell: None } => format!("nb:{notebook}"),
            SessionRole::Notebook { notebook, cell: Some(c) } => format!("nb:{notebook}:{c}"),
            SessionRole::Meta => META.into(),
            SessionRole::Index => INDEX.into(),
            SessionRole::Ai { chat, part } => format!("{}{chat}", part.prefix()),
            SessionRole::JobStep { job, node } => format!("job:{job}:{node}"),
            SessionRole::Load(id) => format!("{LOAD}{id}"),
            SessionRole::Export(id) => format!("{EXPORT}{id}"),
            SessionRole::DryRun(id) => format!("{DRY_RUN}{id}"),
            SessionRole::TestData(id) => format!("{TEST_DATA}{id}"),
            SessionRole::Tests => TESTS.into(),
        }
    }

    /// Only reads the catalog: holds no state of the user's (temp tables,
    /// settings, a transaction), so it can be closed and opened again at
    /// any time (Refresh, the sandbox switch on a DuckDB file).
    pub fn catalog_only(&self) -> bool {
        matches!(self, SessionRole::Meta | SessionRole::Index)
    }

    /// Closed by its owner when its one task ends (a load, a dry run, an
    /// export): nothing comes back to it later.
    pub fn one_off(&self) -> bool {
        matches!(self, SessionRole::Load(_) | SessionRole::Export(_) | SessionRole::DryRun(_))
    }

    /// The assistant's (AI chat or MCP): SQL from it may run sandboxed.
    pub fn is_ai(&self) -> bool {
        matches!(self, SessionRole::Ai { .. })
    }
}

impl std::fmt::Display for SessionRole {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.key())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keys_read_back_as_their_role() {
        let roles = [
            SessionRole::Tab("2f1c7a9e-tab".into()),
            SessionRole::Notebook { notebook: "n1".into(), cell: None },
            SessionRole::Notebook { notebook: "n1".into(), cell: Some("c2".into()) },
            SessionRole::Meta,
            SessionRole::Index,
            SessionRole::Ai { chat: "s9".into(), part: AiPart::Query },
            SessionRole::Ai { chat: "s9".into(), part: AiPart::Outputs },
            SessionRole::Ai { chat: "mcp".into(), part: AiPart::Sample },
            SessionRole::DryRun("j1:n3:abc".into()),
            SessionRole::TestData("c1:main.users".into()),
            SessionRole::JobStep { job: "j1".into(), node: "n3".into() },
            SessionRole::Load("l1".into()),
            SessionRole::Load("job:j1:n3".into()),
            SessionRole::Export("e1".into()),
            SessionRole::Tests,
        ];
        for r in roles {
            assert_eq!(SessionRole::parse(&r.key()), r, "{}", r.key());
        }
        // Keys as the UI and older code wrote them.
        assert_eq!(SessionRole::parse("nb:n1:c2"), SessionRole::Notebook { notebook: "n1".into(), cell: Some("c2".into()) });
        assert_eq!(SessionRole::parse("job:j1:n3"), SessionRole::JobStep { job: "j1".into(), node: "n3".into() });
        // "job:" without a node is no job step's: a tab's own id.
        assert_eq!(SessionRole::parse("job:x"), SessionRole::Tab("job:x".into()));
    }

    #[test]
    fn roles_decide() {
        assert!(SessionRole::Meta.catalog_only() && SessionRole::Index.catalog_only());
        let ai = SessionRole::Ai { chat: "s".into(), part: AiPart::Query };
        assert!(!SessionRole::Tab("t".into()).catalog_only() && !ai.catalog_only());
        assert!(SessionRole::Load("l".into()).one_off() && SessionRole::Export("e".into()).one_off());
        assert!(!SessionRole::JobStep { job: "j".into(), node: "n".into() }.one_off());
        assert!(ai.is_ai() && !SessionRole::Tab("ai".into()).is_ai());
        assert!(SessionRole::DryRun("d".into()).one_off());
    }
}
