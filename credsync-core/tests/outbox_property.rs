//! CS-8 DoD 1 and 2, as properties over arbitrary result sequences.
//!
//! The claim the whole engine is sold on is *"never loses an acknowledged write"*. Platform Plan
//! v1.1 calls silent loss trust-fatal, and D-009 makes it a protocol violation rather than a
//! tradeoff. A claim that strong cannot rest on hand-written cases: the losing sequence is
//! precisely the one nobody thought to write down.
//!
//! So the server here is hostile in all the ordinary ways. It answers subsets, answers out of
//! order, repeats itself, answers commands that were never sent, and stays silent about others.
//! Across every such sequence exactly one thing must hold:
//!
//! > **Every command is either still queued, or resolved with a recorded outcome. Never both,
//! > never neither.**
//!
//! Both DoD boxes fall out of that single invariant — nothing acknowledged is lost, and nothing
//! leaves silently — which is a sign it is the right invariant rather than two coincidences.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

mod common;

use common::*;
use credsync_core::Resolution;
use proptest::prelude::*;
use std::collections::BTreeSet;

/// How many commands a run queues. Small, so that answers collide and repeat often.
const MAX_COMMANDS: u8 = 12;

/// One verdict the server might return.
#[derive(Debug, Clone, Copy)]
enum Verdict {
    Applied,
    Rejected,
    Superseded,
}

fn verdict() -> impl Strategy<Value = Verdict> {
    prop_oneof![
        Just(Verdict::Applied),
        Just(Verdict::Rejected),
        Just(Verdict::Superseded),
    ]
}

/// One response: a set of (command index, verdict) pairs.
///
/// The index may exceed the number of commands actually queued, which produces results for
/// commands this client never sent — a real possibility on a shared or replayed connection, and
/// one the engine must shrug off rather than trust.
fn round() -> impl Strategy<Value = Vec<(u8, Verdict)>> {
    proptest::collection::vec((0u8..MAX_COMMANDS + 4, verdict()), 0..6)
}

fn rounds() -> impl Strategy<Value = Vec<Vec<(u8, Verdict)>>> {
    proptest::collection::vec(round(), 0..8)
}

fn to_result(n: u8, v: Verdict) -> credsync_protocol::CommandResult {
    match v {
        Verdict::Applied => applied(n, 1),
        Verdict::Rejected => rejected(n, "Refused by the host."),
        Verdict::Superseded => superseded(n),
    }
}

proptest! {
    #![proptest_config(ProptestConfig { cases: 256, ..ProptestConfig::default() })]

    /// DoD 1 and 2 together: every command is queued or resolved, never both, never neither.
    #[test]
    fn no_command_is_ever_lost_or_silently_dropped(
        count in 1u8..MAX_COMMANDS,
        rounds in rounds(),
    ) {
        let (mut engine, storage) = new_engine();
        for n in 0..count {
            engine.enqueue(entry(n, 8)).expect("enqueues");
        }

        for r in &rounds {
            let results = r.iter().map(|(n, v)| to_result(*n, *v)).collect();
            engine.apply_results(&push_response(results)).expect("resolves");

            // Checked after EVERY round, not once at the end. CLAUDE.md section 4: an invariant
            // is checked continuously, because a bug that self-corrects before the run ends is
            // still a bug -- and a command briefly absent from both places is a command that
            // would have been lost had the process died right then.
            let queued: BTreeSet<_> = (0..count)
                .map(command_id)
                .filter(|id| engine.outbox_contains(*id))
                .collect();
            let resolved: BTreeSet<_> = storage.with(|s| {
                s.resolved
                    .iter()
                    .map(|(id, _)| *id)
                    .filter(|id| (0..count).map(command_id).any(|c| c == *id))
                    .collect()
            });

            prop_assert!(
                queued.is_disjoint(&resolved),
                "a command was both queued and resolved"
            );
            prop_assert_eq!(
                queued.len() + resolved.len(),
                usize::from(count),
                "a command is neither queued nor resolved -- it was lost"
            );
        }
    }

    /// Every recorded outcome is one a person could act on.
    ///
    /// DoD 2 and 4. A dead letter without a reason is a dead end for the user: they wrote
    /// something, it did not save, and nothing on the screen says why.
    #[test]
    fn every_recorded_outcome_is_legible(count in 1u8..MAX_COMMANDS, rounds in rounds()) {
        let (mut engine, storage) = new_engine();
        for n in 0..count {
            engine.enqueue(entry(n, 8)).expect("enqueues");
        }
        for r in &rounds {
            let results = r.iter().map(|(n, v)| to_result(*n, *v)).collect();
            engine.apply_results(&push_response(results)).expect("resolves");
        }

        storage.with(|s| {
            for (_, resolution) in &s.resolved {
                match resolution {
                    Resolution::DeadLettered { reason } => {
                        prop_assert!(
                            !reason.as_str().trim().is_empty(),
                            "a dead letter must carry a reason a person can read"
                        );
                    }
                    Resolution::Applied { .. } | Resolution::Superseded => {}
                    other => prop_assert!(false, "unexpected resolution {:?}", other),
                }
            }
            Ok(())
        })?;
    }

    /// Each command is resolved at most once, however often the server repeats itself.
    ///
    /// DoD 5 as a property. A retried push returns the same results again, so double-resolution
    /// is the ordinary case rather than the exotic one.
    #[test]
    fn a_command_is_resolved_at_most_once(count in 1u8..MAX_COMMANDS, rounds in rounds()) {
        let (mut engine, storage) = new_engine();
        for n in 0..count {
            engine.enqueue(entry(n, 8)).expect("enqueues");
        }
        for r in &rounds {
            let results = r.iter().map(|(n, v)| to_result(*n, *v)).collect();
            engine.apply_results(&push_response(results)).expect("resolves");
        }

        let recorded: Vec<_> = storage.with(|s| s.resolved.iter().map(|(id, _)| *id).collect());
        let unique: BTreeSet<_> = recorded.iter().copied().collect();
        prop_assert_eq!(
            recorded.len(),
            unique.len(),
            "a command was resolved more than once"
        );
    }

    /// Replaying the whole history changes nothing after the first pass.
    #[test]
    fn replaying_every_round_is_idempotent(count in 1u8..MAX_COMMANDS, rounds in rounds()) {
        let (mut engine, storage) = new_engine();
        for n in 0..count {
            engine.enqueue(entry(n, 8)).expect("enqueues");
        }

        let responses: Vec<_> = rounds
            .iter()
            .map(|r| push_response(r.iter().map(|(n, v)| to_result(*n, *v)).collect()))
            .collect();

        for response in &responses {
            engine.apply_results(response).expect("resolves");
        }
        let after_first = storage.with(|s| s.resolved.clone());
        let queued_after_first = engine.outbox_len();

        for response in &responses {
            engine.apply_results(response).expect("replay is harmless");
        }

        prop_assert_eq!(
            storage.with(|s| s.resolved.clone()),
            after_first,
            "replaying the history recorded something new"
        );
        prop_assert_eq!(engine.outbox_len(), queued_after_first);
    }
}
