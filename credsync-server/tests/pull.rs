//! CS-14: pull pagination, against a real Postgres.
//!
//! Not a mock. The definition of done says so, and the reason is that the interesting parts of
//! this slice are the parts a mock would invent: `bigserial` allocating `seq` under concurrent
//! inserts, the check constraints refusing a malformed row, `WHERE scope =` isolating a tenant,
//! and `ORDER BY seq` meaning what it says. A mock would agree with whatever the code expected.
//!
//! # Running these
//!
//! They need `CREDSYNC_TEST_DATABASE_URL`, and **fail loudly when it is absent** rather than
//! skipping. A test that silently passes when its database is missing is worse than no test: the
//! suite goes green on a machine that never ran it, and nobody notices until the day it matters.
//!
//! ```sh
//! export CREDSYNC_TEST_DATABASE_URL=postgres://credsync@127.0.0.1:55432/credsync_test
//! cargo test -p credsync-server
//! ```
//!
//! CI supplies it from a `services: postgres` container; see `.github/workflows/rust.yml`.

// This file needs the `postgres` feature: it drives a real database. With the feature off (which
// is how `credsync-sim` depends on this crate) it compiles to an empty test binary rather than a
// build failure.
#![cfg(feature = "postgres")]
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use credsync_protocol::{Cursor, HexString, ScopeId};
use credsync_server::pull::{Compressor, fill_batch};
use credsync_server::{db, error::ServerError};
use tokio_postgres::{Client, NoTls};

/// A compressor with a fixed ratio, so a test can reason about the budget rather than measure it.
struct Ratio(usize);

impl Compressor for Ratio {
    fn compressed_len(&self, bytes: &[u8]) -> usize {
        bytes.len().div_ceil(self.0.max(1))
    }
}

/// Connects and migrates. Does **not** clear the tables.
///
/// Truncating would be the obvious thing and is wrong here: `cargo test` runs these concurrently
/// against one database, so a truncate in one test deletes another test's rows mid-walk. The
/// first draft did exactly that and six of eleven tests failed in ways that looked like ordering
/// bugs.
///
/// Instead each test owns a unique scope. That is also closer to the real deployment — one shared
/// `bigserial` log holding many tenants' changes interleaved — so the isolation the scope filter
/// provides is exercised by every test rather than only the one that names it.
async fn fresh() -> Client {
    let url = std::env::var("CREDSYNC_TEST_DATABASE_URL").unwrap_or_else(|_| {
        panic!(
            "CREDSYNC_TEST_DATABASE_URL is not set.\n\
             These tests run against a real Postgres and will not pretend otherwise. Start one \
             and export the URL, for example:\n  \
             postgres://credsync@127.0.0.1:55432/credsync_test"
        )
    });

    let (client, connection) = tokio_postgres::connect(&url, NoTls)
        .await
        .expect("connects to the test database");
    tokio::spawn(async move {
        // The connection drives the protocol; dropping it would close the socket mid-test.
        let _ = connection.await;
    });

    db::migrate(&client).await.expect("migrates");
    client
}

/// A scope nobody else will touch, in this run or any previous one.
///
/// Two levels of uniqueness, both learned by getting it wrong:
///
/// - **Per test**, because `cargo test` runs these concurrently against one database. The first
///   draft truncated the tables instead and six of eleven tests failed in ways that looked like
///   ordering bugs.
/// - **Per run**, because the test database is a long-lived local cluster rather than a fresh
///   container. Without this a second `cargo test` sees the first run's rows and every count
///   assertion doubles.
///
/// The test name is in there so a failure message names the test that produced the scope.
fn unique_scope(test: &str) -> String {
    use std::sync::OnceLock;
    static RUN: OnceLock<u128> = OnceLock::new();

    // Not the process id alone. Operating systems reuse pids, and the test cluster is long-lived
    // (D-059), so a later run can be handed an earlier run's pid and then read its rows — the
    // count assertions would fail intermittently, which is the worst kind of failure to debug.
    // Nanoseconds since the epoch do not repeat between runs on any machine this will run on.
    let run = RUN.get_or_init(|| {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_nanos())
    });
    format!("inst:{test}:{run}")
}

