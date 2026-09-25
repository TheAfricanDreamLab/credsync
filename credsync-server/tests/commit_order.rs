//! CS-14's commit-order guard, on a cluster of its own.
//!
//! # Why this is not in `pull.rs`
//!
//! This test holds a write transaction open on purpose — that is the entire scenario. But
//! `changes_after` decides what to withhold from `pg_snapshot_xmin`, the oldest transaction running
//! anywhere in the **cluster**, so while this test's transaction is open every *other* pull test
//! running in parallel has its committed rows withheld.
//!
//! That is not hypothetical. It failed CI on #70: `a_cursor_walks_forward_without_repeating` saw
//! eight of its nine changes and reported "the walk did not cover every change", which reads like
//! a pagination bug and is nothing of the kind.
//!
//! So it runs against the isolated cluster, alongside the migration tests and under the same
//! advisory-lock serialiser, where the only thing its open transaction can delay is itself.
//!
//! The underlying liveness problem is #62. This is a workaround for the suite, not a fix for it.

// This file needs the `postgres` feature: it drives a real database. With the feature off (which
// is how `credsync-sim` depends on this crate) it compiles to an empty test binary rather than a
// build failure.
#![cfg(feature = "postgres")]
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use credsync_protocol::ScopeId;
use credsync_server::db;
use tokio_postgres::{Client, NoTls};

/// The isolated cluster. See the module docs, and `scripts/test-postgres.sh`.
fn url() -> String {
    std::env::var("CREDSYNC_TEST_ISOLATED_DATABASE_URL").unwrap_or_else(|_| {
        panic!(
            "CREDSYNC_TEST_ISOLATED_DATABASE_URL is not set.\n\
             This test holds a transaction open and needs its own cluster -- see the module docs.\n\
             Start both with `eval \"$(./scripts/test-postgres.sh)\"`."
        )
    })
}

async fn connect() -> Client {
    let (client, connection) = tokio_postgres::connect(&url(), NoTls)
        .await
        .expect("connects");
    tokio::spawn(async move {
        let _ = connection.await;
    });
    client
}

async fn fresh() -> Client {
    let client = connect().await;
    db::migrate(&client).await.expect("migrates");
    client
}

/// The same key the migration tests use, so the two cannot overlap on this cluster.
const TEST_SERIALISER: i64 = 0x0000_c2ed_5900_7e57;

async fn serialised() -> Client {
    let client = connect().await;
    client
        .execute("SELECT pg_advisory_lock($1)", &[&TEST_SERIALISER])
        .await
        .expect("takes the serialiser");
    client
}

