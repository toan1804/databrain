use serde::{Deserialize, Serialize};

use std::collections::BTreeMap;

use super::{JobEdge, JobNode, JobSchedule, NodeRunSummary, RunLimits};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, ts_rs::TS)]
#[ts(export)]
#[ts(optional_fields = nullable)]
pub struct Job {
    #[serde(default)]
    pub id: String,
    pub name: String,
    #[serde(default)]
    pub nodes: Vec<JobNode>,
    #[serde(default)]
    pub edges: Vec<JobEdge>,
    #[serde(default)]
    pub schedule: JobSchedule,
    /// When the scheduler last started it (epoch ms).
    #[serde(default)]
    #[ts(as = "Option<f64>")]
    pub last_scheduled_at: Option<i64>,
    /// Values of the steps' `:name` parameters (over those of `param_set`).
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    #[ts(as = "Option<BTreeMap<String, databrain_connector_core::sql::ParamValue>>", optional)]
    pub params: BTreeMap<String, databrain_connector_core::sql::ParamValue>,
    /// Saved parameter set (id) whose values the steps get.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub param_set: Option<String>,
    /// Time limits, retries and steps at once of its runs.
    #[serde(default, skip_serializing_if = "RunLimits::is_default")]
    #[ts(as = "Option<RunLimits>", optional)]
    pub limits: RunLimits,
    #[serde(default)]
    #[ts(as = "f64")]
    pub created_at: i64,
    #[serde(default)]
    #[ts(as = "f64")]
    pub updated_at: i64,
}

impl Job {
    /// Time the next scheduled run is counted from: the last scheduled
    /// start, or the last edit (turning a schedule on doesn't run at once).
    /// Never later than now: a mark made while the computer's clock was
    /// ahead (then corrected) would hold the schedule until real time
    /// caught up, possibly years.
    pub fn schedule_base(&self) -> i64 {
        self.schedule_base_at(crate::now_ms())
    }

    /// [`Self::schedule_base`] as of `now` (ms).
    pub fn schedule_base_at(&self, now: i64) -> i64 {
        self.last_scheduled_at.unwrap_or(self.updated_at).max(self.updated_at).min(now)
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, ts_rs::TS)]
#[ts(export)]
#[ts(optional_fields = nullable)]
pub struct JobSummary {
    pub id: String,
    pub name: String,
    pub node_count: usize,
    /// Output names its steps own.
    pub step_names: Vec<String>,
    pub schedule: JobSchedule,
    #[ts(as = "Option<f64>")]
    pub next_run_at: Option<i64>,
    pub last_run: Option<JobRun>,
    #[ts(as = "f64")]
    pub updated_at: i64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, ts_rs::TS)]
#[ts(export)]
pub struct NodeRunRecord {
    pub node_id: String,
    pub name: String,
    #[serde(flatten)]
    pub summary: NodeRunSummary,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, ts_rs::TS)]
#[ts(export)]
#[ts(optional_fields = nullable)]
pub struct JobRun {
    #[ts(as = "f64")]
    pub id: i64,
    pub job_id: String,
    /// "manual", "schedule" or "ai" (the assistant, after approval), …, or
    /// "after:<job name>" (started when that job succeeded).
    #[ts(type = "\"manual\" | \"schedule\" | \"ai\" | \"resume\" | \"background\" | \"cli\" | `after:${string}`")]
    pub trigger: String,
    #[ts(as = "f64")]
    pub started_at: i64,
    #[ts(as = "Option<f64>")]
    pub finished_at: Option<i64>,
    /// "running", "success", "error", "cancelled".
    #[ts(type = "\"running\" | \"success\" | \"error\" | \"cancelled\"")]
    pub status: String,
    pub nodes: Vec<NodeRunRecord>,
    pub error: Option<String>,
    /// `:name` values the steps got (a resume runs with the same).
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    #[ts(as = "Option<BTreeMap<String, databrain_connector_core::sql::ParamValue>>", optional)]
    pub params: BTreeMap<String, databrain_connector_core::sql::ParamValue>,
    /// Every step runs under the AI rules (`Origin::Ai`) though the trigger
    /// isn't `ai`: a "run after" start from a run the assistant started
    /// (or from such a run, in turn).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    #[ts(as = "Option<bool>", optional)]
    pub ai: bool,
}

