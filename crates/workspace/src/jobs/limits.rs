//! Limits of a job's runs: how long a run and each step may take, how often
//! a step that lost its connection is tried again, and how many steps run
//! at once (in the job, and on one connection).

use serde::{Deserialize, Serialize};
use std::time::Duration;

/// Steps of a run running at once when the job doesn't say.
pub const DEFAULT_MAX_PARALLEL: u32 = 4;
/// Steps of a run on one connection at once when the job doesn't say
/// (each opens its own session on the server).
pub const DEFAULT_MAX_PER_CONNECTION: u32 = 2;
/// Most steps at once a job may allow, in all and on one connection (what
/// the editor accepts; a larger saved value is read as this).
pub const MAX_STEPS_AT_ONCE: u32 = 64;
/// Most retries a step may have.
pub const MAX_RETRIES: u32 = 10;
/// Wait before the first retry; each next one waits 4 times longer, up to
/// [`RETRY_DELAY_MAX`] times this (30 s, 2 min, 8 min, 30 min, …).
pub const RETRY_DELAY: Duration = Duration::from_secs(30);
const RETRY_DELAY_MAX: u32 = 60;

/// A job's run settings. Empty fields: no time limit, no retries, the
/// default parallelism. Steps can override the time limit and the retries
/// ([`StepLimits`]).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, ts_rs::TS)]
#[ts(export)]
#[ts(optional_fields = nullable)]
pub struct RunLimits {
    /// Most seconds a whole run may take; then it is stopped and fails.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub run_timeout_secs: Option<u32>,
    /// Most seconds one try of a step may take (steps without their own).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub step_timeout_secs: Option<u32>,
    /// Times a step is tried again after a connection error, when that
    /// can't write twice (steps without their own).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub retries: Option<u32>,
    /// Steps running at once (default [`DEFAULT_MAX_PARALLEL`]).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub max_parallel: Option<u32>,
    /// Steps running at once on one connection (default
    /// [`DEFAULT_MAX_PER_CONNECTION`]; the Results DuckDB isn't limited).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub max_per_connection: Option<u32>,
}

impl RunLimits {
    pub fn is_default(&self) -> bool {
        self == &Self::default()
    }
    pub fn run_timeout(&self) -> Option<Duration> {
        secs(self.run_timeout_secs)
    }
    pub fn max_parallel(&self) -> usize {
        steps_at_once(self.max_parallel, DEFAULT_MAX_PARALLEL)
    }
    pub fn max_per_connection(&self) -> usize {
        steps_at_once(self.max_per_connection, DEFAULT_MAX_PER_CONNECTION)
    }
}

/// A step's own time limit and retries, over the job's ([`RunLimits`]); 0
/// turns the job's off for this step.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, ts_rs::TS)]
#[ts(export)]
#[ts(optional_fields = nullable)]
pub struct StepLimits {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub timeout_secs: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub retries: Option<u32>,
}

impl StepLimits {
    pub fn is_default(&self) -> bool {
        self == &Self::default()
    }
    /// Time limit of one try of the step.
    pub fn timeout(&self, job: &RunLimits) -> Option<Duration> {
        secs(self.timeout_secs.or(job.step_timeout_secs))
    }
    /// Retries after a connection error.
    pub fn retries(&self, job: &RunLimits) -> u32 {
        self.retries.or(job.retries).unwrap_or(0).min(MAX_RETRIES)
    }
}

/// A steps-at-once limit: `default` when unset or 0, at most [`MAX_STEPS_AT_ONCE`].
fn steps_at_once(n: Option<u32>, default: u32) -> usize {
    n.filter(|n| *n > 0).unwrap_or(default).min(MAX_STEPS_AT_ONCE) as usize
}

fn secs(s: Option<u32>) -> Option<Duration> {
    s.filter(|s| *s > 0).map(|s| Duration::from_secs(s.into()))
}