/// Unique per test and per run, so nothing collides.
fn unique_scope(test: &str) -> String {
    use std::sync::OnceLock;
    static RUN: OnceLock<u128> = OnceLock::new();
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
// The commit-order gap
// -------------------------------------------------------------------------------------------

/// A change committed *after* a higher `seq` is never skipped.
///
/// `seq` is a `bigserial`, which allocates when the `INSERT` runs — but a row becomes visible when
/// its transaction commits, and those orders differ. Demonstrated against Postgres 14 before this
/// guard existed:
///
/// ```text
///   A: BEGIN; INSERT -> seq 1; (still open)
///   B:        INSERT -> seq 2; COMMIT
///   reader sees: seq 2 only
/// ```
///
/// A client would take `seq 2`, advance its cursor past `1`, and never be given change 1 — silent
/// loss, invisible to every ordering check because the batch it received was perfectly ordered.
///
/// Found by review on #59. The simulator could not have caught it: its server is single-threaded,
/// so no two writes are ever in flight at once.
#[tokio::test]
async fn a_change_committed_after_a_higher_seq_is_not_skipped() {
    let _serialiser = serialised().await;
    let sc = unique_scope("commit_order_gap");
    let reader = fresh().await;

    // A writer that takes a seq and holds its transaction open.
    let slow = connect().await;
    slow.batch_execute("BEGIN").await.expect("begins");
    let snapshot = serde_json::json!({ "body": "written first, committed last" });
    db::append_change(
        &slow,
        &db::NewChange {
            scope: &sc,
            entity: "reflections",
            entity_id: "slow",
            op: "upsert",
            snapshot: Some(&snapshot),
            row_version: 1,
            schema_version: 1,
        },
    )
    .await
    .expect("appends inside the open transaction");

    // A second writer takes a higher seq and commits immediately.
    append(&reader, &sc, "fast", 16, 1).await;

    // While the first transaction is still open, the reader must be given NOTHING — withholding
    // the later change rather than delivering it out of order.
    let during = db::changes_after(&reader, &scope(&sc), 0, 100)
        .await
        .expect("reads")
        .changes;
    assert!(
        during.is_empty(),
        "the reader was handed {} change(s) while an earlier seq was still uncommitted; \
         advancing the cursor past them loses the earlier one forever",
        during.len()
    );

    slow.batch_execute("COMMIT").await.expect("commits");

    // Once it commits, both arrive, in seq order.
    let after = db::changes_after(&reader, &scope(&sc), 0, 100)
        .await
        .expect("reads")
        .changes;
    assert_eq!(
        after.len(),
        2,
        "both changes must arrive once the writer commits"
    );
    assert!(
        after[0].seq.get() < after[1].seq.get(),
        "the pair arrived out of seq order"
    );
    assert_eq!(
        after[0].entity_id.as_str(),
        "slow",
        "the earlier seq must come first"
    );
}

/// **A lower `seq` still in flight is never skipped, even when its writer holds a higher xid.**
///
/// The interleaving that defeated the original guard (#74). It is not exotic: a host that writes
/// domain state before its change-log row takes its xid early and its `seq` late, which is exactly
/// what `docs/spec.md` §1 prescribes.
///
/// ```text
///   T0: BEGIN; INSERT elsewhere     -> takes the LOWER xid
///   T1: BEGIN; INSERT sync_changes  -> takes the higher xid, and the LOWER seq   (stays open)
///   T0:        INSERT sync_changes  -> the higher seq; COMMIT
/// ```
///
/// The old guard admitted T0's row because its `xmin` was below the snapshot's — and T1's lower
/// `seq` was still uncommitted underneath it. A client would have advanced its cursor past a change
/// it would never be given.
#[tokio::test]
async fn a_lower_seq_in_flight_is_not_skipped_when_its_writer_holds_a_higher_xid() {
    let _serialiser = serialised().await;
    let reader = fresh().await;
    let sc = unique_scope("xid_order_does_not_bound_seq_order");

    // T0 takes its xid first, by writing somewhere else entirely.
    let t0 = connect().await;
    t0.batch_execute("BEGIN").await.expect("begins");
    append(&t0, &unique_scope("xid_warmup"), "warmup", 16, 1).await;

    // T1 then takes a later xid and an EARLIER seq, and stays open.
    let t1 = connect().await;
    t1.batch_execute("BEGIN").await.expect("begins");
    db::lock_for_write(&t1)
        .await
        .expect("takes the writer lock");
    append(&t1, &sc, "early", 16, 1).await;

    // T0 now takes a LATER seq and commits, while T1 is still open.
    append(&t0, &sc, "late", 16, 1).await;
    t0.batch_execute("COMMIT").await.expect("commits");

    // A reader must be given nothing: the only visible row sits above an uncommitted one.
    let during = db::changes_after(&reader, &scope(&sc), 0, 100)
        .await
        .expect("reads")
        .changes;
    assert!(
        during.is_empty(),
        "the reader was handed {} change(s) while a LOWER seq was still uncommitted; advancing \
         the cursor past them loses the earlier one for ever. seqs delivered: {:?}",
        during.len(),
        during.iter().map(|c| c.seq.get()).collect::<Vec<_>>()
    );

    t1.batch_execute("COMMIT").await.expect("commits");

    // Once both commit, both arrive, in seq order.
    let after = db::changes_after(&reader, &scope(&sc), 0, 100)
        .await
        .expect("reads")
        .changes;
    assert_eq!(
        after.len(),
        2,
        "both changes must arrive once the writers commit"
    );
    assert!(
        after[0].seq.get() < after[1].seq.get(),
        "the pair arrived out of seq order"
    );
    assert_eq!(
        after[0].entity_id.as_str(),
        "early",
        "the earlier seq must come first"
    );
}

/// An unrelated transaction does not withhold anything. `docs/spec.md` §5, issue #62.
///
/// The liveness half. The old guard keyed on `pg_snapshot_xmin`, which reflects the oldest
/// transaction running anywhere in the **cluster** — so a `pg_dump`, an analytics query, or one
/// leaked idle-in-transaction connection stalled sync for every scope and every client. The
/// watermark waits only for change-log writers.
#[tokio::test]
async fn an_unrelated_open_transaction_does_not_withhold_anything() {
    let _serialiser = serialised().await;
    let writer = fresh().await;
    let sc = unique_scope("unrelated_transaction");

    append(&writer, &sc, "r1", 16, 1).await;

    // Something else entirely, holding a transaction open and touching nothing of ours.
    let bystander = connect().await;
    bystander.batch_execute("BEGIN").await.expect("begins");
    bystander
        .query_one("SELECT txid_current()", &[])
        .await
        .expect("takes an xid");

    let delivered = db::changes_after(&writer, &scope(&sc), 0, 100)
        .await
        .expect("reads")
        .changes;

    bystander.batch_execute("COMMIT").await.ok();

    assert_eq!(
        delivered.len(),
        1,
        "a committed row was withheld because an unrelated transaction was open somewhere in the \
         cluster; that is #62, and sync stalls for every scope while it lasts"
    );
}
