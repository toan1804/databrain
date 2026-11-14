//! A small timing harness: no dependency, the same numbers on every run
//! of the same machine within a few percent, and a file to compare with.

use std::collections::BTreeMap;
use std::hint::black_box;
use std::time::{Duration, Instant};

/// One timed path.
pub struct Bench {
    /// `group/name`: the key in the baseline file.
    pub id: String,
    /// What a user is doing when this runs.
    pub what: &'static str,
    /// The most one iteration may take on any machine DataBrain runs on
    /// (`--check`): many times the usual, so only a real slowdown (a
    /// quadratic scan, a parse done a hundred times) fails it.
    pub budget: Duration,
    /// What one iteration handles, for a rate: (amount, unit).
    pub per: Option<(f64, &'static str)>,
    pub run: Box<dyn FnMut()>,
}

/// Registers benches; `group` is set for the ones that follow.
#[derive(Default)]
pub struct Suite {
    pub benches: Vec<Bench>,
    /// Files the paths made, removed when the run ends.
    pub temp_files: Vec<std::path::PathBuf>,
    group: &'static str,
}

impl Suite {
    pub fn group(&mut self, name: &'static str) {
        self.group = name;
    }

    /// A path timed as `group/name`; `f`'s result is kept from the optimizer.
    pub fn add<R>(&mut self, name: &str, what: &'static str, budget_ms: u64, per: Option<(f64, &'static str)>, mut f: impl FnMut() -> R + 'static) {
        self.benches.push(Bench {
            id: format!("{}/{name}", self.group),
            what,
            budget: Duration::from_millis(budget_ms),
            per,
            run: Box::new(move || {
                black_box(f());
            }),
        });
    }
}

/// The median time of one iteration: a warm-up run, then samples of at
/// least a millisecond each (fast paths run many times per sample) for
/// `target`, at least 5 and at most 200 of them.
pub fn measure(b: &mut Bench, target: Duration) -> Duration {
    let t = Instant::now();
    (b.run)();
    let once = t.elapsed().max(Duration::from_nanos(1));
    let per_sample = (Duration::from_millis(1).as_nanos() / once.as_nanos()).clamp(1, 100_000) as u32;
    let mut samples = Vec::new();
    let start = Instant::now();
    while samples.len() < 5 || (start.elapsed() < target && samples.len() < 200) {
        let t = Instant::now();
        for _ in 0..per_sample {
            (b.run)();
        }
        samples.push(t.elapsed() / per_sample);
    }
    samples.sort();
    samples[samples.len() / 2]
}

pub fn human(d: Duration) -> String {
    let ns = d.as_nanos() as f64;
    if ns < 1e3 {
        format!("{ns:.0} ns")
    } else if ns < 1e6 {
        format!("{:.1} µs", ns / 1e3)
    } else if ns < 1e9 {
        format!("{:.2} ms", ns / 1e6)
    } else {
        format!("{:.2} s", ns / 1e9)
    }
}

/// `1.2 M rows/s`, `340 K rows/s`, `12.0 MB/s` (for an amount in KB).
pub fn rate(amount: f64, unit: &str, d: Duration) -> String {
    let r = amount / d.as_secs_f64();
    if unit == "KB" {
        format!("{:.1} MB/s", r / 1024.0)
    } else if r >= 1e6 {
        format!("{:.1} M {unit}/s", r / 1e6)
    } else if r >= 1e3 {
        format!("{:.0} K {unit}/s", r / 1e3)
    } else {
        format!("{r:.0} {unit}/s")
    }
}

/// The machine the numbers are from: a baseline only compares with runs
/// on the same one.
pub fn machine() -> String {
    let cpus = std::thread::available_parallelism().map_or(0, |n| n.get());
    format!("{} {} ({cpus} cpus)", std::env::consts::OS, std::env::consts::ARCH)
}

/// A baseline file: `{"machine": …, "version": …, "ns": {"group/name": 123}}`.
pub struct Baseline {
    pub machine: String,
    pub version: String,
    pub ns: BTreeMap<String, u64>,
}

impl Baseline {
    pub fn read(path: &std::path::Path) -> Option<Self> {
        let v: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(path).ok()?).ok()?;
        Some(Self {
            machine: v["machine"].as_str()?.to_string(),
            version: v["version"].as_str().unwrap_or("").to_string(),
            ns: v["ns"].as_object()?.iter().filter_map(|(k, n)| Some((k.clone(), n.as_u64()?))).collect(),
        })
    }

    pub fn write(path: &std::path::Path, ns: &BTreeMap<String, u64>) -> std::io::Result<()> {
        let v = serde_json::json!({ "machine": machine(), "version": env!("CARGO_PKG_VERSION"), "ns": ns });
        std::fs::write(path, serde_json::to_string_pretty(&v).expect("json") + "\n")
    }
}
