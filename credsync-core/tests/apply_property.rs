//! CS-7 DoD 4: applying any valid change sequence yields a digest matching a from-scratch
//! recomputation.
//!
//! This is the property the whole divergence mechanism rests on. The client maintains its digest
//! *incrementally* — add on insert, subtract-then-add on replace, subtract on tombstone — while
//! the server computes its own over the rows it holds. If those two ways of arriving at a number
//! can ever disagree, then every mismatch is suspect, and the detector that exists to find silent
//! divergence becomes a generator of false alarms that teams learn to ignore.
//!
//! Hand-written examples cannot establish this. Three orderings proving nothing about the fourth
//! is exactly the situation `CLAUDE.md` §4 names when it says to reach for `proptest` rather than
//! a handful of cases.
//!
//! The recomputation side never consults the engine: `FakeStorage::digest_from_scratch` folds
//! `ScopeDigest::from_rows` over whatever rows survived. Agreement therefore means the two
//! independent routes match, rather than that one function agrees with itself.

// Not run under Miri, and the case count is not the reason.
//
// Each case here drives the whole engine — dozens of batches, hundreds of canonical JSON
// encodings, a full storage fake — so even two cases is minutes of interpreted work, and the
// suite dominated the Miri job while adding nothing to it.
//
// What Miri buys this project is soundness of the *dependencies* we lean on (`serde_json`,
// `twox-hash`, `blake3`), because `credsync-core` and `credsync-protocol` are both
// `#![forbid(unsafe_code)]` and cannot express undefined behaviour at all. The unit suites in
// `tests/apply.rs`, `tests/outbox.rs` and `tests/conflict.rs` walk every one of those code paths
// under Miri already. More inputs through the same unsafe code finds no new unsoundness; it
// finds logic bugs, which is what the native run at 256 cases is for.
//
// **This does not weaken any gate.** Every property below runs in full on every pull request
// under `cargo test`, and at 4096 cases nightly.
#![cfg(not(miri))]
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

mod common;

use common::*;
use credsync_protocol::{Change, Cursor};
use proptest::prelude::*;

/// One intended change, before `seq` values are assigned.
#[derive(Debug, Clone)]
struct Step {
    /// Index into a small pool of row ids, so upserts and tombstones collide often.
    row: usize,
    /// `true` for an upsert, `false` for a tombstone.
    upsert: bool,
    row_version: u64,
}

/// A small pool of ids. Deliberately small: with 200 distinct ids almost every change would
/// touch a fresh row, and the interesting arithmetic — replace and remove — would rarely run.
const ROWS: usize = 6;

fn step() -> impl Strategy<Value = Step> {
    (0..ROWS, any::<bool>(), 1u64..=50).prop_map(|(row, upsert, row_version)| Step {
        row,
        upsert,
        row_version,
    })
}

fn steps() -> impl Strategy<Value = Vec<Step>> {
    proptest::collection::vec(step(), 0..40)
}

/// Turns steps into changes with strictly increasing, deliberately sparse `seq` values.
///
/// Sparse because `seq` is a `bigserial` shared across scopes, so contiguity is not something a
/// real batch has — and a test that only ever generated `1, 2, 3…` would not notice an apply path
/// that assumed it.
fn to_changes(steps: &[Step]) -> Vec<Change> {
    let mut seq = 0u64;
    steps
        .iter()
        .map(|s| {
            // A varying stride keeps the gaps irregular.
            seq += 1 + (s.row as u64 % 3);
            let name = format!("refl:{}", s.row);
            if s.upsert {
                upsert(seq, &name, s.row_version)
            } else {
                tombstone(seq, &name, s.row_version)
            }
        })
        .collect()
}

/// Splits changes into batches of at most `size`, each with a correct `next_cursor`.
fn into_batches(changes: Vec<Change>, size: usize) -> Vec<credsync_protocol::Batch> {
    changes
        .chunks(size.max(1))
        .map(|chunk| {
            let last = chunk.last().map_or(0, |c| c.seq.get());
            let mut b = batch(chunk.to_vec());
            b.next_cursor = Cursor::new(last).expect("valid cursor");
            b
        })
        .collect()
}

proptest! {
    #![proptest_config(config(256))]

    /// DoD 4. The running digest equals one computed from scratch over the surviving rows.
    #[test]
    fn incremental_digest_equals_a_from_scratch_recomputation(
        steps in steps(),
        batch_size in 1usize..6,
    ) {
        let (mut engine, storage) = new_engine();

        for b in into_batches(to_changes(&steps), batch_size) {
            engine.apply_batch(&b).expect("generated batches are always valid");
        }

        let running = engine
            .scope_state(&scope())
            .map_or(credsync_protocol::ScopeDigest::EMPTY, |s| s.digest);
        let recomputed = storage.with(|s| s.digest_from_scratch());

        prop_assert_eq!(
            running,
            recomputed,
            "the incrementally maintained digest drifted from the rows actually stored"
        );
    }

    /// Batching must not change the outcome.
    ///
    /// The same changes delivered as one large batch or as many small ones must leave the client
    /// in an identical state. `docs/spec.md` §5 requires the digest be incremental — *"applying N
    /// changes individually equals applying them as a batch"* — and a client that pulls under a
    /// byte budget on a bad link has no control over how its changes get divided up.
    #[test]
    fn batching_does_not_change_the_result(steps in steps()) {
        let changes = to_changes(&steps);

        let (mut one, one_storage) = new_engine();
        for b in into_batches(changes.clone(), usize::MAX) {
            one.apply_batch(&b).expect("applies");
        }

        let (mut many, many_storage) = new_engine();
        for b in into_batches(changes, 1) {
            many.apply_batch(&b).expect("applies");
        }

        prop_assert_eq!(
            one.scope_state(&scope()).map(|s| s.digest),
            many.scope_state(&scope()).map(|s| s.digest),
            "one batch and many batches produced different digests"
        );
        prop_assert_eq!(
            one_storage.with(|s| s.live_rows()),
            many_storage.with(|s| s.live_rows()),
            "one batch and many batches produced different rows"
        );
    }

    /// The digest persisted alongside the rows matches the one held in memory.
    ///
    /// They are written in the same transaction, so a disagreement would mean a restart resumed
    /// from a digest describing rows the database does not hold — divergence manufactured by the
    /// client itself, on every launch.
    #[test]
    fn the_persisted_digest_matches_the_running_one(steps in steps()) {
        let (mut engine, storage) = new_engine();
        let changes = to_changes(&steps);
        prop_assume!(!changes.is_empty());

        for b in into_batches(changes, 3) {
            engine.apply_batch(&b).expect("applies");
        }

        let running = engine.scope_state(&scope()).expect("state exists").digest;
        let persisted = storage.with(|s| s.digest(&scope()).cloned()).expect("digest persisted");

        prop_assert_eq!(running.to_hex(), persisted);
    }

    /// Whatever the sequence, the cursor ends where the last batch said it would.
    #[test]
    fn the_cursor_ends_at_the_last_next_cursor(steps in steps(), batch_size in 1usize..6) {
        let (mut engine, storage) = new_engine();
        let batches = into_batches(to_changes(&steps), batch_size);
        prop_assume!(!batches.is_empty());

        let expected = batches.last().expect("non-empty").next_cursor;
        for b in &batches {
            engine.apply_batch(b).expect("applies");
        }

        prop_assert_eq!(engine.scope_state(&scope()).expect("state").cursor, expected);
        prop_assert_eq!(storage.with(|s| s.cursor(&scope())), Some(expected));
    }
}