fn scope(s: &str) -> ScopeId {
    ScopeId::new(s).expect("valid scope")
}

fn digest() -> HexString {
    HexString::new("00000000000000000000000000000000").expect("valid hex")
}

/// Appends an upsert whose snapshot is `filler` bytes of repeated text.
async fn append(client: &Client, scope: &str, id: &str, filler: usize, version: i64) -> i64 {
    let snapshot = serde_json::json!({ "body": "x".repeat(filler) });
    db::append_change(
        client,
        &db::NewChange {
            scope,
            entity: "reflections",
            entity_id: id,
            op: "upsert",
            snapshot: Some(&snapshot),
            row_version: version,
            schema_version: 1,
        },
    )
    .await
    .expect("appends")
}

// -------------------------------------------------------------------------------------------
// Ordering
// -------------------------------------------------------------------------------------------

/// Pull is strictly `seq`-ordered within a scope.
#[tokio::test]
async fn pull_is_strictly_seq_ordered_within_a_scope() {
    let client = fresh().await;
    let sc = unique_scope("pull_is_strictly_seq_ordered_within_a_scope");
    for n in 0..12 {
        append(&client, &sc, &format!("r{n}"), 16, 1).await;
    }

    let changes = db::changes_after(&client, &scope(&sc), 0, 100)
        .await
        .expect("reads")
        .changes;

    assert_eq!(changes.len(), 12);
    let seqs: Vec<u64> = changes.iter().map(|c| c.seq.get()).collect();
    let mut sorted = seqs.clone();
    sorted.sort_unstable();
    assert_eq!(seqs, sorted, "changes came back out of order");
    assert!(
        seqs.windows(2).all(|w| w[0] < w[1]),
        "seqs must strictly increase, got {seqs:?}"
    );
}

/// A cursor walks forward and never re-delivers.
#[tokio::test]
async fn a_cursor_walks_forward_without_repeating() {
    let client = fresh().await;
    let sc = unique_scope("a_cursor_walks_forward_without_repeating");
    for n in 0..9 {
        append(&client, &sc, &format!("r{n}"), 16, 1).await;
    }

    let mut cursor = 0u64;
    let mut seen: Vec<u64> = Vec::new();
    for _ in 0..10 {
        let changes = db::changes_after(&client, &scope(&sc), cursor, 3)
            .await
            .expect("reads")
            .changes;
        if changes.is_empty() {
            break;
        }
        for c in &changes {
            assert!(
                c.seq.get() > cursor,
                "seq {} is at or below the cursor {cursor}",
                c.seq.get()
            );
            seen.push(c.seq.get());
        }
        cursor = changes.last().expect("non-empty").seq.get();
    }

    assert_eq!(seen.len(), 9, "the walk did not cover every change");
    let mut unique = seen.clone();
    unique.dedup();
    assert_eq!(unique.len(), seen.len(), "a change was delivered twice");
}

// -------------------------------------------------------------------------------------------
// Scope isolation — `docs/spec.md` §8
// -------------------------------------------------------------------------------------------

