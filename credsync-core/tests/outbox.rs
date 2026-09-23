//! CS-8: the outbox — enqueue, budget-aware batching, results, dead-letter.
//!
//! Platform Plan v1.1 names losing student work silently as risk R1 and calls it trust-fatal.
//! Everything here is testing one claim from different sides: **a command that reached the outbox
//! leaves it only through an outcome someone can read.**

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

mod common;

use common::*;
use credsync_core::{OutboxError, Resolution, StorageError};
use credsync_protocol::limits;

// -------------------------------------------------------------------------------------------
// Enqueue
// -------------------------------------------------------------------------------------------

#[test]
fn an_enqueued_command_is_persisted_before_it_is_queued_in_memory() {
    let (mut engine, storage) = new_engine();

    engine.enqueue(entry(1, 10)).expect("enqueues");

    assert_eq!(engine.outbox_len(), 1);
    assert!(engine.outbox_contains(command_id(1)));
    storage.with(|s| {
        assert_eq!(s.queued.len(), 1, "the entry reached storage");
        assert_eq!(s.queued[0].0, command_id(1));
    });
}

/// A failed persist must not leave the command queued in memory.
///
/// Otherwise the user is told their work is saved, the app restarts, and it is simply gone —
/// with the app having reported success. Queueing in memory only after the commit is what makes
/// "saved" mean the same thing to the user and to the database.
#[test]
fn a_failed_persist_leaves_nothing_queued() {
    let (mut engine, storage) = new_engine();
    storage.with_mut(|s| {
        s.fail_next = Some(StorageError::Transient {
            detail: "disk full".into(),
        });
    });

    let err = engine.enqueue(entry(1, 10)).expect_err("must surface");
    assert!(matches!(err, OutboxError::Storage(_)));

    assert_eq!(engine.outbox_len(), 0, "nothing queued in memory");
    storage.with(|s| assert!(s.queued.is_empty(), "and nothing in storage"));
}

#[test]
fn commands_are_pushed_in_the_order_they_were_written() {
    let (mut engine, _storage) = new_engine();
    for n in 1..=4 {
        engine.enqueue(entry(n, 10)).expect("enqueues");
    }

    let push = engine
        .build_push(protocol(), 1_000_000)
        .expect("builds")
        .expect("something to send");

    let ids: Vec<_> = push.commands.iter().map(|c| c.id).collect();
    assert_eq!(
        ids,
        (1..=4).map(command_id).collect::<Vec<_>>(),
        "a later edit must never reach the host ahead of the earlier one it depends on"
    );
}

// -------------------------------------------------------------------------------------------
// DoD 3 — batching respects the COMPRESSED byte budget, not a row count
// -------------------------------------------------------------------------------------------

/// The same commands and the same budget, but a better compression ratio, fit more entries.
///
/// This is the test that distinguishes a compressed-byte budget from a row count. If batching
/// counted rows, or measured uncompressed bytes, both runs below would send identical batches.
#[test]
fn a_better_compression_ratio_fits_more_commands_in_the_same_budget() {
    let budget = 400;

    let (mut incompressible, _s1) = new_engine_with(FakeCompressor::with_ratio(1));
    let (mut compressible, _s2) = new_engine_with(FakeCompressor::with_ratio(8));
    for n in 1..=20 {
        incompressible.enqueue(entry(n, 200)).expect("enqueues");
        compressible.enqueue(entry(n, 200)).expect("enqueues");
    }

    let lean = incompressible
        .build_push(protocol(), budget)
        .expect("builds")
        .expect("some");
    let fat = compressible
        .build_push(protocol(), budget)
        .expect("builds")
        .expect("some");

    assert!(
        fat.commands.len() > lean.commands.len(),
        "compressible payloads must fill a larger batch: got {} vs {}",
        fat.commands.len(),
        lean.commands.len()
    );
}

#[test]
fn the_compressor_is_actually_consulted() {
    let compressor = FakeCompressor::with_ratio(2);
    let calls = compressor.calls.clone();
    let (mut engine, _storage) = new_engine_with(compressor);
    engine.enqueue(entry(1, 50)).expect("enqueues");

    engine.build_push(protocol(), 10_000).expect("builds");

    assert!(
        calls.get() > 0,
        "the budget must be measured through the Compressor, not guessed"
    );
}

