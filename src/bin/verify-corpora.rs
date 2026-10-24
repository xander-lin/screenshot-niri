//! Runs the acceptance corpora and compares them with the recorded baselines.
//!
//! Verification used to be a shell loop whose numbers were read by eye, which
//! is how stale baselines survived in the docs (`real-4-4k` read 447/0 in
//! `testdata/frames/README.md` long after it had moved to 438/2). This binary
//! owns the expectations: one command, every corpus, a non-zero exit on any
//! mismatch, and the failing run's output printed.
//!
//! It shells out to `replay-frames` (a sibling in the same target directory)
//! rather than linking the library: the corpus tools are the measurement, and
//! the runner must exercise exactly the binary a person would run. Corpora run
//! concurrently, but each `replay-frames` already saturates its own core budget,
//! so the default job count is deliberately small.
//!
//! Build first: `cargo build --release --bins`.

use std::process::Command;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Instant;

/// One corpus and what a correct run must report.
///
/// `no_match` is `None` where refusal counts are not a correctness signal
/// (`real-2`'s boundary moves with `REFUSAL_PATIENCE`; the 1.2x corpora refuse
/// structurally on their fractional step). Everything else is gated, because a
/// change there means a placement changed.
struct Expectation {
    dir: &'static str,
    accepted: u32,
    no_match: Option<u32>,
    misplaced: u32,
    bands: u32,
    uncovered: u32,
    /// `Some(n)` only for a corpus whose *point* is that n pairs are ambiguous
    /// (`pipeline-bad`); elsewhere the count is informational.
    ambiguous: Option<u32>,
    note: &'static str,
}

const BASELINES: &[Expectation] = &[
    Expectation {
        dir: "real-1",
        accepted: 204,
        no_match: Some(0),
        misplaced: 0,
        bands: 0,
        uncovered: 0,
        ambiguous: None,
        note: "the main acceptance corpus; jump-scroll input",
    },
    Expectation {
        dir: "real-2",
        accepted: 230,
        no_match: None,
        misplaced: 0,
        bands: 1,
        uncovered: 0,
        ambiguous: None,
        note: "blank areas + selection edits; the page itself changes",
    },
    Expectation {
        dir: "real-3",
        accepted: 65,
        no_match: Some(0),
        misplaced: 0,
        bands: 0,
        uncovered: 0,
        ambiguous: None,
        note: "thin evidence: an almost empty terminal",
    },
    Expectation {
        dir: "real-4-4k",
        accepted: 438,
        no_match: None,
        misplaced: 2,
        bands: 45,
        uncovered: 0,
        ambiguous: None,
        note: "1.2x fractional phase: bands and 2 far placements are the known state",
    },
    Expectation {
        dir: "real-5-4k",
        accepted: 542,
        no_match: Some(0),
        misplaced: 0,
        bands: 0,
        uncovered: 0,
        ambiguous: None,
        note: "three width changes on one 1.2x output",
    },
    Expectation {
        dir: "real-long",
        accepted: 97,
        no_match: Some(0),
        misplaced: 0,
        bands: 0,
        uncovered: 0,
        ambiguous: None,
        note: "CDP corpus the gates were calibrated on",
    },
    Expectation {
        dir: "real-scroll",
        accepted: 7,
        no_match: Some(0),
        misplaced: 0,
        bands: 0,
        uncovered: 0,
        ambiguous: None,
        note: "short CDP sequence",
    },
    Expectation {
        dir: "mini",
        accepted: 3,
        no_match: Some(0),
        misplaced: 0,
        bands: 0,
        uncovered: 0,
        ambiguous: None,
        note: "fast regression over three turning points",
    },
    Expectation {
        dir: "pipeline-test",
        accepted: 7,
        no_match: Some(0),
        misplaced: 0,
        bands: 0,
        uncovered: 0,
        ambiguous: None,
        note: "self-check: unique rows, zero misplacements",
    },
    Expectation {
        dir: "pipeline-bad",
        accepted: 7,
        no_match: Some(0),
        misplaced: 0,
        bands: 0,
        uncovered: 0,
        ambiguous: Some(7),
        note: "self-check: the tool must report the ambiguity",
    },
    Expectation {
        dir: "lowcontrast-2b",
        accepted: 78,
        no_match: Some(0),
        misplaced: 0,
        bands: 0,
        uncovered: 0,
        ambiguous: None,
        note: "2 grey levels; boundary direction and adaptive quantisation",
    },
];

