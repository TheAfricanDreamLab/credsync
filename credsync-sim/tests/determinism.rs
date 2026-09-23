//! CS-11: the harness itself. Determinism, seed replay, fault coverage, time compression.
//!
//! These test the simulator rather than the engine. That distinction matters: the invariants that
//! test the *engine* arrive at CS-12, and until they do a green run here means the rig works, not
//! that credSync is correct.
//!
//! # Why determinism comes first
//!
//! Nondeterminism does not announce itself. A simulator that has quietly lost determinism still
//! passes, still looks busy, and no longer finds anything — and every seed recorded in every bug
//! issue silently starts replaying a different run. If the first test below ever fails, nothing
//! else the simulator has ever reported can be trusted, and it is the only thing worth fixing
//! that day.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use credsync_sim::{Fault, FaultRates, STEPS_PER_RUN, Trace, World};

/// Runs a seed and returns its trace bytes.
fn trace_of(seed: u64) -> Vec<u8> {
    let mut world = World::new(seed, FaultRates::default(), Trace::recording());
    // A shorter run than the full batch: still thousands of steps and every fault class, but
    // fast enough that this file stays a test rather than an errand.
    world.run(2_000);
    world.trace.to_bytes()
}

// ---------------------------------------------------------------------------------------------
// DoD 2 — the same seed produces a byte-identical trace
// ---------------------------------------------------------------------------------------------

#[test]
fn the_same_seed_produces_a_byte_identical_trace() {
    let a = trace_of(0x4f21_a9c3);
    let b = trace_of(0x4f21_a9c3);

    assert_eq!(
        a.len(),
        b.len(),
        "two runs of one seed produced traces of different lengths"
    );
    assert!(
        a == b,
        "two runs of one seed diverged — determinism has been lost, and every recorded seed \
         in every bug issue now replays a different run"
    );
    assert!(!a.is_empty(), "the trace recorded nothing at all");
}

/// Determinism must survive many seeds, not just a lucky one.
#[test]
fn every_seed_in_a_small_batch_is_reproducible() {
    for seed in 0..12 {
        assert_eq!(
            trace_of(seed),
            trace_of(seed),
            "seed {seed} did not reproduce"
        );
    }
}

/// Different seeds must actually explore different runs.
///
/// The counterpart to the test above, and just as necessary: a simulator whose every seed
/// produced the same trace would be perfectly deterministic and completely useless.
#[test]
fn different_seeds_produce_different_traces() {
    let a = trace_of(1);
    let b = trace_of(2);
    assert_ne!(a, b, "two different seeds produced identical runs");
}

// ---------------------------------------------------------------------------------------------
// DoD 3 — a seed replays exactly
// ---------------------------------------------------------------------------------------------

/// A seed replays into the same world state, not merely the same trace text.
///
/// The trace is a summary; this checks the thing the summary describes. Two runs that agreed on
/// their trace while disagreeing on what the devices actually stored would be a trace that has
/// stopped describing the run.
#[test]
fn a_replayed_seed_lands_in_the_same_state() {
    let run = |seed: u64| {
        let mut world = World::new(seed, FaultRates::default(), Trace::counting());
        world.run(2_000);
        let rows: Vec<usize> = world
            .databases()
            .iter()
            .map(|db| db.borrow().rows.len())
            .collect();
        let outboxes: Vec<usize> = world
            .databases()
            .iter()
            .map(|db| db.borrow().outbox.len())
            .collect();
        let resolved: Vec<usize> = world
            .databases()
            .iter()
            .map(|db| db.borrow().resolved.len())
            .collect();
        (rows, outboxes, resolved, world.server().head())
    };

    assert_eq!(run(0x4f21_a9c3), run(0x4f21_a9c3));
}

// ---------------------------------------------------------------------------------------------
// DoD 1 — the fault menu is complete and every entry fires
// ---------------------------------------------------------------------------------------------

