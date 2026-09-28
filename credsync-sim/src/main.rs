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

    // Where the batch starts. A nightly job that sweeps 0..10_000 every night re-searches the same
    // ten thousand schedules and finds nothing after the first night, so the range has to move.
    let from = flag_value(&args, "--from")
        .and_then(|v| parse_seed(&v))
        .unwrap_or(0);

    let jobs = flag_value(&args, "--jobs")
        .and_then(|v| v.parse::<usize>().ok())
        .unwrap_or_else(|| {
            std::thread::available_parallelism().map_or(1, std::num::NonZeroUsize::get)
        })
        .max(1);

    run_batch(from, seeds, jobs)
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
    // The version goes with the seed, always. A seed alone is only a reproduction if the schedule
    // that produced it is the one replaying it -- see `SIM_VERSION`.
    eprintln!("  simulator v{}", credsync_sim::SIM_VERSION);
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

/// What one stripe of seeds produced.
struct Stripe {
    /// Device-life simulated across the stripe, summed.
    simulated_ms: i64,
    /// Fault counts, for the coverage table.
    faults: Vec<(&'static str, u64)>,
    /// The lowest failing seed in this stripe, if any.
    failure: Option<(u64, Vec<credsync_sim::Violation>)>,
}

/// Runs seeds `lo..hi`, stopping at the first failure in that stripe.
///
/// Stopping early is per stripe rather than per batch: the other stripes finish their own work,
/// which costs a little time on a failing run and keeps the coverage numbers honest for the seeds
/// that did run.
fn run_stripe(lo: u64, hi: u64) -> Stripe {
    let mut totals: std::collections::BTreeMap<&'static str, u64> =
        std::collections::BTreeMap::new();
    let mut simulated_ms: i64 = 0;

    for seed in lo..hi {
        let mut world = World::new(seed, FaultRates::default(), Trace::counting());
        world.run(STEPS_PER_RUN);
        simulated_ms += world.elapsed_ms();

        for (name, n) in world.trace.faults() {
            *totals.entry(name).or_insert(0) += n;
        }

        if !world.invariants.holds() {
            return Stripe {
                simulated_ms,
                faults: totals.into_iter().collect(),
                failure: Some((seed, world.invariants.violations().to_vec())),
            };
        }
    }

    Stripe {
        simulated_ms,
        faults: totals.into_iter().collect(),
        failure: None,
    }
}

/// Runs a batch, reporting aggregate fault coverage.
///
/// The coverage line is not decoration. Design §11 is blunt that a simulator can look busy while
/// exploring almost nothing, so a fault showing zero occurrences across a thousand seeds is a
/// fault this rig does not actually have — and that is worth seeing on every run rather than
/// discovering during a coverage pass months later.
fn run_batch(from: u64, seeds: u64, jobs: usize) -> ExitCode {
    let started = std::time::Instant::now();

    // Seeds are independent -- each owns its world and its generator -- so which core runs one
    // cannot change what it does. That is what makes spreading them safe, and it is the only
    // reason the sweep can be large enough to find anything within a CI budget (#78).
    //
    // The batch is split into contiguous stripes rather than handed out one seed at a time,
    // because a work-stealing order would make the *reporting* non-deterministic even though each
    // run is not: two failing seeds in one batch would be announced in whichever order the threads
    // reached them. The lowest failing seed is chosen explicitly below for the same reason.
    let width = seeds.div_ceil(jobs as u64).max(1);
    let mut handles = Vec::new();
    for stripe in 0..jobs as u64 {
        let lo = from.saturating_add(stripe.saturating_mul(width));
        let hi = from.saturating_add(seeds).min(lo.saturating_add(width));
        if lo >= hi {
            break;
        }
        handles.push(std::thread::spawn(move || run_stripe(lo, hi)));
    }

    let mut totals: std::collections::BTreeMap<&'static str, u64> =
        std::collections::BTreeMap::new();
    let mut simulated_ms: i64 = 0;
    let mut failure: Option<(u64, Vec<credsync_sim::Violation>)> = None;

    for handle in handles {
        // A panicking stripe is a broken simulator, not a failing seed, and the two must not read
        // alike: reporting it as a seed failure would send someone chasing an invariant that never
        // fired.
        let Ok(outcome) = handle.join() else {
            eprintln!("error: a simulator thread panicked; this is a bug in the simulator itself");
            return ExitCode::FAILURE;
        };
        simulated_ms += outcome.simulated_ms;
        for (name, n) in outcome.faults {
            *totals.entry(name).or_insert(0) += n;
        }
        // Lowest wins, so the batch reports the same seed however the threads were scheduled.
        if let Some((seed, violations)) = outcome.failure
            && failure.as_ref().is_none_or(|(prev, _)| seed < *prev)
        {
            failure = Some((seed, violations));
        }
    }

    if let Some((seed, violations)) = failure {
        println!("{}/{seeds} seeds green, then:", seed.saturating_sub(from));
        report(seed, &violations);
        return ExitCode::FAILURE;
    }

    let wall = started.elapsed();
    let last = from.saturating_add(seeds).saturating_sub(1);
    println!("{seeds}/{seeds} seeds green in {wall:.2?}  (0x{from:016x}..=0x{last:016x})");
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
    --from S         start the batch at seed S (default 0); accepts 0x-prefixed hex
    --jobs N         seeds to run at once (default: available cores)
    --seed S         run one seed; accepts 0x-prefixed hex
    --trace          print the run's trace (with --seed)
    -h, --help       this

Seeds are independent, so a batch spreads across cores. Which core ran a seed
cannot change what that seed does: each run owns its world and its generator,
and the failure reported is always the LOWEST failing seed in the range, never
whichever thread happened to finish first.

A bug report is one integer. If a batch fails, replay its seed:

    cargo run -p credsync-sim -- --seed 0x4f21a9c3 --trace"
    );
}
