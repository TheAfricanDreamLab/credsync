//! CS-7: the pull apply path — cursor walk, ordered apply, tombstones, atomicity.
//!
//! Every test here asserts on what the *database* holds afterwards, not on the engine's private
//! state. A test that reaches into internals breaks on every refactor while proving nothing about
//! behaviour, and the behaviour that matters is what survives a restart.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

mod common;

use common::*;
use credsync_core::{ApplyError, ScopeState, StorageError};
use credsync_protocol::{Cursor, ScopeDigest};

// -------------------------------------------------------------------------------------------
// DoD 1 — in-order apply, out-of-order rejection, gap rejection, duplicate seq handling
// -------------------------------------------------------------------------------------------

#[test]
fn in_order_changes_apply_and_advance_the_cursor() {
    let (mut engine, storage) = new_engine();

    let applied = engine
        .apply_batch(&batch(vec![
            upsert(1, "refl:a", 1),
            upsert(2, "refl:b", 1),
            upsert(5, "refl:c", 1),
        ]))
        .expect("a well-formed batch applies");

    assert_eq!(applied.changes, 3);
    storage.with(|s| {
        assert_eq!(s.row_count(), 3);
        assert_eq!(s.cursor(&scope()), Some(Cursor::new(5).unwrap()));
    });
    assert_eq!(
        engine.scope_state(&scope()).unwrap().cursor,
        Cursor::new(5).unwrap()
    );
}

/// Sparse `seq` values are normal, not a gap.
///
/// `seq` is a `bigserial` shared by every scope, so one scope's entries are naturally
/// non-contiguous — `1, 2, 7, 19` is a healthy scope whose neighbours were busy. An apply path
/// that demanded contiguity would reject almost every real batch.
#[test]
fn sparse_seq_values_are_not_treated_as_gaps() {
    let (mut engine, _storage) = new_engine();
    engine
        .apply_batch(&batch(vec![
            upsert(1, "refl:a", 1),
            upsert(7, "refl:b", 1),
            upsert(19_000, "refl:c", 1),
        ]))
        .expect("sparse seqs are ordinary");
}

#[test]
fn out_of_order_changes_are_refused() {
    let (mut engine, storage) = new_engine();

    let err = engine
        .apply_batch(&batch(vec![
            upsert(1, "refl:a", 1),
            upsert(5, "refl:b", 1),
            upsert(3, "refl:c", 1),
        ]))
        .expect_err("decreasing seq must be refused");

    assert_eq!(
        err,
        ApplyError::OutOfOrder {
            previous: 5,
            found: 3
        }
    );
    storage.with(|s| {
        assert_eq!(s.row_count(), 0, "a refused batch writes nothing");
        assert_eq!(s.attempts, 0, "a refused batch never reaches storage");
    });
}

#[test]
fn a_repeated_seq_within_a_batch_is_refused() {
    let (mut engine, _storage) = new_engine();

    let err = engine
        .apply_batch(&batch(vec![upsert(4, "refl:a", 1), upsert(4, "refl:b", 1)]))
        .expect_err("a repeated seq must be refused");

    assert_eq!(
        err,
        ApplyError::OutOfOrder {
            previous: 4,
            found: 4
        }
    );
}

/// A replayed batch is refused rather than silently skipped.
///
/// Re-applying would add the row's contribution to the digest a second time, and the digest does
/// not cancel duplicates (D-032) — so the corruption would persist and surface much later as a
/// divergence report pointing nowhere near its cause.
#[test]
fn changes_at_or_below_the_cursor_are_refused() {
    let (mut engine, storage) = new_engine();
    engine
        .apply_batch(&batch(vec![upsert(1, "refl:a", 1), upsert(4, "refl:b", 1)]))
        .expect("first batch applies");

    let err = engine
        .apply_batch(&batch(vec![upsert(4, "refl:b", 1), upsert(9, "refl:c", 1)]))
        .expect_err("a replayed seq must be refused");

    assert_eq!(
        err,
        ApplyError::AlreadyApplied {
            cursor: 4,
            found: 4
        }
    );
    storage.with(|s| assert_eq!(s.commits, 1, "only the first batch committed"));
}