/// A single command larger than the budget still goes, alone.
///
/// Otherwise one oversized entry wedges the outbox permanently: nothing sent, nothing resolved,
/// and every later write stuck behind it forever. `docs/spec.md` §2 states the same rule for pull
/// — a change larger than the budget is delivered alone rather than stalling the cursor.
#[test]
fn one_oversized_command_is_sent_alone_rather_than_wedging_the_outbox() {
    let (mut engine, _storage) = new_engine();
    engine.enqueue(entry(1, 5_000)).expect("enqueues");
    engine.enqueue(entry(2, 10)).expect("enqueues");

    let push = engine
        .build_push(protocol(), 10)
        .expect("builds")
        .expect("something must be sent");

    assert_eq!(
        push.commands.len(),
        1,
        "the oversized command goes alone rather than not at all"
    );
    assert_eq!(push.commands[0].id, command_id(1));
}

#[test]
fn the_entry_count_cap_binds_too() {
    let (mut engine, _storage) = new_engine();
    // Two more than the cap, each tiny, with an effectively unlimited byte budget.
    for n in 0..=(limits::COMMANDS_MAX_COUNT + 1) {
        engine
            .enqueue(entry(u8::try_from(n % 250).unwrap_or(0), 1))
            .expect("enqueues");
    }

    let push = engine
        .build_push(protocol(), usize::MAX)
        .expect("builds")
        .expect("some");

    assert_eq!(
        push.commands.len(),
        limits::COMMANDS_MAX_COUNT,
        "spec.md section 2.1 caps a push at 256 entries"
    );
}

#[test]
fn an_empty_outbox_produces_no_request() {
    let (engine, _storage) = new_engine();
    assert!(
        engine
            .build_push(protocol(), 1000)
            .expect("builds")
            .is_none(),
        "there is nothing to say, so say nothing"
    );
}

// -------------------------------------------------------------------------------------------
// DoD 4 — rejected commands retain their reason
// -------------------------------------------------------------------------------------------

#[test]
fn a_rejected_command_is_dead_lettered_with_its_reason() {
    let (mut engine, storage) = new_engine();
    engine.enqueue(entry(1, 10)).expect("enqueues");

    let resolved = engine
        .apply_results(&push_response(vec![rejected(
            1,
            "Submission deadline passed on 14 September.",
        )]))
        .expect("resolves");

    assert_eq!(engine.outbox_len(), 0, "it left the outbox");

    let (_, reason) = resolved.dead_lettered().next().expect("one dead letter");
    assert_eq!(
        reason.as_str(),
        "Submission deadline passed on 14 September."
    );

    storage.with(|s| {
        assert_eq!(s.resolved.len(), 1, "the outcome was recorded");
        match &s.resolved[0].1 {
            Resolution::DeadLettered { reason } => {
                assert_eq!(
                    reason.as_str(),
                    "Submission deadline passed on 14 September."
                );
            }
            other => panic!("expected a dead letter, got {other:?}"),
        }
    });
}

#[test]
fn applied_and_superseded_are_recorded_distinctly() {
    let (mut engine, storage) = new_engine();
    engine.enqueue(entry(1, 10)).expect("enqueues");
    engine.enqueue(entry(2, 10)).expect("enqueues");

    engine
        .apply_results(&push_response(vec![applied(1, 900), superseded(2)]))
        .expect("resolves");

    storage.with(|s| {
        assert!(matches!(
            s.resolved[0].1,
            Resolution::Applied {
                server_seq: Some(_)
            }
        ));
        assert!(
            matches!(s.resolved[1].1, Resolution::Superseded),
            "superseded must not be folded into applied: a UI showing 'saved' for a superseded \
             draft is telling the user something false"
        );
    });
}

// -------------------------------------------------------------------------------------------
// DoD 5 — replayed results are idempotent
// -------------------------------------------------------------------------------------------

#[test]
fn applying_the_same_result_twice_changes_nothing() {
    let (mut engine, storage) = new_engine();
    engine.enqueue(entry(1, 10)).expect("enqueues");

    let response = push_response(vec![applied(1, 900)]);
    let first = engine.apply_results(&response).expect("resolves");
    let second = engine.apply_results(&response).expect("replay is harmless");

    assert_eq!(first.resolutions.len(), 1);
    assert_eq!(second.resolutions.len(), 0, "nothing left to resolve");
    assert_eq!(second.unknown, 1, "and the replay is visible as such");

    storage.with(|s| {
        assert_eq!(s.resolved.len(), 1, "the outcome was recorded exactly once");
    });
}

