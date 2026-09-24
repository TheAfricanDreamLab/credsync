//! The commit-order gap, and the simulator's ability to see it.
//!
//! `seq` is allocated when a write starts; a row becomes visible when its transaction commits. A
//! server that hands over every visible change regardless lets a client advance its cursor past
//! one that has not committed yet — and that change is then never delivered. Silent loss,
//! invisible to every ordering check, because the batch the client received was perfectly ordered.
//!
//! # Why this file exists
//!
//! Review found that bug in `credsync-server` (D-063). The simulator **could not have**: its
//! server committed every write instantly, so no two writes were ever in flight and the whole
//! class was unreachable. Two independent methods — the invariants at CS-12 and the planted-bug
//! drill at CS-13 — would have reported green forever against a server losing data.
//!
//! So the hazard is modelled here, deterministically, and the tests below are the drill run
//! against it: with the guard off the invariants must fire, and with it on they must not. A rig
//! that cannot fail on a bug class is a rig that does not test it.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use credsync_sim::{FaultRates, Trace, World};

/// With the guard on, a hostile run converges.
#[test]
fn the_guard_keeps_a_run_green_under_slow_commits() {
    for seed in 0..6 {
        let mut world = World::new(seed, FaultRates::default(), Trace::counting());
        world.run(1_200);
        assert!(
            world.invariants.holds(),
            "seed {seed} failed with the commit-order guard on: {:?}",
            world.invariants.violations()
        );
    }
}

/// Slow commits actually happen, or the test above proves nothing.
///
/// The control. A guard is only worth testing against a hazard that occurs.
#[test]
fn slow_commits_really_occur() {
    let mut world = World::new(3, FaultRates::default(), Trace::counting());
    world.run(1_200);
    let n = world
        .trace
        .faults()
        .get("slow-commit")
        .copied()
        .unwrap_or(0);
    assert!(
        n > 0,
        "no write was ever held open, so the guard was never exercised"
    );
}

/// **Without the guard, the invariants catch the loss.**
///
/// The point of the whole file. A server that hands over a visible change while an earlier `seq`
/// is still uncommitted lets the client advance past it, and `durable-effects` reports the row
/// that should exist and does not.
///
/// If this test ever stops failing to fail — that is, if the run goes green with the guard off —
/// the simulator has lost the ability to see this class again, and every later result about it is
/// worthless.
#[test]
fn without_the_guard_the_invariants_catch_the_lost_change() {
    let mut caught = 0;
    let mut checked = 0;

    for seed in 0..8 {
        let mut world = World::new(seed, FaultRates::default(), Trace::recording());
        world.disable_commit_order_guard();
        world.run(1_200);
        checked += 1;
        if !world.invariants.holds() {
            caught += 1;
        }
    }

    assert!(
        caught > 0,
        "the simulator ran {checked} seeds against a server that loses changes and reported \
         green every time; the hazard is modelled but nothing is watching for it"
    );
}
