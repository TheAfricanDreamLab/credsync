//! CS-18: backpressure, and the real server logic running inside a seeded run.
//!
//! # What is actually being claimed
//!
//! "Backpressure sheds load without dropping acknowledged commands" is two claims, and only one of
//! them is about shedding:
//!
//! 1. The server may deliver less and process fewer commands when loaded.
//! 2. It may **never** answer a command it did not process.
//!
//! The second is the dangerous one. Answering `rejected` because the server was busy would tell a
//! student their work was refused when nothing ever examined it — and because the dedupe table is
//! durable, that lie is served back on every retry, forever. Same rule as D-065 approached from the
//! other side: there the server could not get an answer, here it chose not to ask.
//!
//! So the tests below are mostly about silence: what a loaded server does *not* say.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use credsync_sim::{FaultRates, Trace, World};

/// Seeds enough to be confident an episode fires without making the suite slow.
const SEEDS: u64 = 12;

/// Steps per run here, matching the other simulator tests rather than a full batch.
///
/// [`STEPS_PER_RUN`] is a fortnight of device life, which is the right length for a nightly batch
/// running in release mode and entirely the wrong one for `cargo test`, which builds unoptimised:
/// the first draft of this file used it and took five minutes **per test**. A suite nobody will
/// run is worse than a shorter one that people do.
///
/// Fifteen hundred steps is a day of device life and enough for load to arrive, bind, and ease
/// off several times over — which `the_overload_fault_actually_sheds_load` asserts rather than
/// assumes.
///
/// [`STEPS_PER_RUN`]: credsync_sim::STEPS_PER_RUN
const STEPS: u32 = 1_500;

/// Rates that guarantee load happens, for tests that need it to.
fn loaded() -> FaultRates {
    FaultRates {
        overload: 2,
        ..FaultRates::default()
    }
}

fn run(seed: u64, rates: FaultRates) -> World {
    let mut world = World::new(seed, rates, Trace::counting());
    world.run(STEPS);
    world
}

// -------------------------------------------------------------------------------------------
// The scenario has to actually happen
// -------------------------------------------------------------------------------------------

/// Shedding fires, and defers real work when it does.
///
/// The first thing to check, and the easiest to skip. Every assertion below is vacuous if the
/// server never went under load — a green run would then prove only that the fault never fired.
/// This is the same failure the fault-distribution work at CS-30 exists to find, caught here for
/// the one fault this slice adds.
#[test]
fn the_overload_fault_actually_sheds_load() {
    let mut episodes = 0u64;
    let mut deferred = 0u64;

    for seed in 0..SEEDS {
        let world = run(seed, loaded());
        episodes += world.pressure.episodes;
        deferred += world.pressure.deferred_commands;
    }

    assert!(
        episodes > 0,
        "the overload fault never fired across {SEEDS} seeds; every backpressure assertion in \
         this file would be vacuous"
    );
    assert!(
        deferred > 0,
        "load was applied {episodes} time(s) but no command was ever deferred, so the shedding \
         path never ran"
    );
}

// -------------------------------------------------------------------------------------------
// The rule: never answer what you did not process
// -------------------------------------------------------------------------------------------

/// Every command the server answered is one it actually processed.
///
/// Checked by counting. The server records an outcome only in `apply_command`, so the number of
/// recorded answers can never exceed the number of commands that reached it. A server that shed
/// load by writing verdicts for commands it skipped would show more answers than it processed.
#[test]
fn a_shedding_server_never_answers_a_command_it_did_not_process() {
    for seed in 0..SEEDS {
        let world = run(seed, loaded());

        let answered = world.server_answered_commands();
        let enqueued = world.commands_ever_enqueued();

        assert!(
            answered <= enqueued,
            "seed {seed:#x}: the server recorded {answered} answers for {enqueued} commands — it \
             invented a verdict for work it never saw"
        );
    }
}

