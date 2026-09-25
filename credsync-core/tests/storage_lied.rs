//! #55: an adapter that commits and then reports failure must not corrupt the digest.
//!
//! # The failure this defends against
//!
//! `apply_changes` advances memory only after `transact` returns `Ok`. That is correct, and on its
//! own it is not enough:
//!
//! ```text
//!   transact(ops)  ->  committed, then Err(Transient)
//!   engine         ->  does not advance its cursor or digest (correct)
//!   next pull      ->  re-fetches changes storage already holds
//!   stage_change   ->  reads row_version, ALREADY the new value, so digest.update(new, new)
//!                      is a no-op
//!                  ->  writes the STALE in-memory digest over the correct stored one
//! ```
//!
//! The rows stay right. The digest regresses permanently, and the scope reports divergence on every
//! pull for the rest of its life — silent, and precisely the shape of failure this project exists
//! to prevent.
//!
//! The `Storage` contract says an adapter must never do this. These tests exist because the engine
//! cannot distinguish "nothing was written" from "everything was written and I was not told", and
//! the cost of assuming the worse one is a single read.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

mod common;

use credsync_core::StorageError;

/// The digest a client holds after one row at a given version.
fn digest_for(entity_id: &str, row_version: u64) -> credsync_protocol::HexString {
    let mut d = credsync_protocol::ScopeDigest::EMPTY;
    d.add(
        &common::entity(),
        &common::id(entity_id),
        credsync_protocol::RowVersion::new(row_version).expect("valid row version"),
    );
    d.to_hex()
}

// -------------------------------------------------------------------------------------------
// The defence
// -------------------------------------------------------------------------------------------

/// A storage error makes the engine re-read the scope rather than trust its cache.
///
/// The mechanism the rest of this file depends on. Without it the engine keeps a cursor and digest
/// that may be behind what storage holds, and the next apply writes the stale one back.
#[test]
fn a_storage_failure_makes_the_engine_reload_the_scope() {
    let (mut engine, storage) = common::new_engine();

    // One batch lands honestly.
    engine
        .apply_batch(&common::batch_with_digest(
            vec![common::upsert(1, "r1", 1)],
            digest_for("r1", 1),
        ))
        .expect("applies");
    let good = storage.with(|s| s.digest(&common::scope()).cloned());

    // The next transaction fails. The engine must now distrust its cache for this scope.
    storage.with_mut(|s| {
        s.fail_next = Some(StorageError::Transient {
            detail: "commit reported as failed".to_owned(),
        });
    });
    engine
        .apply_batch(&common::batch(vec![common::upsert(2, "r2", 1)]))
        .expect_err("the transaction failed");

    // Storage is unchanged, because that failure was honest.
    assert_eq!(
        storage.with(|s| s.digest(&common::scope()).cloned()),
        good,
        "an honest rollback should have changed nothing"
    );

    // The next successful apply must have re-read the scope. If it trusted its cache it would
    // still work here — so the assertion is on the *read*, counted below.
    let reads_before = storage.with(|s| s.attempts);
    engine
        .apply_batch(&common::batch_with_digest(
            vec![common::upsert(2, "r2", 1)],
            digest_for("r1", 1),
        ))
        .expect("applies");
    assert!(
        storage.with(|s| s.attempts) > reads_before,
        "the engine did not touch storage again after a failure"
    );
}

/// **The digest does not regress when an adapter commits and then reports failure.**
///
/// The bug in #55, end to end. Storage ends up ahead of memory; the engine must notice on the next
/// apply rather than writing its stale digest back over the correct one.
#[test]
fn a_committed_then_failed_transaction_does_not_corrupt_the_digest() {
    let (mut engine, storage) = common::new_engine();

    // Two rows land, honestly, so both storage and memory agree.
    engine
        .apply_batch(&common::batch_with_digest(
            vec![common::upsert(1, "r1", 1)],
            digest_for("r1", 1),
        ))
        .expect("applies");

    // Now the dishonest transaction: the ops land, and the engine is told they did not.
    //
    // Modelled by applying through a *second* engine over the same storage — storage advances,
    // this engine's memory does not, which is exactly the state the lie leaves behind.
    {
        let mut other = common::engine_over(storage.clone());
        other.restore_scope(
            common::scope(),
            credsync_core::ScopeState::restored(
                credsync_protocol::Cursor::new(1).expect("valid cursor"),
                credsync_protocol::ScopeDigest::from_raw(
                    u128::from_str_radix(digest_for("r1", 1).as_str(), 16).expect("hex"),
                ),
            ),
        );
        other
            .apply_batch(&common::batch_with_digest(
                vec![common::upsert(2, "r2", 1)],
                digest_for("r2", 1),
            ))
            .expect("the write lands in storage");
    }

    let stored_after_lie = storage
        .with(|s| s.digest(&common::scope()).cloned())
        .expect("a digest");

    // The engine is told its transaction failed, so it distrusts its cache.
    storage.with_mut(|s| {
        s.fail_next = Some(StorageError::Transient {
            detail: "committed, then reported failure".to_owned(),
        });
    });
    engine
        .apply_batch(&common::batch(vec![common::upsert(3, "r3", 1)]))
        .expect_err("the transaction 'failed'");

    // Re-delivering what storage already holds must not move the digest backwards.
    engine
        .apply_batch(&common::batch_with_digest(
            vec![common::upsert(2, "r2", 1)],
            stored_after_lie.clone(),
        ))
        .ok();

    let stored_now = storage
        .with(|s| s.digest(&common::scope()).cloned())
        .expect("a digest");
    assert_eq!(
        stored_now, stored_after_lie,
        "the engine wrote a stale digest back over the correct one; this scope now reports \
         divergence on every pull for the rest of its life"
    );
}

/// A reload that itself fails leaves the scope suspect, so the next attempt tries again.
///
/// Clearing the flag on a failed read would be worse than never setting it: the engine would
/// proceed on state it had just decided not to trust.
#[test]
fn a_failed_reload_leaves_the_scope_suspect() {
    let (mut engine, storage) = common::new_engine();

    storage.with_mut(|s| {
        s.fail_next = Some(StorageError::Transient {
            detail: "first failure".to_owned(),
        });
    });
    engine
        .apply_batch(&common::batch(vec![common::upsert(1, "r1", 1)]))
        .expect_err("the transaction failed");

    // The reload now fails too.
    storage.with_mut(|s| {
        s.fail_read = Some(StorageError::Corrupt {
            detail: "cannot read the scope back".to_owned(),
        });
    });
    engine
        .apply_batch(&common::batch(vec![common::upsert(1, "r1", 1)]))
        .expect_err("the reload failed, so the apply must not proceed");

    // With reads working again, the next attempt succeeds rather than staying stuck.
    storage.with_mut(|s| s.fail_read = None);
    engine
        .apply_batch(&common::batch_with_digest(
            vec![common::upsert(1, "r1", 1)],
            digest_for("r1", 1),
        ))
        .expect("applies once storage is readable again");
}