/// Output names are SQL identifiers (`results.<name>`), same rules as
/// other outputs: at most 63 characters, no `__` (versions, `x__1`), not a
/// handle (`r12`).
pub fn valid_node_name(name: &str) -> bool {
    let mut chars = name.chars();
    let ident = matches!(chars.next(), Some(c) if c.is_ascii_alphabetic() || c == '_') && chars.all(|c| c.is_ascii_alphanumeric() || c == '_');
    let handle = name.len() > 1 && name.starts_with(['r', 'R']) && name[1..].chars().all(|c| c.is_ascii_digit());
    ident && name.len() <= 63 && !name.contains("__") && !handle
}

/// The job step that owns an output name: only that step's runs may
/// produce `results.<name>`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StepOwner {
    pub job_id: String,
    pub job_name: String,
    pub node_id: String,
    pub step: String,
}

impl StepOwner {
    /// Run key of the step (`RunRequest.tab_id` of its runs).
    pub fn tab(&self) -> String {
        crate::SessionRole::JobStep { job: self.job_id.clone(), node: self.node_id.clone() }.key()
    }
    pub fn describe(&self) -> String {
        format!("step \"{}\" of job \"{}\"", self.step, self.job_name)
    }
}

/// Nodes in run order (each after its upstreams), or the nodes of a cycle.
pub fn topo_order(job: &Job) -> std::result::Result<Vec<String>, Vec<String>> {
    let ids: Vec<&str> = job.nodes.iter().map(|n| n.id.as_str()).collect();
    let mut indeg: std::collections::HashMap<&str, usize> = ids.iter().map(|i| (*i, 0)).collect();
    for e in &job.edges {
        if let Some(d) = indeg.get_mut(e.to.as_str())
            && ids.contains(&e.from.as_str())
        {
            *d += 1;
        }
    }
    let mut ready: Vec<&str> = ids.iter().copied().filter(|i| indeg[i] == 0).collect();
    let mut out = Vec::new();
    while let Some(n) = (!ready.is_empty()).then(|| ready.remove(0)) {
        out.push(n.to_string());
        for e in job.edges.iter().filter(|e| e.from == n) {
            if let Some(d) = indeg.get_mut(e.to.as_str()) {
                *d -= 1;
                if *d == 0 {
                    ready.push(e.to.as_str());
                }
            }
        }
    }
    if out.len() == ids.len() { Ok(out) } else { Err(ids.iter().filter(|i| !out.iter().any(|o| o == *i)).map(|s| s.to_string()).collect()) }
}

/// An incremental step's mark ([`JobNode::incremental_column`]).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, ts_rs::TS)]
#[ts(export)]
pub struct Watermark {
    /// The column it was taken from.
    pub column: String,
    /// Its largest value then, as the grid shows it.
    pub value: String,
    /// The column's type, which the value is written back into SQL as
    /// (`text`, `integer`, `timestamptz`…: see
    /// `databrain_connector_core::sql::MarkType`); `None` for marks without
    /// one (written as a parameter is).
    #[serde(default)]
    #[ts(optional)]
    pub value_type: Option<String>,
    /// The run that set it.
    #[ts(as = "Option<f64>")]
    pub run_id: Option<i64>,
    #[ts(as = "f64")]
    pub updated_at: i64,
}

/// A mark an incremental step took while a step after it didn't succeed
/// ([`crate::Workspace::job_watermark_pending`]).
#[derive(Debug, Clone, PartialEq)]
pub struct PendingMark {
    pub column: String,
    pub value: String,
    pub value_type: Option<String>,
    /// The run that took it.
    pub run_id: Option<i64>,
    /// When the step took it (its rows were read then): a step after it
    /// that succeeded since has those rows.
    pub taken_at: i64,
}