/// Wait before retry `n` (from 1): `base`, then 4 times longer each time,
/// at most 60 × `base`.
pub fn retry_delay(base: Duration, n: u32) -> Duration {
    let factor = 4u32.saturating_pow(n.saturating_sub(1)).min(RETRY_DELAY_MAX);
    base * factor
}

/// "30 s", "2 min", "1 h 30 min".
pub fn duration_text(d: Duration) -> String {
    let s = d.as_secs();
    match s {
        0..60 => format!("{} s", s.max(1)),
        60..3600 if s.is_multiple_of(60) => format!("{} min", s / 60),
        60..3600 => format!("{} min {} s", s / 60, s % 60),
        _ if s.is_multiple_of(3600) => format!("{} h", s / 3600),
        _ => format!("{} h {} min", s / 3600, (s % 3600) / 60),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn steps_override_the_jobs_limits() {
        let job = RunLimits { step_timeout_secs: Some(60), retries: Some(2), ..Default::default() };
        assert_eq!(StepLimits::default().timeout(&job), Some(Duration::from_secs(60)));
        assert_eq!(StepLimits::default().retries(&job), 2);
        let own = StepLimits { timeout_secs: Some(5), retries: Some(1) };
        assert_eq!((own.timeout(&job), own.retries(&job)), (Some(Duration::from_secs(5)), 1));
        // 0 turns the job's off for the step.
        let off = StepLimits { timeout_secs: Some(0), retries: Some(0) };
        assert_eq!((off.timeout(&job), off.retries(&job)), (None, 0));
        assert_eq!(StepLimits { retries: Some(99), ..Default::default() }.retries(&job), MAX_RETRIES);
        // Nothing set: no limit, no retries, the default parallelism.
        let none = RunLimits::default();
        assert_eq!((none.run_timeout(), StepLimits::default().timeout(&none), StepLimits::default().retries(&none)), (None, None, 0));
        assert_eq!((none.max_parallel(), none.max_per_connection()), (4, 2));
        let zero = RunLimits { max_parallel: Some(0), max_per_connection: Some(0), run_timeout_secs: Some(0), ..Default::default() };
        assert_eq!((zero.max_parallel(), zero.max_per_connection(), zero.run_timeout()), (4, 2, None));
        // Past what the editor accepts (a hand-edited or old file): capped.
        let big = RunLimits { max_parallel: Some(u32::MAX), max_per_connection: Some(1000), ..Default::default() };
        assert_eq!((big.max_parallel(), big.max_per_connection()), (64, 64));
        let one = RunLimits { max_parallel: Some(1), max_per_connection: Some(64), ..Default::default() };
        assert_eq!((one.max_parallel(), one.max_per_connection()), (1, 64));
    }

    #[test]
    fn retries_wait_longer_each_time() {
        let d: Vec<u64> = (1..=6).map(|n| retry_delay(RETRY_DELAY, n).as_secs()).collect();
        assert_eq!(d, [30, 120, 480, 1800, 1800, 1800]);
        assert_eq!(retry_delay(RETRY_DELAY, 40).as_secs(), 1800);
    }

    #[test]
    fn durations_read_naturally() {
        assert_eq!(duration_text(Duration::from_secs(30)), "30 s");
        assert_eq!(duration_text(Duration::from_secs(1800)), "30 min");
        assert_eq!(duration_text(Duration::from_secs(90)), "1 min 30 s");
        assert_eq!(duration_text(Duration::from_secs(7200)), "2 h");
        assert_eq!(duration_text(Duration::from_secs(5400)), "1 h 30 min");
        assert_eq!(duration_text(Duration::from_millis(300)), "1 s");
    }

    #[test]
    fn old_jobs_load_without_limits() {
        let l: RunLimits = serde_json::from_str("{}").unwrap();
        assert!(l.is_default());
        assert_eq!(serde_json::to_string(&StepLimits::default()).unwrap(), "{}");
    }
}