/// A `next_cursor` that does not cover what was sent would wedge the scope forever.
///
/// The client would advance to it, the server would re-deliver the uncovered change on the next
/// pull, and the client would refuse it as already applied — one round trip at a time, for good.
#[test]
fn a_next_cursor_below_the_last_change_is_refused() {
    let (mut engine, _storage) = new_engine();

    let mut b = batch(vec![upsert(3, "refl:a", 1), upsert(8, "refl:b", 1)]);
    b.next_cursor = Cursor::new(5).unwrap();

    let err = engine
        .apply_batch(&b)
        .expect_err("cursor must cover the batch");
    assert_eq!(
        err,
        ApplyError::CursorWouldRegress {
            next_cursor: 5,
            last_seq: 8
        }
    );
}

#[test]
fn an_empty_batch_still_advances_the_cursor() {
    let (mut engine, storage) = new_engine();

    let mut b = batch(vec![]);
    b.next_cursor = Cursor::new(120).unwrap();

    let applied = engine.apply_batch(&b).expect("an empty batch is valid");

    assert_eq!(applied.changes, 0);
    storage.with(|s| assert_eq!(s.cursor(&scope()), Some(Cursor::new(120).unwrap())));
}

// -------------------------------------------------------------------------------------------
// DoD 2 — tombstones remove the row and correct the digest
// -------------------------------------------------------------------------------------------

#[test]
fn a_tombstone_removes_the_row() {
    let (mut engine, storage) = new_engine();
    engine
        .apply_batch(&batch(vec![upsert(1, "refl:a", 1), upsert(2, "refl:b", 1)]))
        .expect("rows land");

    engine
        .apply_batch(&batch(vec![tombstone(3, "refl:a", 2)]))
        .expect("tombstone applies");

    storage.with(|s| {
        assert_eq!(s.row_count(), 1);
        assert!(s.row(&entity(), &id("refl:a")).is_none());
        assert!(s.row(&entity(), &id("refl:b")).is_some());
    });
}

/// The digest must return to exactly what it was before the row ever existed.
///
/// Exactly, not approximately — `docs/spec.md` §5. This is the property that makes a tombstone
/// safe, and it is checked here against a digest computed from scratch rather than against the
/// engine's own arithmetic.
#[test]
fn a_tombstone_leaves_the_digest_as_if_the_row_never_existed() {
    let (mut engine, storage) = new_engine();

    engine
        .apply_batch(&batch(vec![upsert(1, "refl:keep", 1)]))
        .expect("first row");
    let before = engine.scope_state(&scope()).unwrap().digest;

    engine
        .apply_batch(&batch(vec![upsert(2, "refl:doomed", 1)]))
        .expect("second row");
    assert_ne!(
        engine.scope_state(&scope()).unwrap().digest,
        before,
        "adding a row must change the digest"
    );

    engine
        .apply_batch(&batch(vec![tombstone(3, "refl:doomed", 1)]))
        .expect("tombstone");

    assert_eq!(
        engine.scope_state(&scope()).unwrap().digest,
        before,
        "the digest must return exactly to its earlier value"
    );
    storage.with(|s| assert_eq!(s.digest_from_scratch(), before));
}

/// Deleting a row this client never held must not move the digest.
///
/// The row may have been created and deleted inside one pull window. Subtracting a contribution
/// that was never added would corrupt the digest permanently, and the corruption would be
/// reported as divergence with no trace of where it came from.
#[test]
fn a_tombstone_for_an_unknown_row_is_a_no_op() {
    let (mut engine, storage) = new_engine();
    engine
        .apply_batch(&batch(vec![upsert(1, "refl:a", 1)]))
        .expect("one row");
    let before = engine.scope_state(&scope()).unwrap().digest;

    engine
        .apply_batch(&batch(vec![tombstone(2, "refl:never-existed", 9)]))
        .expect("tombstone for an absent row is not an error");

    assert_eq!(engine.scope_state(&scope()).unwrap().digest, before);
    storage.with(|s| assert_eq!(s.row_count(), 1));
}

/// A row created and deleted inside one batch must leave no trace in the digest.
///
/// Regression test. `tests/apply_property.rs` found this on a two-change sequence: staging read
/// each row's current version from *storage*, but an earlier change in the same batch has not
/// committed yet and is invisible there. The tombstone therefore looked up a row storage had
/// never heard of, subtracted nothing, and left the upsert's contribution in the digest forever.
///
/// The client would then disagree with the server on every subsequent pull and re-bootstrap the
/// scope endlessly — the divergence detector firing on damage it had caused itself. Entirely
/// routine input, too: a pull window spans days for an offline device, so a row being created and
/// deleted within one is ordinary.
#[test]
fn a_row_created_and_deleted_in_one_batch_leaves_the_digest_empty() {
    let (mut engine, storage) = new_engine();

    engine
        .apply_batch(&batch(vec![
            upsert(1, "refl:brief", 1),
            tombstone(2, "refl:brief", 1),
        ]))
        .expect("applies");

    storage.with(|s| {
        assert_eq!(s.row_count(), 0, "the row was created then deleted");
        assert_eq!(
            s.digest_from_scratch(),
            engine.scope_state(&scope()).unwrap().digest,
            "the running digest must match the rows actually stored"
        );
    });
    assert_eq!(
        engine.scope_state(&scope()).unwrap().digest,
        ScopeDigest::EMPTY,
        "no rows means the empty digest, exactly"
    );
}