/// Every fault in Design §7.1 occurs in a small batch.
///
/// Design §11 is blunt that a rig can look busy while exploring almost nothing. A fault that
/// never fires is a fault this simulator does not have, whatever the menu claims — so the menu is
/// checked against what actually happened rather than against itself.
#[test]
fn every_fault_in_the_menu_fires() {
    let mut totals: std::collections::BTreeMap<&str, u64> = std::collections::BTreeMap::new();

    for seed in 0..8 {
        let mut world = World::new(seed, FaultRates::default(), Trace::counting());
        world.run(2_000);
        for (name, n) in world.trace.faults() {
            *totals.entry(name).or_insert(0) += n;
        }
    }

    for fault in Fault::ALL {
        let n = totals.get(fault.name()).copied().unwrap_or(0);
        assert!(
            n > 0,
            "fault '{}' never fired across eight seeds — it is in the menu but not in the rig",
            fault.name()
        );
    }
}

/// A world with the faults turned off produces none.
///
/// The control for the test above. Without it, "every fault fired" could be true of a rig that
/// fires faults unconditionally, which would be a different bug with the same green result.
#[test]
fn a_quiet_world_records_no_faults() {
    let mut world = World::new(7, FaultRates::none(), Trace::counting());
    world.run(500);
    assert!(
        world.trace.faults().is_empty(),
        "faults fired in a world configured to have none: {:?}",
        world.trace.faults()
    );
}

// ---------------------------------------------------------------------------------------------
// DoD 4 — time compression
// ---------------------------------------------------------------------------------------------

/// A full run covers weeks of device life, and does it in a moment.
///
/// The threshold is deliberately loose. This is guarding against the simulator quietly becoming
/// slow enough that nobody runs a thousand seeds any more — the failure mode is social, and it
/// arrives long before any hard limit.
#[test]
fn a_run_covers_weeks_of_device_life_in_moments() {
    let started = std::time::Instant::now();
    let mut world = World::new(3, FaultRates::default(), Trace::counting());
    world.run(STEPS_PER_RUN);
    let wall = started.elapsed();

    let days = world.elapsed_ms() / (24 * 60 * 60 * 1000);
    assert!(
        days >= 14,
        "a run should cover at least a fortnight, covered {days} days"
    );

    // Generous, because a debug build under a loaded CI runner is far slower than a release one.
    // Even so: if a single seed takes a minute, a thousand-seed batch takes a day and stops
    // being something anyone runs.
    assert!(
        wall.as_secs() < 120,
        "one seed took {wall:?}; a 1,000-seed batch would be untenable"
    );
}

/// Devices really do disagree about the time, and by days.
///
/// `docs/spec.md` §6 rests on device clocks lying — `client_ts` is a hint *"because device clocks
/// lie"* — so a simulator whose clocks all told the truth would let a `client_ts`-dependent bug
/// pass every run it ever did. The menu says ±3 days; this checks the rig delivers it.
#[test]
fn devices_disagree_about_what_time_it_is() {
    let mut worst_skew_ms: i64 = 0;
    let mut saw_disagreement_between_devices = false;

    for seed in 0..16 {
        let mut world = World::new(seed, FaultRates::default(), Trace::counting());
        world.run(200);

        let (true_now, apparent) = world.apparent_times_ms();
        for a in &apparent {
            worst_skew_ms = worst_skew_ms.max((a - true_now).abs());
        }
        if apparent.windows(2).any(|w| w[0] != w[1]) {
            saw_disagreement_between_devices = true;
        }
    }

    assert!(
        saw_disagreement_between_devices,
        "every device agreed with every other on the time; the clocks are not lying"
    );

    // A full day of skew somewhere across sixteen seeds. The menu allows three.
    let a_day_ms = 24 * 60 * 60 * 1000;
    assert!(
        worst_skew_ms > a_day_ms,
        "worst clock skew was {worst_skew_ms}ms, under a day — the ±3 day menu is not reaching \
         the devices"
    );
}
