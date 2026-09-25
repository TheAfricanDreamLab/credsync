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

    // The next successful apply must have re-READ the scope.
    //
    // Counted as reads, not as transactions. The first version of this asserted on `attempts`,
    // which counts `transact` — and every successful apply makes one, so the assertion held with
    // the defence deleted. Caught in review on #77, and it is the third test in this session that
    // passed for a reason unrelated to what it claimed to check.
    let reads_before = storage.with(|s| s.scope_reads.get());
    engine
        .apply_batch(&common::batch_with_digest(
            vec![common::upsert(2, "r2", 1)],
            digest_for("r1", 1),
        ))
        .expect("applies");
    assert!(
        storage.with(|s| s.scope_reads.get()) > reads_before,
        "the engine did not re-read the scope after a failure"
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

    // With reads working again, the next attempt succeeds — and proves it re-read rather than
    // simply giving up on the flag.
    storage.with_mut(|s| s.fail_read = None);
    let reads_before = storage.with(|s| s.scope_reads.get());
    engine
        .apply_batch(&common::batch_with_digest(
            vec![common::upsert(1, "r1", 1)],
            digest_for("r1", 1),
        ))
        .expect("applies once storage is readable again");
    assert!(
        storage.with(|s| s.scope_reads.get()) > reads_before,
        "the scope stopped being suspect without ever being re-read"
    );
}

// -------------------------------------------------------------------------------------------
// Reconciling without an apply
// -------------------------------------------------------------------------------------------

/// A suspect scope can be made trustworthy on demand, without an apply to carry the reload.
///
/// # The deadlock this pins
///
/// A caller that learns a scope is suspect will reasonably decline to build a request from it —
/// that is the whole point of `needs_reload`. If the *only* thing that clears the flag is the head
/// of `apply_changes`, that caller has deadlocked itself: the flag is cleared by an apply, an apply
/// consumes a response, a response needs a request, and the request is what was withheld.
///
/// This is not hypothetical. Wiring the simulator's pull path to skip suspect scopes turned seed 1
/// of `a_hostile_run_converges` red with *"still at cursor 49 after settling, with the server at
/// 51; the device never caught up"* — a device permanently stuck one slice short, in a run whose
/// faults had all stopped.
///
/// So the flag is not merely a signal to stand down; it comes with a way to stand up again.
#[test]
fn a_suspect_scope_can_be_reconciled_without_applying_anything() {
    let (mut engine, storage) = common::new_engine();

    engine
        .apply_batch(&common::batch_with_digest(
            vec![common::upsert(1, "r1", 1)],
            digest_for("r1", 1),
        ))
        .expect("applies");

    storage.with_mut(|s| {
        s.fail_next = Some(StorageError::Transient {
            detail: "committed, then lost the acknowledgement".to_owned(),
        });
    });
    engine
        .apply_batch(&common::batch(vec![common::upsert(2, "r1", 2)]))
        .expect_err("the transaction failed");

    assert!(
        engine.needs_reload(&common::scope()),
        "a failed transaction must leave the scope suspect"
    );

    let reads_before = storage.with(|s| s.scope_reads.get());
    engine
        .reload_scope(&common::scope())
        .expect("storage is readable, so the reload succeeds");

    assert!(
        storage.with(|s| s.scope_reads.get()) > reads_before,
        "reload_scope cleared the flag without reading storage"
    );
    assert!(
        !engine.needs_reload(&common::scope()),
        "the scope is reconciled, so the caller may now build a request from it"
    );
}

/// A reload that cannot read storage leaves the scope suspect rather than reporting success.
///
/// The failure mode worth guarding: a `reload_scope` that swallowed its error would hand the caller
/// a scope it believes is reconciled and is not, which is strictly worse than the deadlock above —
/// the caller would go on to build a request from a cursor storage does not support.
#[test]
fn a_reload_that_cannot_read_storage_keeps_the_scope_suspect() {
    let (mut engine, storage) = common::new_engine();

    storage.with_mut(|s| {
        s.fail_next = Some(StorageError::Transient {
            detail: "committed, then lost the acknowledgement".to_owned(),
        });
    });
    engine
        .apply_batch(&common::batch(vec![common::upsert(1, "r1", 1)]))
        .expect_err("the transaction failed");

    storage.with_mut(|s| {
        s.fail_read = Some(StorageError::Corrupt {
            detail: "cannot read the scope back".to_owned(),
        });
    });
    engine
        .reload_scope(&common::scope())
        .expect_err("the reload must report the read failure");

    assert!(
        engine.needs_reload(&common::scope()),
        "a failed reload must leave the scope suspect so the next attempt tries again"
    );
}

// -------------------------------------------------------------------------------------------
// The outbox, which fails the same way for a different reason
// -------------------------------------------------------------------------------------------

/// A command that committed under a reported failure is still pushed.
///
/// # Why the empty check is the bug
///
/// `enqueue` writes to storage and only then appends to the in-memory queue, so a commit reported
/// as a failure leaves storage holding a command the cache has never heard of. `build_push` then
/// finds an empty queue and returns `Ok(None)` — and because the only other reload lives in
/// `apply_results`, which cannot run when no push was built, **nothing ever looks again**. The
/// command is durable, invisible, and unsent for the life of the install.
///
/// That is why the reload sits *before* the `is_empty()` short-circuit rather than after it. An
/// empty cache is precisely the state that needs checking, so a guard that trusts it to skip the
/// check has inverted itself.
///
/// Found in review on #77 — where the reload this test covers had silently failed to apply at all,
/// so the method the pull request described was not the method that shipped.
#[test]
fn a_command_that_committed_under_a_reported_failure_is_still_pushed() {
    let (mut engine, storage) = common::new_engine();

    // A second engine commits a command, standing in for the write that succeeded before the
    // adapter reported failure. `fail_next` rolls back the staged write, so the durable entry has
    // to come from somewhere the failure did not touch.
    let mut writer = common::engine_over(storage.clone());
    writer
        .enqueue(common::entry(7, 0))
        .expect("the honest write commits");

    // The engine under test still has an empty cache, and now learns its own write failed.
    storage.with_mut(|s| {
        s.fail_next = Some(StorageError::Transient {
            detail: "committed, then lost the acknowledgement".to_owned(),
        });
    });
    engine
        .enqueue(common::entry(8, 0))
        .expect_err("the transaction failed");

    let push = engine
        .build_push(common::protocol(), 1_000_000)
        .expect("building a push must not fail")
        .expect("the outbox was reloaded, so there is a command to send");

    assert!(
        !push.commands.is_empty(),
        "storage held a command and the push went out empty"
    );
}

/// An outbox reload that cannot read storage refuses to build a push rather than building an empty one.
///
/// The counterpart to the test above: reporting `Ok(None)` here would be indistinguishable from
/// "there is nothing to send", which is the one answer the engine has no basis for.
#[test]
fn a_push_is_not_built_while_the_outbox_cannot_be_reloaded() {
    let (mut engine, storage) = common::new_engine();

    storage.with_mut(|s| {
        s.fail_next = Some(StorageError::Transient {
            detail: "committed, then lost the acknowledgement".to_owned(),
        });
    });
    engine
        .enqueue(common::entry(7, 0))
        .expect_err("the transaction failed");

    storage.with_mut(|s| {
        s.fail_read = Some(StorageError::Corrupt {
            detail: "cannot read the outbox back".to_owned(),
        });
    });
    engine
        .build_push(common::protocol(), 1_000_000)
        .expect_err("an unreadable outbox must not read as an empty one");
}
