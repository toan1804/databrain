//! Timings of DataBrain's critical paths: what runs on every keystroke,
//! every statement, every result page, every export. See docs/PERFORMANCE.md.
//!
//! ```text
//! cargo run --release -p databrain-bench                 # all, compared with the baseline
//! cargo run --release -p databrain-bench -- redact       # only names containing "redact"
//! cargo run --release -p databrain-bench -- --check      # fail when one is over its budget
//! cargo run --release -p databrain-bench -- --save       # write the baseline (at a release)
//! ```

mod harness;
mod paths;

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::time::Duration;

use harness::{Baseline, Suite, human, machine, measure, rate};

struct Args {
    filter: Vec<String>,
    quick: bool,
    check: bool,
    save: bool,
    list: bool,
    baseline: PathBuf,
    /// Fail when a path is this many times slower than the baseline.
    max_slowdown: Option<f64>,
}

fn args() -> Result<Args, String> {
    let mut a = Args {
        filter: Vec::new(),
        quick: false,
        check: false,
        save: false,
        list: false,
        baseline: PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("baseline.json"),
        max_slowdown: None,
    };
    let mut it = std::env::args().skip(1);
    while let Some(arg) = it.next() {
        match arg.as_str() {
            "--quick" => a.quick = true,
            "--check" => a.check = true,
            "--save" => a.save = true,
            "--list" => a.list = true,
            "--baseline" => a.baseline = it.next().ok_or("--baseline needs a file")?.into(),
            "--max-slowdown" => a.max_slowdown = Some(it.next().and_then(|v| v.parse().ok()).ok_or("--max-slowdown needs a number (1.5 = 50% slower)")?),
            "-h" | "--help" => {
                return Err("usage: databrain-bench [name parts…] [--quick] [--check] [--save] [--list] [--baseline FILE] [--max-slowdown N]".into());
            }
            s if s.starts_with('-') => return Err(format!("unknown option {s} (see --help)")),
            s => a.filter.push(s.to_string()),
        }
    }
    Ok(a)
}

fn main() -> std::process::ExitCode {
    let args = match args() {
        Ok(a) => a,
        Err(e) => {
            eprintln!("{e}");
            return std::process::ExitCode::from(2);
        }
    };
    if cfg!(debug_assertions) {
        eprintln!("warning: a debug build; its timings say nothing about the app. Run with --release.\n");
    }
    // The engine paths start tasks on this runtime.
    let rt = tokio::runtime::Builder::new_multi_thread().worker_threads(2).enable_time().build().expect("runtime");
    let _in_rt = rt.enter();

    let mut suite = Suite::default();
    paths::register(&mut suite);
    suite.benches.retain(|b| args.filter.is_empty() || args.filter.iter().any(|f| b.id.contains(f.as_str())));
    if args.list {
        for b in &suite.benches {
            println!("{:<34} {}", b.id, b.what);
        }
        for f in &suite.temp_files {
            let _ = std::fs::remove_file(f);
        }
        return std::process::ExitCode::SUCCESS;
    }

    let baseline = Baseline::read(&args.baseline);
    let same_machine = baseline.as_ref().is_some_and(|b| b.machine == machine());
    println!("DataBrain {} on {}", env!("CARGO_PKG_VERSION"), machine());
    match &baseline {
        Some(b) if same_machine => println!("baseline: {} (version {})", args.baseline.display(), b.version),
        Some(b) => println!("baseline: from another machine ({}), not compared", b.machine),
        None => println!("baseline: none at {}", args.baseline.display()),
    }
    println!("\n{:<34} {:>10} {:>20} {:>10} {:>9}", "path", "time", "rate", "budget", "baseline");

    let target = Duration::from_millis(if args.quick { 100 } else { 500 });
    let mut ns = BTreeMap::new();
    let (mut over, mut slower) = (Vec::new(), Vec::new());
    let mut group = String::new();
    for b in &mut suite.benches {
        let g = b.id.split('/').next().unwrap_or("").to_string();
        if g != group {
            println!();
            group = g;
        }
        let d = measure(b, target);
        ns.insert(b.id.clone(), d.as_nanos() as u64);
        let ratio = baseline.as_ref().filter(|_| same_machine).and_then(|bl| bl.ns.get(&b.id)).map(|old| d.as_nanos() as f64 / (*old).max(1) as f64);
        let mut flag = String::new();
        if d > b.budget {
            over.push(b.id.clone());
            flag.push_str("  OVER BUDGET");
        }
        if let (Some(r), Some(max)) = (ratio, args.max_slowdown)
            && r > max
        {
            slower.push(b.id.clone());
            flag.push_str("  SLOWER");
        }
        println!(
            "{:<34} {:>10} {:>20} {:>10} {:>9}{flag}",
            b.id,
            human(d),
            b.per.map(|(n, unit)| rate(n, unit, d)).unwrap_or_default(),
            human(b.budget),
            ratio.map(|r| format!("{r:.2}x")).unwrap_or_else(|| "-".into()),
        );
    }

    drop(std::mem::take(&mut suite.benches));
    for f in &suite.temp_files {
        let _ = std::fs::remove_file(f);
    }

    if args.save {
        // A partial run keeps the other paths' numbers.
        let mut all = baseline.filter(|_| same_machine && !args.filter.is_empty()).map(|b| b.ns).unwrap_or_default();
        all.extend(ns);
        match Baseline::write(&args.baseline, &all) {
            Ok(()) => println!("\nbaseline written: {}", args.baseline.display()),
            Err(e) => {
                eprintln!("\ncan't write {}: {e}", args.baseline.display());
                return std::process::ExitCode::from(2);
            }
        }
    }
    if !slower.is_empty() {
        println!("\nslower than the baseline allows: {}", slower.join(", "));
    }
    if !over.is_empty() {
        println!("\nover budget: {}", over.join(", "));
    }
    if (args.check && !over.is_empty()) || !slower.is_empty() {
        return std::process::ExitCode::FAILURE;
    }
    std::process::ExitCode::SUCCESS
}
