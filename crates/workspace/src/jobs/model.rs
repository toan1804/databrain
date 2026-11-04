use serde::{Deserialize, Serialize};

use std::collections::BTreeMap;

use super::{JobEdge, JobNode, JobSchedule, NodeRunSummary};

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
    pub fn schedule_base(&self) -> i64 {
        self.last_scheduled_at.unwrap_or(self.updated_at).max(self.updated_at)
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
    /// "manual", "schedule" or "ai" (the assistant, after approval).
    #[ts(type = "\"manual\" | \"schedule\" | \"ai\" | \"resume\" | \"background\" | \"cli\"")]
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
        format!("job:{}:{}", self.job_id, self.node_id)
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
