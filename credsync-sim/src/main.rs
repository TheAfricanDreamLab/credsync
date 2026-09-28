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

    // Absent means "use the default"; present but unparseable means the caller meant something and
    // got it wrong. Those must not read alike: a CI job whose `--from` expression produced an empty
    // string would otherwise sweep 0..N every night and report success, which is the exact failure
    // this flag exists to prevent (#78).
    // Zero is refused rather than accepted as "a batch of nothing". `run_batch` would start no
    // stripe, check no invariant, and exit 0 -- a gate that cannot fail, which `.coderabbit.yaml`
    // names as its own category of defect. A sweep that reports success has to have swept.
    let seeds = match option(&args, "--seeds", 1_000, positive_seeds) {
        Ok(v) => v,
        Err(()) => return ExitCode::FAILURE,
    };
    let from = match option(&args, "--from", 0, parse_seed) {
        Ok(v) => v,
        Err(()) => return ExitCode::FAILURE,
    };
    let default_jobs = std::thread::available_parallelism().map_or(1, std::num::NonZeroUsize::get);
    let jobs = match option(&args, "--jobs", default_jobs, positive_jobs) {
        Ok(v) => v,
        Err(()) => return ExitCode::FAILURE,
    };

    // A range that runs off the end of u64 would saturate, start no worker for the lost seeds, and
    // still report the full requested count green -- a sweep that claims coverage it never had.
    if from.checked_add(seeds).is_none() {
        eprintln!(
            "error: --from 0x{from:x} plus --seeds {seeds} runs past the end of the seed space"
        );
        return ExitCode::FAILURE;
    }

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

/// Keeps whichever of two candidate failures names the lower seed.
///
/// Extracted so the claim can be *tested* rather than reasoned about. Parallelism does not change
/// what any seed does -- each run owns its world and its generator -- but it does change the order
/// stripes finish in, and without this the batch would report whichever failing seed a thread
/// happened to reach first. The simulator's whole contract is that a bug report is one integer
/// that replays identically, so two runs of the same range must name the same seed.
///
/// Folding rather than sorting because the violations are carried along and cloning them to sort
/// would cost more than the comparison saves.
fn keep_lowest(
    current: Option<(u64, Vec<credsync_sim::Violation>)>,
    candidate: Option<(u64, Vec<credsync_sim::Violation>)>,
) -> Option<(u64, Vec<credsync_sim::Violation>)> {
    match (current, candidate) {
        (None, other) | (other, None) => other,
        (Some(a), Some(b)) => Some(if b.0 < a.0 { b } else { a }),
    }
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
        //
        // This arm runs in debug and under `cargo test`. It does NOT run in the release builds both
        // CI jobs use, because the workspace sets `panic = "abort"` there, so the process dies on
        // the panic itself and never reaches the join. That is still unambiguous -- an abort with a
        // panic message is nobody's idea of a seed failure -- but it is worth stating rather than
        // leaving this to read as a guarantee it cannot offer in the build that matters
        // (found in review on #81).
        let Ok(outcome) = handle.join() else {
            eprintln!("error: a simulator thread panicked; this is a bug in the simulator itself");
            return ExitCode::FAILURE;
        };
        simulated_ms += outcome.simulated_ms;
        for (name, n) in outcome.faults {
            *totals.entry(name).or_insert(0) += n;
        }
        failure = keep_lowest(failure, outcome.failure);
    }

    if let Some((seed, violations)) = failure {
        println!("{}/{seeds} seeds green, then:", seed.saturating_sub(from));
        report(seed, &violations);
        return ExitCode::FAILURE;
    }

    // Deterministic output on stdout, measurement on stderr. A seeded run's report must be the
    // same bytes on every machine and every date, and elapsed time is neither -- `.coderabbit.yaml`
    // bans real clocks reaching the simulator's output for exactly this reason. The timing is still
    // printed, because sizing the CI sweep needs it (#78); it is just not part of the report.
    let last = from.saturating_add(seeds).saturating_sub(1);
    println!("{seeds}/{seeds} seeds green  (0x{from:016x}..=0x{last:016x})");
    eprintln!("took {:.2?}", started.elapsed());
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
/// Reads a flag, distinguishing "absent" from "present and wrong".
///
/// `Ok(default)` only when the flag is genuinely absent. A flag that is present is then required
/// to carry a value the parser accepts; anything else prints a message and returns `Err(())`.
///
/// Falling back to the default on a bad value -- or on a missing one -- would let a typo, a
/// truncated command line or an empty shell expansion look like a deliberate choice.
/// A seed count, which has to be at least one.
///
/// Named rather than written inline at the call site so the tests bind to the parser `main` uses.
/// As a closure it was possible to test a copy of the rule and watch the real one be deleted: the
/// planted-bug drill for this guard passed with the production filter removed, which is the whole
/// reason it is a function (found while drilling #81).
fn positive_seeds(v: &str) -> Option<u64> {
    v.parse::<u64>().ok().filter(|n| *n > 0)
}