/// Pull cannot cross scopes, whatever cursor the client sends.
///
/// The client controls `cursor` completely and can send any value it likes. What it cannot do is
/// reach another scope's rows, because the query never looks at them.
///
/// The cursors are taken from the **actual** seqs allocated, not written as literals. An earlier
/// version used `0, 1, 5, 11`, and since `seq` is shared by every scope, every concurrent test and
/// every earlier run, those were all far below every row this test wrote — so each query returned
/// all of scope A and no cursor ever landed between B's rows. The test could not have failed for
/// the reason its own comment gave.
#[tokio::test]
async fn pull_cannot_cross_scopes_whatever_the_cursor() {
    let client = fresh().await;
    let a = unique_scope("cross_a");
    let b = unique_scope("cross_b");

    // Interleaved, so the two scopes' seqs genuinely alternate in one shared bigserial.
    let mut a_seqs = Vec::new();
    let mut b_seqs = Vec::new();
    for n in 0..6 {
        a_seqs.push(append(&client, &a, &format!("a{n}"), 16, 1).await);
        b_seqs.push(append(&client, &b, &format!("b{n}"), 16, 1).await);
    }

    // Cursors that really do fall before, between and after scope B's rows.
    let cursors = [
        0u64,
        u64::try_from(a_seqs[0] - 1).expect("positive"),
        u64::try_from(b_seqs[0]).expect("positive"),
        u64::try_from(b_seqs[2]).expect("positive"),
        u64::try_from(*b_seqs.last().expect("six rows")).expect("positive"),
    ];

    for cursor in cursors {
        let changes = db::changes_after(&client, &scope(&a), cursor, 100)
            .await
            .expect("reads")
            .changes;

        for c in &changes {
            assert!(
                c.entity_id.as_str().starts_with('a'),
                "cursor {cursor} leaked a row from another scope: {}",
                c.entity_id
            );
            assert!(
                !b_seqs.contains(&i64::try_from(c.seq.get()).expect("fits")),
                "cursor {cursor} returned a seq that belongs to scope B"
            );
        }
    }

    // And the middle cursor really does sit between B's rows, or the test proves less than it says.
    let middle = u64::try_from(b_seqs[2]).expect("positive");
    assert!(
        b_seqs
            .iter()
            .any(|s| u64::try_from(*s).expect("fits") > middle),
        "the chosen cursor was not actually between scope B's rows"
    );
}

/// An unknown scope returns nothing rather than everything.
///
/// The failure mode worth guarding: a missing or mistyped filter that silently becomes "all rows".
#[tokio::test]
async fn an_unknown_scope_returns_nothing() {
    let client = fresh().await;
    let sc = unique_scope("an_unknown_scope_returns_nothing");
    for n in 0..5 {
        append(&client, &sc, &format!("a{n}"), 16, 1).await;
    }

    let changes = db::changes_after(&client, &scope("inst:nobody-at-all"), 0, 100)
        .await
        .expect("reads")
        .changes;
    assert!(
        changes.is_empty(),
        "an unknown scope saw {} rows",
        changes.len()
    );
}

// -------------------------------------------------------------------------------------------
// The compressed byte budget — `docs/spec.md` §2
// -------------------------------------------------------------------------------------------

/// Batches are capped by compressed bytes, and `has_more` drives continuation.
#[tokio::test]
async fn batches_are_capped_by_compressed_bytes_not_row_count() {
    let client = fresh().await;
    let sc = unique_scope("batches_are_capped_by_compressed_bytes_not_row_count");
    for n in 0..40 {
        append(&client, &sc, &format!("r{n}"), 500, 1).await;
    }

    let candidates = db::changes_after(&client, &scope(&sc), 0, 100)
        .await
        .expect("reads")
        .changes;
    assert_eq!(candidates.len(), 40);

    let batch = fill_batch(
        &scope(&sc),
        Cursor::START,
        candidates,
        false,
        2_000,
        &Ratio(1),
        digest(),
    )
    .expect("fills");

    assert!(
        batch.changes.len() < 40,
        "the budget did not bind: all 40 changes fit in 2,000 bytes"
    );
    assert!(!batch.changes.is_empty(), "the budget starved the batch");
    assert!(batch.has_more, "has_more must drive continuation");
    assert_eq!(
        batch.next_cursor.get(),
        batch.changes.last().expect("non-empty").seq.get(),
        "next_cursor must cover exactly what was sent"
    );
}