/// One response naming the same command twice resolves it once.
///
/// Regression test. `tests/outbox_property.rs` found this: the in-memory outbox is not pruned
/// until the commit succeeds, so `outbox_contains` alone let a repeated id through twice and
/// recorded two outcomes for one command.
///
/// With differing verdicts the stored outcome then depended on write order — a dead letter
/// recorded for a command the host actually applied, or an "applied" masking a real rejection the
/// user needed to see. The first answer wins; later repeats are noise.
///
/// Same shape as the CS-7 staging bug (D-041): logic reading state its own in-progress batch is
/// about to change.
#[test]
fn a_response_naming_one_command_twice_resolves_it_once() {
    let (mut engine, storage) = new_engine();
    engine.enqueue(entry(1, 10)).expect("enqueues");

    let resolved = engine
        .apply_results(&push_response(vec![
            applied(1, 900),
            rejected(1, "contradictory second verdict"),
        ]))
        .expect("resolves");

    assert_eq!(resolved.resolutions.len(), 1, "resolved exactly once");
    storage.with(|s| {
        assert_eq!(s.resolved.len(), 1, "one outcome recorded, not two");
        assert!(
            matches!(s.resolved[0].1, Resolution::Applied { .. }),
            "the first answer wins; a later contradiction must not overwrite it"
        );
    });
    assert_eq!(engine.outbox_len(), 0);
}

#[test]
fn a_result_for_a_command_we_never_queued_is_ignored() {
    let (mut engine, storage) = new_engine();
    engine.enqueue(entry(1, 10)).expect("enqueues");

    let resolved = engine
        .apply_results(&push_response(vec![applied(99, 1)]))
        .expect("resolves");

    assert_eq!(resolved.unknown, 1);
    assert_eq!(engine.outbox_len(), 1, "our own command is untouched");
    storage.with(|s| assert!(s.resolved.is_empty()));
}

// -------------------------------------------------------------------------------------------
// The central claim — nothing leaves without an outcome
// -------------------------------------------------------------------------------------------

/// A command the server said nothing about stays queued.
///
/// This is the single most important behaviour in the outbox. The server answered about other
/// commands and was silent about this one; silence is not permission to discard it (D-009).
#[test]
fn a_command_with_no_result_stays_queued() {
    let (mut engine, _storage) = new_engine();
    engine.enqueue(entry(1, 10)).expect("enqueues");
    engine.enqueue(entry(2, 10)).expect("enqueues");
    engine.enqueue(entry(3, 10)).expect("enqueues");

    engine
        .apply_results(&push_response(vec![applied(1, 900), applied(3, 901)]))
        .expect("resolves");

    assert_eq!(engine.outbox_len(), 1);
    assert!(
        engine.outbox_contains(command_id(2)),
        "the command nobody mentioned is still waiting, not gone"
    );
}

/// A failed resolution commit leaves everything queued, ready to retry.
#[test]
fn a_failed_resolution_commit_leaves_the_outbox_intact() {
    let (mut engine, storage) = new_engine();
    engine.enqueue(entry(1, 10)).expect("enqueues");
    storage.with_mut(|s| {
        s.fail_next = Some(StorageError::Transient {
            detail: "killed mid-commit".into(),
        });
    });

    engine
        .apply_results(&push_response(vec![applied(1, 900)]))
        .expect_err("the failure must surface");

    assert_eq!(
        engine.outbox_len(),
        1,
        "the command is still queued, so the push is simply retried"
    );
    storage.with(|s| {
        assert!(s.resolved.is_empty(), "and no outcome was recorded");
        assert_eq!(s.queued.len(), 1, "the entry is still in storage");
    });
}

/// A rejection arriving without a reason is still dead-lettered, never dropped.
///
/// The decoder enforces that a rejection carries a reason, so this can only arrive from a
/// programmatically built response — but refusing the whole batch over one malformed result would
/// strand every other command in it. Dead-lettering with an unhelpful reason is recoverable;
/// dropping the entry is not.
#[test]
fn a_rejection_without_a_reason_is_still_dead_lettered() {
    let (mut engine, storage) = new_engine();
    engine.enqueue(entry(1, 10)).expect("enqueues");

    let mut bad = rejected(1, "placeholder");
    bad.reason = None;

    engine
        .apply_results(&push_response(vec![bad]))
        .expect("resolves");

    storage.with(|s| match &s.resolved[0].1 {
        Resolution::DeadLettered { reason } => {
            assert!(!reason.as_str().is_empty(), "a reason is always present");
        }
        other => panic!("expected a dead letter, got {other:?}"),
    });
}