/// The numbers a run reported, or why it could not be read.
struct Measured {
    accepted: u32,
    no_match: u32,
    misplaced: u32,
    bands: u32,
    uncovered: u32,
    ambiguous: u32,
    /// Kept for the report: a move here is a behaviour change even where the
    /// count is not a correctness gate.
    delta_outside_tie_set: u32,
}

fn number_after(stdout: &str, label: &str) -> Option<u32> {
    stdout.lines().find_map(|line| {
        let rest = line.trim_start().strip_prefix(label)?;
        if !rest.starts_with(' ') {
            return None;
        }
        rest.split_whitespace().next()?.parse().ok()
    })
}

fn parse(stdout: &str) -> Result<Measured, String> {
    let get = |label: &str| {
        number_after(stdout, label).ok_or_else(|| format!("no `{label}` line in the output"))
    };
    Ok(Measured {
        accepted: get("accepted")?,
        no_match: get("no-match")?,
        misplaced: get("MISPLACED")?,
        bands: get("frames with displaced bands")?,
        uncovered: get("UNCOVERED")?,
        ambiguous: get("ambiguous pairs")?,
        delta_outside_tie_set: get("delta outside the pixels' tie set")?,
    })
}

/// A corpus whose run finished, with the child's output kept for failures.
struct Run {
    dir: &'static str,
    measured: Result<Measured, String>,
    output: String,
    seconds: f64,
}

fn run_one(
    dir: &'static str,
    root: &std::path::Path,
    replay: &std::path::Path,
    extra: &[String],
) -> Run {
    let started = Instant::now();
    let result = Command::new(replay)
        .arg("--dir")
        .arg(root.join(dir))
        .args(extra)
        .output();
    let seconds = started.elapsed().as_secs_f64();
    match result {
        Ok(output) => {
            let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
            let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
            let measured = if output.status.success() {
                parse(&stdout).map_err(|error| format!("{error} (exit {})", output.status))
            } else {
                Err(format!("replay-frames exited with {}", output.status))
            };
            Run {
                dir,
                measured,
                output: format!("{stdout}{stderr}"),
                seconds,
            }
        }
        Err(error) => Run {
            dir,
            measured: Err(format!("could not run {replay:?}: {error}")),
            output: String::new(),
            seconds,
        },
    }
}

fn check(expectation: &Expectation, measured: &Measured) -> Vec<String> {
    let mut failures = Vec::new();
    let mut compare = |name: &str, expected: u32, actual: u32| {
        if expected != actual {
            failures.push(format!("{name}: expected {expected}, got {actual}"));
        }
    };
    compare("accepted", expectation.accepted, measured.accepted);
    if let Some(expected) = expectation.no_match {
        compare("no-match", expected, measured.no_match);
    }
    compare("MISPLACED", expectation.misplaced, measured.misplaced);
    compare("displaced bands", expectation.bands, measured.bands);
    compare("UNCOVERED", expectation.uncovered, measured.uncovered);
    if let Some(expected) = expectation.ambiguous {
        compare("ambiguous pairs", expected, measured.ambiguous);
    }
    failures
}

