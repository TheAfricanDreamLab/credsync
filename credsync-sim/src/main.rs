//! The simulator's command line.
//!
//! ```sh
//! cargo run -p credsync-sim -- --seeds 1000        # a batch
//! cargo run -p credsync-sim -- --seed 0x4f21a9c3 --trace   # replay one, exactly
//! ```
//!
//! A bug report is one integer. When a batch fails, the seed it failed on is printed, and that
//! seed replays the run identically — on any machine, at any later date. If a printed seed ever
//! fails to reproduce, **that is a more serious bug than whatever was being chased**: determinism
//! has been lost, and every other result the simulator has ever produced is suspect.

use credsync_sim::{FaultRates, STEPS_PER_RUN, Trace, World};
use std::process::ExitCode;

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();

    if args.iter().any(|a| a == "--help" || a == "-h") {
        print_help();
        return ExitCode::SUCCESS;
    }

    let trace_wanted = args.iter().any(|a| a == "--trace");

    if let Some(seed) = flag_value(&args, "--seed").map(|v| parse_seed(&v)) {
        let Some(seed) = seed else {
            eprintln!("error: --seed expects a number, optionally 0x-prefixed");
            return ExitCode::FAILURE;
        };
        return run_one(seed, trace_wanted);
    }

    let seeds = flag_value(&args, "--seeds")
        .and_then(|v| v.parse::<u64>().ok())
        .unwrap_or(1_000);

    run_batch(seeds)
}

/// Runs one seed, optionally printing its trace.
fn run_one(seed: u64, trace_wanted: bool) -> ExitCode {
    let trace = if trace_wanted {
        Trace::recording()
    } else {
        Trace::counting()
    };
    let mut world = World::new(seed, FaultRates::default(), trace);
    world.run(STEPS_PER_RUN);

    if trace_wanted {
        println!("{}", world.trace.render());
    }

    println!(
        "seed 0x{seed:016x}  devices={}  simulated={}  {}",
        world.device_count(),
        human_duration(world.elapsed_ms()),
        world.trace.coverage()
    );

    if world.invariants.holds() {
        ExitCode::SUCCESS
    } else {
        report(seed, world.invariants.violations());
        ExitCode::FAILURE
    }
}

/// Prints a violation report, seed first.
///
/// The seed is the whole reproduction, so it goes at the top and again at the bottom as a
/// runnable command. Someone reading a CI log at speed should be able to copy one line.
fn report(seed: u64, violations: &[credsync_sim::Violation]) {
    eprintln!();
    eprintln!("FAILED at seed 0x{seed:016x}");
    for v in violations.iter().take(20) {
        eprintln!("  {v}");
    }
    if violations.len() > 20 {
        eprintln!("  ... and {} more", violations.len() - 20);
    }
    eprintln!();
    eprintln!("replay it exactly:");
    eprintln!("  cargo run --release -p credsync-sim -- --seed 0x{seed:x} --trace");
}

/// Runs a batch, reporting aggregate fault coverage.
///
/// The coverage line is not decoration. Design §11 is blunt that a simulator can look busy while
/// exploring almost nothing, so a fault showing zero occurrences across a thousand seeds is a
/// fault this rig does not actually have — and that is worth seeing on every run rather than
/// discovering during a coverage pass months later.
fn run_batch(seeds: u64) -> ExitCode {
    let started = std::time::Instant::now();
    let mut totals: std::collections::BTreeMap<&'static str, u64> =
        std::collections::BTreeMap::new();
    let mut simulated_ms: i64 = 0;

    for seed in 0..seeds {
        let mut world = World::new(seed, FaultRates::default(), Trace::counting());
        world.run(STEPS_PER_RUN);
        simulated_ms += world.elapsed_ms();

        for (name, n) in world.trace.faults() {
            *totals.entry(name).or_insert(0) += n;
        }

        // Stop at the first failure rather than pressing on. The seed is the reproduction, and a
        // batch that carried on would bury it under hundreds of later lines.
        if !world.invariants.holds() {
            println!("{}/{seeds} seeds green, then:", seed);
            report(seed, world.invariants.violations());
            return ExitCode::FAILURE;
        }
    }

    let wall = started.elapsed();
    println!("{seeds}/{seeds} seeds green in {wall:.2?}");
    println!(
        "simulated {} of device life across the batch",
        human_duration(simulated_ms)
    );

    println!("fault coverage:");
    let mut silent = Vec::new();
    for fault in credsync_sim::Fault::ALL {
        let n = totals.get(fault.name()).copied().unwrap_or(0);
        println!("  {:<32} {n}", fault.name());
        if n == 0 {
            silent.push(fault.name());
        }
    }

    if silent.is_empty() {
        ExitCode::SUCCESS
    } else {
        // Not a failure of the engine, but a failure of the rig, and the loudest thing on screen
        // ought to be the part that has stopped working.
        eprintln!();
        eprintln!(
            "warning: {} fault(s) never fired: {}",
            silent.len(),
            silent.join(", ")
        );
        eprintln!("a fault that never fires is a fault this simulator does not have.");
        ExitCode::SUCCESS
    }
}

/// The value after `flag`, if present.
fn flag_value(args: &[String], flag: &str) -> Option<String> {
    let i = args.iter().position(|a| a == flag)?;
    args.get(i + 1).cloned()
}

/// Parses a seed, decimal or `0x`-prefixed.
///
/// Both, because bug issues carry the hex form (`sim: divergence at seed 0x4f21a9c3`) while a
/// batch counts in decimal, and a reader should not have to convert between them by hand.
fn parse_seed(v: &str) -> Option<u64> {
    v.strip_prefix("0x")
        .map_or_else(|| v.parse().ok(), |hex| u64::from_str_radix(hex, 16).ok())
}

/// Renders milliseconds as something a person can judge at a glance.
fn human_duration(ms: i64) -> String {
    let days = ms / (24 * 60 * 60 * 1000);
    if days > 0 {
        format!("{days}d")
    } else {
        format!("{}h", ms / (60 * 60 * 1000))
    }
}

fn print_help() {
    println!(
        "credsync-sim — deterministic simulation

    --seeds N        run a batch of N seeds (default 1000)
    --seed S         run one seed; accepts 0x-prefixed hex
    --trace          print the run's trace (with --seed)
    -h, --help       this

A bug report is one integer. If a batch fails, replay its seed:

    cargo run -p credsync-sim -- --seed 0x4f21a9c3 --trace"
    );
}