/// Nothing acknowledged is lost, and nothing unanswered is abandoned.
///
/// After settling, every command a device enqueued must have reached a terminal state: either the
/// server answered it and the device recorded the resolution, or it is still in the outbox waiting.
/// What must never happen is a command that left the outbox without a recorded outcome — that is a
/// write the user made and nothing anywhere remembers.
#[test]
fn shedding_never_loses_an_enqueued_command() {
    for seed in 0..SEEDS {
        let world = run(seed, loaded());

        for (i, db) in world.databases().iter().enumerate() {
            let db = db.borrow();

            let resolved: std::collections::BTreeSet<_> =
                db.resolved.iter().map(|(id, _)| *id).collect();
            let queued: std::collections::BTreeSet<_> =
                db.outbox.iter().map(|(c, _)| c.id).collect();

            // Everything the server answered for this device must be resolved on it, or still
            // queued — never simply gone.
            for id in db.enqueued_ids() {
                assert!(
                    resolved.contains(&id) || queued.contains(&id),
                    "seed {seed:#x}, device {i}: command {id} left the outbox with no recorded \
                     outcome — the user's write is gone"
                );
            }
        }
    }
}

/// A command the server declined to look at stays in the outbox.
///
/// The mechanism that makes shedding safe. `apply_results` resolves only ids the response names,
/// so a prefix response leaves the rest queued. If the client resolved commands it had not been
/// told about, shedding would silently discard work.
#[test]
fn deferred_commands_stay_queued_rather_than_resolving() {
    for seed in 0..SEEDS {
        let world = run(seed, loaded());

        for (i, db) in world.databases().iter().enumerate() {
            let db = db.borrow();
            for (id, _) in &db.resolved {
                assert!(
                    world.server_has_answered(id),
                    "seed {seed:#x}, device {i}: command {id} is resolved on the device and the \
                     server has no record of it. Either the server answered a command it never \
                     processed, or the client resolved one it was never told about — both are a \
                     verdict nobody made"
                );
            }
        }
    }
}

// -------------------------------------------------------------------------------------------
// Convergence still holds, which is the point of shedding rather than failing
// -------------------------------------------------------------------------------------------

/// The world still converges with load applied across the full fault menu.
///
/// Backpressure that stopped the world converging would not be backpressure, it would be an
/// outage. This runs the standing distribution *plus* guaranteed load.
#[test]
fn the_world_converges_under_load_and_the_full_fault_menu() {
    for seed in 0..SEEDS {
        let world = run(seed, loaded());
        assert!(
            world.invariants.holds(),
            "seed {seed:#x}: {:?}",
            world.invariants.violations()
        );
    }
}

/// A server restarting mid-cycle is survivable, not a failure.
///
/// The restart drops the warm cache; the log and the dedupe table are durable and survive. A
/// restart that lost recorded outcomes would re-apply commands the client had already been told
/// were applied, which is the one thing the dedupe table exists to prevent.
#[test]
fn a_server_restart_mid_cycle_is_survivable() {
    let rates = FaultRates {
        // Aggressive: a restart every other step or so, which is far past anything realistic. If
        // the invariants hold here they hold at any sane rate.
        server_restart: 2,
        overload: 3,
        ..FaultRates::default()
    };

    for seed in 0..SEEDS {
        let world = run(seed, rates);
        assert!(
            world.invariants.holds(),
            "seed {seed:#x}: a server restart broke an invariant: {:?}",
            world.invariants.violations()
        );
    }
}

// -------------------------------------------------------------------------------------------
// Determinism, which every fault must preserve
// -------------------------------------------------------------------------------------------

/// Load episodes come from the seed, not from a clock.
///
/// The severity and duration of an episode are drawn from the run's generator. If either came from
/// elapsed time or a thread, a seed would stop replaying — and the simulator would keep passing
/// while quietly losing the ability to find anything.
#[test]
fn load_episodes_replay_identically() {
    for seed in 0..4 {
        let a = World::new(seed, loaded(), Trace::recording());
        let b = World::new(seed, loaded(), Trace::recording());
        let (mut a, mut b) = (a, b);
        a.run(STEPS);
        b.run(STEPS);

        assert_eq!(
            a.trace.render(),
            b.trace.render(),
            "seed {seed:#x}: two runs of one seed produced different traces, so load is not \
             deterministic"
        );
        assert_eq!(
            a.pressure.episodes, b.pressure.episodes,
            "seed {seed:#x}: episode counts differ"
        );
        assert_eq!(
            a.pressure.deferred_commands, b.pressure.deferred_commands,
            "seed {seed:#x}: deferred counts differ"
        );
    }
}