/// A worker count, same rule and same reason. Zero workers would start no thread, join nothing,
/// and report every seed green.
fn positive_jobs(v: &str) -> Option<usize> {
    v.parse::<usize>().ok().filter(|n| *n > 0)
}

fn option<T>(
    args: &[String],
    flag: &str,
    default: T,
    parse: impl Fn(&str) -> Option<T>,
) -> Result<T, ()> {
    if !args.iter().any(|a| a == flag) {
        return Ok(default);
    }
    // Present with nothing after it: `--seeds` as the last argument used to read as absence and
    // take the default, so `credsync-sim --from 50000 --seeds` would have swept 1000 seeds from
    // 50000 and called it the range that was asked for. A trailing flag is a truncated command
    // line, not a choice (found in review on #81).
    let Some(raw) = flag_value(args, flag) else {
        eprintln!("error: {flag} expects a value");
        return Err(());
    };
    parse(&raw).ok_or_else(|| {
        eprintln!("error: {flag} does not accept {raw:?}");
    })
}

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

#[cfg(test)]
mod tests {
    use super::{keep_lowest, option, parse_seed, positive_jobs, positive_seeds};

    /// An absent flag takes the default; a present one that parses takes its value.
    #[test]
    fn a_flag_that_is_absent_or_valid_is_accepted() {
        let none: Vec<String> = Vec::new();
        assert_eq!(
            option(&none, "--seeds", 1_000, |v| v.parse::<u64>().ok()),
            Ok(1_000)
        );

        let given = vec!["--seeds".to_owned(), "42".to_owned()];
        assert_eq!(
            option(&given, "--seeds", 1_000, |v| v.parse::<u64>().ok()),
            Ok(42)
        );
    }

    /// A flag that is present and unparseable is refused, not quietly defaulted.
    ///
    /// The failure this prevents is specific and silent: a CI expression that produced an empty
    /// string or a stray word would have sent `--from` back to seed 0, and the nightly job would
    /// re-sweep the same range every night while reporting success — the exact thing the flag was
    /// added to stop (#78). "Absent" and "present and wrong" are different answers.
    #[test]
    fn a_flag_that_is_present_and_wrong_is_refused() {
        let bad = vec!["--from".to_owned(), "not-a-seed".to_owned()];
        assert_eq!(option(&bad, "--from", 0u64, parse_seed), Err(()));

        let zero = vec!["--jobs".to_owned(), "0".to_owned()];
        assert_eq!(option(&zero, "--jobs", 4usize, positive_jobs), Err(()));
    }

    /// A trailing flag is a truncated command line, not a choice.
    ///
    /// `credsync-sim --from 50000 --seeds` used to read as "--seeds absent", take the default and
    /// sweep 1000 seeds while the caller believed they had asked for something else. Absent and
    /// present-without-a-value are different answers, and only the first has a default.
    #[test]
    fn a_flag_with_no_value_after_it_is_refused() {
        for flag in ["--seeds", "--from", "--jobs"] {
            let truncated = vec![flag.to_owned()];
            assert_eq!(
                option(&truncated, flag, 7u64, |v| v.parse::<u64>().ok()),
                Err(()),
                "{flag} with nothing after it must not fall back to the default"
            );
        }
    }

