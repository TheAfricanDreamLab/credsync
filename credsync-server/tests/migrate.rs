//! CS-17 / #63: the migration must not wedge live writers.
//!
//! `migrate()` runs DDL, which needs `ACCESS EXCLUSIVE` on `sync_changes`. Postgres grants lock
//! requests in order, so an open write transaction blocks the DDL and every writer arriving
//! afterwards queues *behind the DDL* rather than behind the transaction it could have shared the
//! table with. If the open transaction cannot commit until one of those queued writers returns,
//! nothing moves again — and the deadlock detector never fires, because the blocking session is
//! waiting on its client rather than on a lock.
//!
//! That is a rolling deploy: a new instance migrating while the old one serves writes.

// This file needs the `postgres` feature: it drives a real database. With the feature off (which
// is how `credsync-sim` depends on this crate) it compiles to an empty test binary rather than a
// build failure.
#![cfg(feature = "postgres")]
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use credsync_server::db;
use std::time::{Duration, Instant};
use tokio_postgres::{Client, NoTls};

/// These tests run against their **own cluster**, not the shared one.
///
/// They hold write transactions open for tens of seconds on purpose. `changes_after` withholds
/// rows whose inserting transaction may still be in flight, and decides that from
/// `pg_snapshot_xmin` — which reflects the oldest transaction running anywhere in the *cluster*,
/// since transaction ids are cluster-wide. Measured at CS-17: a transaction held in a completely
/// unrelated database still made a committed row read back as 0 of 1.
///
/// So on a shared cluster these tests would withhold rows from every concurrent pull test and make
/// the suite flake for reasons unrelated to the code under test. A separate database would not
/// help; a separate cluster does. `scripts/test-postgres.sh` starts both. The underlying liveness
/// problem is #62.
fn url() -> String {
    std::env::var("CREDSYNC_TEST_ISOLATED_DATABASE_URL").unwrap_or_else(|_| {
        panic!(
            "CREDSYNC_TEST_ISOLATED_DATABASE_URL is not set.\n\
             These tests need their own cluster -- see the note above `url()`.\n\
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

fn unique_scope(test: &str) -> String {
    let run = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos());
    format!("inst:{test}:{run}")
}

async fn insert(client: &Client, scope: &str, id: &str) -> Result<(), tokio_postgres::Error> {
    let snapshot = serde_json::json!({ "body": "x" });
    client
        .execute(
            "INSERT INTO sync_changes
                 (scope, entity, entity_id, op, snapshot, row_version, schema_version)
             VALUES ($1, 'reflections', $2, 'upsert', $3, 1, 1)",
            &[&scope, &id, &snapshot],
        )
        .await
        .map(|_| ())
}

/// A lock that makes these tests run one at a time.
///
/// Each test here deliberately holds a write transaction open while something else migrates, which
/// is exactly the interference they exist to study — and exactly what they do to *each other* when
/// `cargo test` runs them in parallel. One test's held transaction blocks another's baseline
/// migration, and the second fails for reasons that have nothing to do with what it is asserting.
///
/// A Postgres advisory lock rather than a Rust mutex: each `#[tokio::test]` gets its own runtime,
/// so a `tokio::sync::Mutex` would be shared across runtimes, and a `std::sync::Mutex` cannot be
/// held across an `.await`. This is session-scoped, so it is released when the returned client is
/// dropped — including when a test panics, which a hand-rolled guard would have to remember.
///
/// A different key from `MIGRATION_LOCK`, obviously; taking that one would be testing the lock
/// against itself.
const TEST_SERIALISER: i64 = 0x0000_c2ed_5900_7e57;

async fn serialised() -> Client {
    let client = connect().await;
    client
        .execute("SELECT pg_advisory_lock($1)", &[&TEST_SERIALISER])
        .await
        .expect("takes the serialiser");
    client
}

/// A migration running against an open write transaction must not block other writers.
///
/// Without a `lock_timeout` this hangs indefinitely: the migration queues for `ACCESS EXCLUSIVE`
/// behind the open transaction, and the second writer queues behind the migration. With one, the
/// migration gives up, the queue drains, and the writer gets through.
///
/// The assertion is on **the writer**, not on the migration. Whether the migration succeeds on a
/// busy database is a scheduling question; whether it is allowed to take the database down with it
/// is not.
#[tokio::test]
async fn a_migration_does_not_block_writers_behind_an_open_transaction() {
    let _serialiser = serialised().await;
    let setup = connect().await;
    db::migrate(&setup).await.expect("baseline migration");

    let scope = unique_scope("migrate_no_wedge");

    // W: a writer that opens a transaction and holds it, as a host mid-request would.
    let holder = connect().await;
    holder.batch_execute("BEGIN").await.expect("begins");
    insert(&holder, &scope, "held").await.expect("writes");

    // M: an instance starting up and migrating, while W is open.
    let migrator = connect().await;
    let migration = tokio::spawn(async move { db::migrate(&migrator).await });

    // Give the migration time to reach its lock request and start queueing.
    tokio::time::sleep(Duration::from_millis(300)).await;

    // W': another writer, arriving after the migration queued. This is the one that must not be
    // wedged. Before the fix it waits forever behind the DDL.
    let second = connect().await;
    let started = Instant::now();
    let wrote = tokio::time::timeout(
        Duration::from_secs(20),
        insert(&second, &scope, "arrived-later"),
    )
    .await;

    let elapsed = started.elapsed();

    // Let the holder go, whatever happened, so a failure here does not leave the cluster wedged
    // for every other test.
    holder.batch_execute("COMMIT").await.ok();
    let _ = migration.await;

    match wrote {
        Ok(Ok(())) => {}
        Ok(Err(e)) => panic!("the later writer failed: {e}"),
        Err(_) => panic!(
            "a writer arriving after a migration was still blocked after {elapsed:?}; \
             the migration queued ahead of it and wedged the table"
        ),
    }
}

/// The migration itself gives up rather than queueing forever.
///
/// Bounded by `lock_timeout` times the retry count. The exact number matters less than the fact
/// that there is one: a start-up that hangs silently is indistinguishable from a slow one.
#[tokio::test]
async fn a_migration_blocked_by_an_open_transaction_gives_up_rather_than_hanging() {
    let _serialiser = serialised().await;
    let setup = connect().await;
    db::migrate(&setup).await.expect("baseline migration");

    let scope = unique_scope("migrate_gives_up");

    let holder = connect().await;
    holder.batch_execute("BEGIN").await.expect("begins");
    insert(&holder, &scope, "held").await.expect("writes");

    let migrator = connect().await;
    let started = Instant::now();
    let outcome = tokio::time::timeout(Duration::from_secs(90), db::migrate(&migrator)).await;
    let elapsed = started.elapsed();

    holder.batch_execute("COMMIT").await.ok();

    assert!(
        outcome.is_ok(),
        "the migration was still waiting after {elapsed:?} rather than giving up"
    );
}

/// And once the blocker is gone, the migration succeeds on a retry.
///
/// Giving up is only correct if it is paired with coming back — otherwise a deploy that lands
/// during a busy moment simply never migrates.
#[tokio::test]
async fn a_migration_succeeds_once_the_blocking_transaction_ends() {
    let _serialiser = serialised().await;
    let setup = connect().await;
    db::migrate(&setup).await.expect("baseline migration");

    let scope = unique_scope("migrate_retry_succeeds");

    let holder = connect().await;
    holder.batch_execute("BEGIN").await.expect("begins");
    insert(&holder, &scope, "held").await.expect("writes");

    let migrator = connect().await;
    let migration = tokio::spawn(async move { db::migrate(&migrator).await });

    // Hold it long enough to force at least one failed attempt, then release.
    tokio::time::sleep(Duration::from_millis(2_000)).await;
    holder.batch_execute("COMMIT").await.expect("commits");

    let result = tokio::time::timeout(Duration::from_secs(60), migration)
        .await
        .expect("the migration should finish once the blocker is gone")
        .expect("task");

    result.expect("the migration should succeed on a retry once the table is free");
}

/// Concurrent migrations still serialise correctly — the property from CS-14 that must survive.
///
/// The advisory lock became `pg_try_advisory_lock` plus a retry rather than a blocking acquire, so
/// this asserts the original guarantee was not traded away for the new one.
#[tokio::test]
async fn concurrent_migrations_all_succeed() {
    let _serialiser = serialised().await;

    let mut tasks = Vec::new();
    for _ in 0..8 {
        tasks.push(tokio::spawn(async {
            let client = connect().await;
            db::migrate(&client).await
        }));
    }

    for (n, task) in tasks.into_iter().enumerate() {
        task.await
            .expect("task")
            .unwrap_or_else(|e| panic!("concurrent migration {n} failed: {e}"));
    }
}