fn main() {
    let mut jobs = 2usize;
    let mut only: Option<String> = None;
    let mut root = std::path::PathBuf::from("testdata/frames");
    let mut extra: Vec<String> = Vec::new();
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--jobs" | "-j" => {
                jobs = args
                    .next()
                    .and_then(|value| value.parse().ok())
                    .unwrap_or(2)
                    .max(1);
            }
            "--only" => only = args.next(),
            "--root" => {
                if let Some(value) = args.next() {
                    root = std::path::PathBuf::from(value);
                }
            }
            "--exact" | "--refresh" => extra.push(arg),
            "--help" | "-h" => {
                println!(
                    "verify-corpora [--jobs N] [--only DIR] [--root DIR] [--exact] [--refresh]\n\n\
                     Runs replay-frames for every acceptance corpus and compares the\n\
                     result with the recorded baselines. --exact/--refresh are passed\n\
                     through. Build the bins first: cargo build --release --bins"
                );
                return;
            }
            other => {
                eprintln!("unknown argument `{other}` (try --help)");
                std::process::exit(2);
            }
        }
    }

    let exe = std::env::current_exe().expect("the runner's own path");
    let Some(replay) = exe.parent().map(|dir| dir.join("replay-frames")) else {
        eprintln!("could not locate the binary directory");
        std::process::exit(2);
    };
    if !replay.exists() {
        eprintln!(
            "{} is missing: build both binaries first (cargo build --release --bins)",
            replay.display()
        );
        std::process::exit(2);
    }
    // A stale replay binary would quietly verify the previous build.
    let built = std::fs::metadata(&replay)
        .and_then(|meta| meta.modified())
        .ok()
        .and_then(|time| time.elapsed().ok())
        .map(|age| format!("{:.0}m old", age.as_secs_f64() / 60.0))
        .unwrap_or_else(|| "unknown age".to_string());
    println!("replay: {} ({built})", replay.display());

    let selected: Vec<&Expectation> = BASELINES
        .iter()
        .filter(|expectation| {
            only
                .as_deref()
                .is_none_or(|needle| expectation.dir.contains(needle))
        })
        .collect();
    if selected.is_empty() {
        eprintln!("no corpus matches --only");
        std::process::exit(2);
    }

    let started = Instant::now();
    let next = Arc::new(AtomicUsize::new(0));
    let mut runs: Vec<Option<Run>> = (0..selected.len()).map(|_| None).collect();
    std::thread::scope(|scope| {
        let mut workers = Vec::new();
        for _ in 0..jobs.min(selected.len()) {
            let next = Arc::clone(&next);
            let replay = &replay;
            let root = &root;
            let extra = &extra;
            let selected = &selected;
            workers.push(scope.spawn(move || {
                let mut done = Vec::new();
                loop {
                    let index = next.fetch_add(1, Ordering::Relaxed);
                    if index >= selected.len() {
                        break;
                    }
                    let dir = selected[index].dir;
                    eprintln!("  running {dir} …");
                    done.push((index, run_one(dir, root, replay, extra)));
                }
                done
            }));
        }
        for worker in workers {
            for (index, run) in worker.join().expect("worker thread") {
                runs[index] = Some(run);
            }
        }
    });

    println!();
    println!(
        "{:<16} {:>10} {:>9} {:>9} {:>7} {:>9} {:>9}  verdict",
        "corpus", "accepted", "no-match", "MISPLACED", "bands", "UNCOVERED", "ambiguous"
    );
    let mut failures = 0usize;
    for (expectation, run) in selected.iter().zip(&runs) {
        let Some(run) = run else { continue };
        match &run.measured {
            Ok(measured) => {
                let problems = check(expectation, measured);
                let verdict = if problems.is_empty() {
                    "PASS".to_string()
                } else {
                    failures += 1;
                    problems.join("; ")
                };
                let shown = |expected: Option<u32>, actual: u32| match expected {
                    Some(expected) => format!("{actual} ({expected})"),
                    None => format!("{actual}"),
                };
                println!(
                    "{:<16} {:>10} {:>9} {:>9} {:>7} {:>9} {:>9}  {}  [{:.1}s]",
                    expectation.dir,
                    shown(Some(expectation.accepted), measured.accepted),
                    shown(expectation.no_match, measured.no_match),
                    shown(Some(expectation.misplaced), measured.misplaced),
                    shown(Some(expectation.bands), measured.bands),
                    shown(Some(expectation.uncovered), measured.uncovered),
                    shown(expectation.ambiguous, measured.ambiguous),
                    verdict,
                    run.seconds,
                );
                if problems.is_empty() && measured.delta_outside_tie_set > 0 {
                    println!(
                        "  {:16} note: {} delta(s) outside the pixels' tie set (diagnostic, not gated)",
                        "", measured.delta_outside_tie_set
                    );
                }
            }
            Err(error) => {
                failures += 1;
                println!("{:<16}  FAILED TO RUN: {error}", expectation.dir);
            }
        }
    }

    if failures > 0 {
        for (expectation, run) in selected.iter().zip(&runs) {
            let failed = run.as_ref().is_some_and(|run| match &run.measured {
                Ok(measured) => !check(expectation, measured).is_empty(),
                Err(_) => true,
            });
            if !failed {
                continue;
            }
            println!();
            println!("--- {} (showing the tail of its output) ---", expectation.dir);
            let lines: Vec<&str> = run.as_ref().unwrap().output.lines().collect();
            for line in lines.iter().skip(lines.len().saturating_sub(20)) {
                println!("{line}");
            }
        }
        println!();
        println!(
            "{failures} corpus/corpora differ from the baselines ({} checked, {:.1}s)",
            selected.len(),
            started.elapsed().as_secs_f64()
        );
        std::process::exit(1);
    }
    println!();
    println!(
        "all {} corpora match the baselines ({:.1}s)",
        selected.len(),
        started.elapsed().as_secs_f64()
    );
}