/// The same changes, better compression, more of them fit.
///
/// This is what distinguishes a compressed-byte budget from an uncompressed one. If the server
/// measured raw bytes, or counted rows, both runs below would produce identical batches.
#[tokio::test]
async fn a_better_compression_ratio_fits_more_changes() {
    let client = fresh().await;
    let sc = unique_scope("a_better_compression_ratio_fits_more_changes");
    for n in 0..40 {
        append(&client, &sc, &format!("r{n}"), 500, 1).await;
    }
    let candidates = db::changes_after(&client, &scope(&sc), 0, 100)
        .await
        .expect("reads")
        .changes;

    let lean = fill_batch(
        &scope(&sc),
        Cursor::START,
        candidates.clone(),
        false,
        2_000,
        &Ratio(1),
        digest(),
    )
    .expect("fills");
    let fat = fill_batch(
        &scope(&sc),
        Cursor::START,
        candidates,
        false,
        2_000,
        &Ratio(8),
        digest(),
    )
    .expect("fills");

    assert!(
        fat.changes.len() > lean.changes.len(),
        "compressible changes must fill a larger batch: {} vs {}",
        fat.changes.len(),
        lean.changes.len()
    );
}

/// A single change larger than the whole budget is still delivered, alone.
///
/// `docs/spec.md` §2. Not a corner case: a snapshot may be 256 KB and the default budget is 100 KB.
/// Refusing to send it would wedge the scope permanently — the client asks, the server has
/// something it will not send, and the cursor never moves again.
#[tokio::test]
async fn one_oversized_change_is_delivered_alone_rather_than_stalling_the_cursor() {
    let client = fresh().await;
    let sc =
        unique_scope("one_oversized_change_is_delivered_alone_rather_than_stalling_the_cursor");
    append(&client, &sc, "huge", 50_000, 1).await;
    append(&client, &sc, "small", 16, 1).await;

    let candidates = db::changes_after(&client, &scope(&sc), 0, 100)
        .await
        .expect("reads")
        .changes;

    let batch = fill_batch(
        &scope(&sc),
        Cursor::START,
        candidates,
        false,
        100,
        &Ratio(1),
        digest(),
    )
    .expect("fills");

    assert_eq!(
        batch.changes.len(),
        1,
        "the oversized change must go alone, not not at all"
    );
    assert!(batch.has_more, "the small change is still waiting");
    assert!(
        batch.next_cursor.get() > 0,
        "the cursor must move, or the scope is stuck forever"
    );
}

/// And the walk continues past it, rather than looping on the same oversized change.
#[tokio::test]
async fn the_walk_continues_past_an_oversized_change() {
    let client = fresh().await;
    let sc = unique_scope("the_walk_continues_past_an_oversized_change");
    append(&client, &sc, "huge", 50_000, 1).await;
    append(&client, &sc, "small", 16, 1).await;

    let mut cursor = Cursor::START;
    let mut delivered = 0usize;
    for _ in 0..5 {
        let candidates = db::changes_after(&client, &scope(&sc), cursor.get(), 100)
            .await
            .expect("reads")
            .changes;
        if candidates.is_empty() {
            break;
        }
        let batch = fill_batch(
            &scope(&sc),
            cursor,
            candidates,
            false,
            100,
            &Ratio(1),
            digest(),
        )
        .expect("fills");
        assert!(
            !batch.changes.is_empty(),
            "a cycle delivered nothing; the cursor is stuck at {}",
            cursor.get()
        );
        delivered += batch.changes.len();
        assert!(
            batch.next_cursor.get() > cursor.get(),
            "the cursor did not advance past {}",
            cursor.get()
        );
        cursor = batch.next_cursor;
    }

    assert_eq!(delivered, 2, "both changes must eventually arrive");
}

/// An empty scope produces an empty batch that does not claim more.
#[tokio::test]
async fn an_empty_scope_produces_an_empty_batch() {
    let client = fresh().await;
    let candidates = db::changes_after(&client, &scope(&unique_scope("empty_scope")), 0, 100)
        .await
        .expect("reads")
        .changes;

    let batch = fill_batch(
        &scope(&unique_scope("empty_scope")),
        Cursor::START,
        candidates,
        false,
        10_000,
        &Ratio(1),
        digest(),
    )
    .expect("fills");

    assert!(batch.changes.is_empty());
    assert!(!batch.has_more);
    assert_eq!(batch.next_cursor, Cursor::START);
}