/// The same row upserted twice in one batch ends at the later version, counted once.
#[test]
fn a_row_upserted_twice_in_one_batch_is_counted_once() {
    let (mut engine, storage) = new_engine();

    engine
        .apply_batch(&batch(vec![
            upsert(1, "refl:a", 1),
            upsert(2, "refl:a", 2),
            upsert(3, "refl:a", 3),
        ]))
        .expect("applies");

    storage.with(|s| {
        assert_eq!(s.row_count(), 1);
        assert_eq!(
            s.row(&entity(), &id("refl:a")).unwrap().row_version.get(),
            3
        );
        assert_eq!(
            s.digest_from_scratch(),
            engine.scope_state(&scope()).unwrap().digest
        );
    });
}

/// Replacing a row subtracts the version actually stored, not the one the change mentions.
#[test]
fn replacing_a_row_replaces_its_digest_contribution() {
    let (mut engine, storage) = new_engine();

    engine
        .apply_batch(&batch(vec![upsert(1, "refl:a", 1)]))
        .expect("v1");
    engine
        .apply_batch(&batch(vec![upsert(2, "refl:a", 7)]))
        .expect("v7");

    storage.with(|s| {
        assert_eq!(s.row_count(), 1, "an upsert replaces, never duplicates");
        assert_eq!(
            s.row(&entity(), &id("refl:a")).unwrap().row_version.get(),
            7
        );
        assert_eq!(
            s.digest_from_scratch(),
            engine.scope_state(&scope()).unwrap().digest,
            "incremental digest must equal a from-scratch recomputation"
        );
    });
}

// -------------------------------------------------------------------------------------------
// DoD 3 — the cursor is persisted in the SAME transaction as the rows
// -------------------------------------------------------------------------------------------

#[test]
fn rows_cursor_and_digest_commit_in_one_transaction() {
    let (mut engine, storage) = new_engine();

    engine
        .apply_batch(&batch(vec![
            upsert(1, "refl:a", 1),
            upsert(2, "refl:b", 1),
            tombstone(3, "refl:a", 2),
        ]))
        .expect("applies");

    storage.with(|s| {
        assert_eq!(
            s.attempts, 1,
            "rows, cursor and digest must be one transaction, not several"
        );
        assert_eq!(s.commits, 1);
        assert!(s.cursor(&scope()).is_some(), "the cursor was in it");
        assert!(s.digest(&scope()).is_some(), "so was the digest");
    });
}

// -------------------------------------------------------------------------------------------
// DoD 5 — an interrupted apply leaves cursor and rows consistent
// -------------------------------------------------------------------------------------------

/// A failed transaction must leave *nothing* behind — no rows, no cursor, no digest.
///
/// `FakeStorage` stages every op and then discards the staged state, so this would also catch an
/// adapter that half-applied before failing, rather than only one that refused up front.
#[test]
fn a_failed_transaction_leaves_no_partial_batch() {
    let (mut engine, storage) = new_engine();
    engine
        .apply_batch(&batch(vec![upsert(1, "refl:a", 1)]))
        .expect("the first batch commits");
    let cursor_before = storage.with(|s| s.cursor(&scope()));
    let digest_before = storage.with(|s| s.digest(&scope()).cloned());

    storage.with_mut(|s| {
        s.fail_next = Some(StorageError::Transient {
            detail: "killed mid-commit".into(),
        });
    });

    let err = engine
        .apply_batch(&batch(vec![upsert(2, "refl:b", 1), upsert(3, "refl:c", 1)]))
        .expect_err("a failed transaction must surface");
    assert!(matches!(err, ApplyError::Storage(_)));

    storage.with(|s| {
        assert_eq!(s.row_count(), 1, "no row from the failed batch is visible");
        assert_eq!(s.cursor(&scope()), cursor_before, "the cursor did not move");
        assert_eq!(
            s.digest(&scope()).cloned(),
            digest_before,
            "the digest did not move"
        );
    });
}