    /// A zero-seed batch is refused, because it would be a gate that cannot fail.
    ///
    /// `--seeds 0` parses, starts no stripe, checks no invariant and exits 0. A sweep that reports
    /// success has to have swept something; `.coderabbit.yaml` names gates that cannot fail as
    /// their own category of defect, and this would have been one in a job whose entire purpose is
    /// to be able to go red.
    #[test]
    fn a_zero_seed_batch_is_refused() {
        let zero = vec!["--seeds".to_owned(), "0".to_owned()];
        assert_eq!(option(&zero, "--seeds", 1_000u64, positive_seeds), Err(()));

        let one = vec!["--seeds".to_owned(), "1".to_owned()];
        assert_eq!(
            option(&one, "--seeds", 1_000u64, positive_seeds),
            Ok(1),
            "one seed is a real batch and must still be accepted"
        );

        // Zero workers would start no thread, join nothing, and call every seed green.
        let no_jobs = vec!["--jobs".to_owned(), "0".to_owned()];
        assert_eq!(option(&no_jobs, "--jobs", 4usize, positive_jobs), Err(()));
    }

    /// The batch reports the lowest failing seed, whatever order the stripes finish in.
    ///
    /// The claim parallelism puts at risk. Each seed is deterministic on its own — it owns its
    /// world and its generator — but stripes finish in whatever order the scheduler picks, so
    /// without this fold the batch would announce whichever failing seed a thread reached first,
    /// and two runs of the same range could name different seeds. "A bug report is one integer"
    /// then quietly stops being true.
    ///
    /// Folded over every permutation rather than one arrangement, because the failure mode here is
    /// precisely an ordering that was not the one tried.
    #[test]
    fn the_lowest_failing_seed_wins_whatever_order_the_stripes_finish_in() {
        let failure = |seed: u64| {
            Some((
                seed,
                vec![credsync_sim::Violation {
                    invariant: "no-loss",
                    device: Some(0),
                    detail: format!("seed {seed}"),
                    at_ms: 0,
                }],
            ))
        };

        let seeds = [700u64, 42, 913];
        for a in 0..3 {
            for b in 0..3 {
                for c in 0..3 {
                    if a == b || b == c || a == c {
                        continue;
                    }
                    let order = [seeds[a], seeds[b], seeds[c]];
                    let folded = order.into_iter().fold(None, |acc, seed| {
                        // A clean stripe between each failing one, since `None` must not displace
                        // a failure already found.
                        keep_lowest(keep_lowest(acc, None), failure(seed))
                    });
                    assert_eq!(
                        folded.map(|(seed, _)| seed),
                        Some(42),
                        "order {order:?} did not report the lowest seed"
                    );
                }
            }
        }
    }

    /// Clean stripes neither invent a failure nor erase one.
    #[test]
    fn clean_stripes_neither_invent_nor_erase_a_failure() {
        assert_eq!(keep_lowest(None, None), None);

        let only = Some((
            9u64,
            vec![credsync_sim::Violation {
                invariant: "idempotency",
                device: None,
                detail: "the only failure".to_owned(),
                at_ms: 0,
            }],
        ));
        let survived = (0..5).fold(only, |acc, _| keep_lowest(acc, None));
        assert_eq!(survived.map(|(seed, _)| seed), Some(9));
    }

    /// A range running off the end of the seed space is refused before any stripe is built.
    ///
    /// Saturating arithmetic would otherwise clamp both bounds to `u64::MAX`, start no worker for
    /// the seeds that fell off, and print the full requested count as green — a sweep claiming
    /// coverage it never had, which is worse than a sweep that failed (found in review on #81).
    #[test]
    fn a_range_that_runs_past_the_end_of_the_seed_space_is_refused() {
        assert!(u64::MAX.checked_add(1).is_none(), "the guard's premise");
        assert!((u64::MAX - 1).checked_add(2).is_none());
        assert!(
            1_000u64.checked_add(10_000).is_some(),
            "an ordinary range still passes"
        );
    }
}
