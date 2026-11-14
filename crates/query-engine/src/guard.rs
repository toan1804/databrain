//! One entry point for work on a connection: [`QueryEngine::guarded`] reads
//! the connection, applies the read-only and production rules for what the
//! caller is about to do ([`Intent`]), and only then claims the session.
//! Runs, loads, grid edits, plans, exports and Commit/Rollback all start
//! here, so a new entry point can't skip a check by forgetting it: it has
//! to name its intent.

use super::*;
use crate::sessions::Claim;

/// What an entry point is about to do on a connection.
pub(crate) enum Intent<'p> {
    /// Run these statements (the editor, the assistant, job steps).
    /// `confirmed`: the user agreed to the prompt (or the caller enforces
    /// its own policy, as the assistant does).
    Run { plan: &'p [PlannedStatement], confirmed: bool },
    /// Write rows into `table` (loads). A dry run keeps nothing, so it
    /// doesn't ask on production.
    Load { table: &'p str, confirmed: bool, dry: bool },
    /// Save grid edits (the user reviewed the statements before saving).
    Edit,
    /// Only read: a plan, a query run again for an export or a load's
    /// rows. The caller has checked that the statement reads.
    Inspect,
    /// Commit or Rollback what the session has open.
    EndTransaction,
}

/// Hold the session while the work runs (see [`QueryEngine::claim`]).
pub(crate) struct Hold<'k> {
    pub session_key: &'k str,
    pub cancel: CancellationToken,
    /// The message when something already holds it.
    pub busy: &'k str,
    /// A run: its run key and job id (the entry its task ends).
    pub run: Option<(&'k str, String)>,
}

impl<'k> Hold<'k> {
    pub(crate) fn session(session_key: &'k str, cancel: CancellationToken, busy: &'k str) -> Self {
        Self { session_key, cancel, busy, run: None }
    }
}

/// What [`QueryEngine::guarded`] decided.
pub(crate) enum Guarded<'e> {
    /// Go ahead: the connection, and the session held when asked to.
    Go { profile: Box<ConnectionProfile>, claim: Option<Claim<'e>> },
    /// Nothing ran: the user must confirm these first.
    Ask { reasons: Vec<String> },
}

impl QueryEngine {
    /// Whether `profile` allows `intent`: an error when it never does (a
    /// read-only connection), the reasons to confirm first (production,
    /// a write without WHERE), or none.
    pub(crate) fn check_intent(profile: &ConnectionProfile, intent: &Intent) -> Result<Vec<String>> {
        let read_only = profile.config.read_only;
        match intent {
            Intent::Run { plan, confirmed } => {
                if read_only {
                    // Only reads (and harmless USE/SET): a transaction or session
                    // statement could turn read-only off for what follows.
                    if let Some(p) = plan.iter().find(|p| !databrain_connector_core::sql::read_only_allows(&p.sql, &p.classification)) {
                        let why = if p.classification.kind.is_write() {
                            ""
                        } else if p.classification.kind == StatementKind::Unknown {
                            ": DataBrain can't tell that it only reads (it doesn't parse for this database, or part of it may run as a statement of its own)"
                        } else {
                            ": on a read-only connection only queries run (transactions and session settings could make it writable)"
                        };
                        return Err(EngineError::new(
                            "invalid",
                            format!("Connection \"{}\" is read-only; statement {} ({}) was not run{why}.", profile.name, p.index + 1, p.classification.keyword),
                        ));
                    }
                }
                Ok(if *confirmed { Vec::new() } else { Self::safety_reasons(profile, plan) })
            }
            Intent::Load { table, confirmed, dry } => {
                if read_only {
                    return Err(EngineError::new("invalid", format!("Connection \"{}\" is read-only; nothing was written.", profile.name)));
                }
                if !dry && !confirmed && profile.env == EnvTag::Prod {
                    return Ok(vec![format!("\"{}\" is a production connection: confirm writing rows into {table}.", profile.name)]);
                }
                Ok(Vec::new())
            }
            Intent::Edit => {
                if read_only {
                    return Err(EngineError::new("invalid", "The connection is read-only"));
                }
                Ok(Vec::new())
            }
            Intent::Inspect | Intent::EndTransaction => Ok(Vec::new()),
        }
    }

    /// The single way to start work on a connection: read it, check
    /// `intent` against it ([`Self::check_intent`]), then hold the session
    /// when `hold` says so. Nothing is claimed when the work is refused or
    /// must be confirmed first.
    pub(crate) fn guarded(&self, connection_id: &str, hold: Option<Hold<'_>>, intent: Intent) -> Result<Guarded<'_>> {
        let profile = self.workspace.get_connection(connection_id)?;
        let reasons = Self::check_intent(&profile, &intent)?;
        if !reasons.is_empty() {
            return Ok(Guarded::Ask { reasons });
        }
        let claim = match hold {
            Some(Hold { session_key, cancel, busy, run: Some((tab_id, id)) }) => Some(self.claim_as(connection_id, tab_id, session_key, id, cancel, busy)?),
            Some(Hold { session_key, cancel, busy, run: None }) => Some(self.claim(connection_id, session_key, cancel, busy)?),
            None => None,
        };
        Ok(Guarded::Go { profile: Box::new(profile), claim })
    }

    /// [`Self::guarded`] for callers with nobody to ask: a confirmation
    /// needed is an error (code `needs_confirmation`, the reasons as message).
    pub(crate) fn guarded_or_refuse(&self, connection_id: &str, hold: Option<Hold<'_>>, intent: Intent) -> Result<(ConnectionProfile, Option<Claim<'_>>)> {
        match self.guarded(connection_id, hold, intent)? {
            Guarded::Go { profile, claim } => Ok((*profile, claim)),
            Guarded::Ask { reasons } => Err(EngineError { code: Some("needs_confirmation".into()), ..EngineError::new("invalid", reasons.join(" ")) }),
        }
    }
}