/// After a failed commit the engine's memory must still describe what storage holds.
///
/// This is the bug the ordering in `apply_batch` exists to prevent: advance the cursor in memory,
/// have the commit fail, and the client never asks for those changes again. It reports success
/// forever while silently missing rows.
#[test]
fn a_failed_transaction_does_not_advance_the_engine() {
    let (mut engine, storage) = new_engine();
    engine
        .apply_batch(&batch(vec![upsert(1, "refl:a", 1)]))
        .expect("first batch");
    let state_before = *engine.scope_state(&scope()).unwrap();

    storage.with_mut(|s| {
        s.fail_next = Some(StorageError::Transient {
            detail: "disk full".into(),
        });
    });
    engine
        .apply_batch(&batch(vec![upsert(2, "refl:b", 1)]))
        .expect_err("fails");

    assert_eq!(
        *engine.scope_state(&scope()).unwrap(),
        state_before,
        "engine state must not move when the commit did not"
    );

    // And the refused batch can simply be retried.
    storage.with_mut(|s| s.fail_next = None);
    engine
        .apply_batch(&batch(vec![upsert(2, "refl:b", 1)]))
        .expect("the same batch applies once storage recovers");
    storage.with(|s| assert_eq!(s.row_count(), 2));
}

// -------------------------------------------------------------------------------------------
// Divergence detection (spec.md section 5)
// -------------------------------------------------------------------------------------------

#[test]
fn a_matching_server_digest_reports_no_divergence() {
    let (mut engine, _storage) = new_engine();

    // Apply once to learn what the client computes, then replay the same change into a fresh
    // engine with the server claiming exactly that.
    engine
        .apply_batch(&batch(vec![upsert(1, "refl:a", 1)]))
        .expect("applies");
    let client_digest = engine.scope_state(&scope()).unwrap().digest.to_hex();

    let (mut fresh, _s) = new_engine();
    let applied = fresh
        .apply_batch(&batch_with_digest(
            vec![upsert(1, "refl:a", 1)],
            client_digest,
        ))
        .expect("applies");

    assert!(!applied.diverged);
    assert_eq!(fresh.pending_effects(), 0, "agreement is not an event");
}

/// A disagreeing digest is silent divergence: reported, and the batch still committed.
///
/// Refusing the batch would strand the client at a cursor it can never advance past while
/// leaving the divergence exactly as unresolved. The repair — taint the scope, re-bootstrap,
/// replay the outbox — is CS-22 (#23).
#[test]
fn a_disagreeing_server_digest_is_reported_as_divergence() {
    let (mut engine, storage) = new_engine();

    let applied = engine
        .apply_batch(&batch_with_digest(
            vec![upsert(1, "refl:a", 1)],
            hex("ffffffffffffffffffffffffffffffff"),
        ))
        .expect("the batch still commits");

    assert!(applied.diverged);
    storage.with(|s| assert_eq!(s.row_count(), 1, "the batch was committed"));

    match engine.next_effect() {
        Some(credsync_core::Effect::Emit(credsync_core::Telemetry::ScopeDiverged {
            scope: s,
            client,
            server,
        })) => {
            assert_eq!(s, scope());
            assert_ne!(client, server, "both digests are carried, and they differ");
            assert_eq!(server, hex("ffffffffffffffffffffffffffffffff"));
        }
        other => panic!("expected a ScopeDiverged telemetry effect, got {other:?}"),
    }
}

// -------------------------------------------------------------------------------------------
// Restoring across a restart
// -------------------------------------------------------------------------------------------

#[test]
fn a_restored_scope_resumes_from_its_persisted_cursor() {
    let (mut engine, _storage) = new_engine();
    engine.restore_scope(
        scope(),
        ScopeState::restored(Cursor::new(100).unwrap(), ScopeDigest::EMPTY),
    );

    let err = engine
        .apply_batch(&batch(vec![upsert(50, "refl:old", 1)]))
        .expect_err("changes before the restored cursor are already applied");
    assert_eq!(
        err,
        ApplyError::AlreadyApplied {
            cursor: 100,
            found: 50
        }
    );

    engine
        .apply_batch(&batch(vec![upsert(101, "refl:new", 1)]))
        .expect("changes after it apply");
}

#[test]
fn an_unknown_scope_starts_from_the_beginning() {
    let (engine, _storage) = new_engine();
    assert!(
        engine.scope_state(&scope()).is_none(),
        "nothing is known until something is applied or restored"
    );
}