// -------------------------------------------------------------------------------------------
// The schema refuses what the protocol forbids
// -------------------------------------------------------------------------------------------

/// The database refuses an upsert with no snapshot, and a delete carrying one.
///
/// `docs/spec.md` §1: snapshots, not diffs, and deletes are tombstones. Enforced by a check
/// constraint rather than by every writer remembering — a host writes these rows through its own
/// outbox, and the rule has to hold for code this project never sees.
#[tokio::test]
async fn the_schema_enforces_the_op_snapshot_rule() {
    let client = fresh().await;
    let sc = unique_scope("the_schema_enforces_the_op_snapshot_rule");

    let upsert_without = db::append_change(
        &client,
        &db::NewChange {
            scope: &sc,
            entity: "reflections",
            entity_id: "r1",
            op: "upsert",
            snapshot: None,
            row_version: 1,
            schema_version: 1,
        },
    )
    .await;
    assert!(
        matches!(upsert_without, Err(ServerError::Database { .. })),
        "an upsert with no snapshot was accepted"
    );

    let snapshot = serde_json::json!({ "body": "x" });
    let delete_with = db::append_change(
        &client,
        &db::NewChange {
            scope: &sc,
            entity: "reflections",
            entity_id: "r1",
            op: "delete",
            snapshot: Some(&snapshot),
            row_version: 1,
            schema_version: 1,
        },
    )
    .await;
    assert!(
        matches!(delete_with, Err(ServerError::Database { .. })),
        "a delete carrying a snapshot was accepted"
    );
}

/// A tombstone is stored and read back with no snapshot.
#[tokio::test]
async fn a_tombstone_round_trips_without_a_snapshot() {
    let client = fresh().await;
    let sc = unique_scope("a_tombstone_round_trips_without_a_snapshot");
    db::append_change(
        &client,
        &db::NewChange {
            scope: &sc,
            entity: "reflections",
            entity_id: "r1",
            op: "delete",
            snapshot: None,
            row_version: 2,
            schema_version: 1,
        },
    )
    .await
    .expect("appends");

    let changes = db::changes_after(&client, &scope(&sc), 0, 10)
        .await
        .expect("reads")
        .changes;

    assert_eq!(changes.len(), 1);
    assert_eq!(changes[0].op, credsync_protocol::Op::Delete);
    assert!(
        changes[0].snapshot.is_none(),
        "a tombstone carried a snapshot"
    );
}

/// The row ceiling sets `has_more`, even when every row fits the byte budget.
///
/// `fill_batch` cannot tell from its side whether the read was truncated: a hundred small
/// tombstones fit a 100 KB budget easily, so every candidate is chosen and `has_more` would read
/// `false` while rows remain. The scope would then sit still until something else happened to it.
#[tokio::test]
async fn the_row_ceiling_sets_has_more_even_when_everything_fits_the_budget() {
    let client = fresh().await;
    let sc = unique_scope("row_ceiling");
    for n in 0..12 {
        append(&client, &sc, &format!("r{n}"), 8, 1).await;
    }

    let page = db::changes_after(&client, &scope(&sc), 0, 4)
        .await
        .expect("reads");
    assert_eq!(
        page.changes.len(),
        4,
        "the ceiling should cap the read at 4"
    );
    assert!(page.more_beyond, "eight rows remain beyond the ceiling");

    let batch = fill_batch(
        &scope(&sc),
        Cursor::START,
        page.changes,
        page.more_beyond,
        10_000_000,
        &Ratio(1),
        digest(),
    )
    .expect("fills");

    assert_eq!(
        batch.changes.len(),
        4,
        "the budget did not bind; the ceiling did"
    );
    assert!(
        batch.has_more,
        "has_more must be true when the ROW ceiling truncated, not only when the budget did"
    );
}
