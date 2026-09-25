//! The four queries the server actually needs.
//!
//! Written as SQL rather than through a query builder. The server owns two tables, and the two
//! statements below are the ones the byte budget and the cursor walk depend on — hiding them
//! behind a builder would hide the only part worth reading closely.

use crate::error::ServerError;
use crate::pull::ChangeRow;
use credsync_protocol::{Change, ScopeId};
use tokio_postgres::Client;

/// A fixed key for the advisory lock that serialises migration.
///
/// Arbitrary but stable: every process that migrates this schema must pick the same number, or the
/// lock protects nothing. Derived from "credsync" so a collision with another application's
/// advisory lock on the same database is unlikely rather than merely hoped for.
const MIGRATION_LOCK: i64 = 0x0000_c2ed_5900_0001;

/// How long the migration will wait for a lock before giving up and trying again.
///
/// Not a performance knob — see [`migrate`] for why queueing is the thing being prevented.
const MIGRATION_LOCK_TIMEOUT_MS: u32 = 1_500;

/// How many times to retry a migration that could not get its locks.
///
/// Ten attempts with linear backoff is roughly twenty seconds of trying. Bounded so a genuinely
/// stuck database fails loudly at start-up rather than a process sitting silently forever, which
/// looks identical to a slow start — and kept short because an orchestrator restarting the whole
/// instance is better observed than a loop retrying quietly inside it.
const MIGRATION_ATTEMPTS: u32 = 10;

/// Applies the schema. Idempotent, so a server may run it on every start.
///
/// # Why this takes a lock
///
/// `CREATE TABLE IF NOT EXISTS` is **not** safe to run concurrently. Two sessions can both find
/// the table absent and both try to create it, and one loses with a duplicate-key error on
/// `pg_class` — the `IF NOT EXISTS` only skips the work, it does not serialise the check against
/// the creation.
///
/// That is not hypothetical. Several server instances starting at once is the normal deployment,
/// and it is exactly what happened in CI: eleven integration tests migrating a fresh database
/// concurrently, three of them failing with `db error`. It passed locally only because the tables
/// already existed from an earlier run, so every check short-circuited and nothing raced.
///
/// A session-level advisory lock serialises the whole migration. It is released explicitly, and
/// also by the session ending, so a process that dies mid-migration does not wedge the next one.
///
/// # Why it also refuses to queue
///
/// The advisory lock serialises migrations against each other. It does nothing about the *other*
/// lock this needs: DDL takes `ACCESS EXCLUSIVE` on `sync_changes`, and Postgres grants lock
/// requests in order. So an open write transaction blocks the DDL, and every writer that arrives
/// afterwards queues **behind the DDL** rather than behind the transaction it could otherwise have
/// shared the table with:
///
/// ```text
///   session W : BEGIN; INSERT INTO sync_changes ...;   (open, idle in transaction)
///   session M : migrate() -> DDL                        waits for ACCESS EXCLUSIVE, blocked by W
///   session W': INSERT INTO sync_changes ...            queued BEHIND M
/// ```
///
/// If `W` cannot commit until `W'` returns — the shape of a host writing state and its change-log
/// row across two statements — nothing moves again. **The deadlock detector does not fire**,
/// because `W` is not waiting on a lock at all; it is waiting on its client, so there is no cycle
/// in the lock graph to find. Observed hanging for over six minutes with no progress and no error.
///
/// A rolling deploy takes exactly this window on every start, so it is a production concern rather
/// than a test artefact. `lock_timeout` closes it: the migration waits briefly and then **fails
/// instead of queueing**, which releases the queue behind it, and retries once the writer is gone.
/// A starting instance can no longer wedge a running one.
///
/// # Errors
/// Returns [`ServerError::Database`] if the lock cannot be taken, the statements cannot be applied,
/// or the locks were still unavailable after every attempt.
pub async fn migrate(client: &Client) -> Result<(), ServerError> {
    let mut last: Option<String> = None;

    for attempt in 0..MIGRATION_ATTEMPTS {
        match try_migrate(client).await {
            Attempt::Done => return Ok(()),
            Attempt::Failed(e) => return Err(e.into()),
            Attempt::Contended { by } => {
                // Somebody else holds the table or the migration lock. Back off and let them
                // finish; this session holds nothing while it waits, so it delays nobody. Linear
                // rather than exponential: the blocker is a transaction, not a congested network,
                // and it will end on its own schedule either way.
                let wait = u64::from(attempt + 1) * 100;
                tokio::time::sleep(std::time::Duration::from_millis(wait)).await;
                last = Some(by);
            }
        }
    }

    Err(ServerError::Database {
        detail: format!(
            "the schema migration could not acquire its locks after {MIGRATION_ATTEMPTS} attempts; \
             something is holding a transaction open on sync_changes ({})",
            last.as_deref().unwrap_or("no further detail")
        ),
    })
}

/// The outcome of one migration attempt.
///
/// `Contended` is deliberately **not** an error. Losing a race for a lock is the expected case when
/// several instances start together, and turning it into an error would mean either retrying real
/// failures or writing an `ERROR` line into the server log every time two deploys overlap.
enum Attempt {
    /// The schema is applied.
    Done,
    /// Somebody else holds a lock this needs. Worth retrying.
    Contended {
        /// Which lock, for the message if the retries run out.
        by: String,
    },
    /// Something actually went wrong. Not worth retrying.
    Failed(tokio_postgres::Error),
}

/// One attempt at the migration, with the locks bounded.
async fn try_migrate(client: &Client) -> Attempt {
    // `lock_timeout` is what stops the DDL queueing ahead of live writers. It does **not** apply to
    // advisory locks, which is why the advisory acquire below is a `try` rather than a blocking
    // wait: leaving that one blocking would move the wedge one step earlier and look fixed.
    if let Err(e) = client
        .batch_execute(&format!("SET lock_timeout = {MIGRATION_LOCK_TIMEOUT_MS}"))
        .await
    {
        return Attempt::Failed(e);
    }

    let got_lock = match client
        .query_one("SELECT pg_try_advisory_lock($1)", &[&MIGRATION_LOCK])
        .await
    {
        Ok(row) => row.get::<_, bool>(0),
        Err(e) => {
            reset_lock_timeout(client).await;
            return Attempt::Failed(e);
        }
    };

    if !got_lock {
        reset_lock_timeout(client).await;
        return Attempt::Contended {
            by: "another migration holds the advisory lock".to_owned(),
        };
    }

    let applied = async {
        client
            .batch_execute(include_str!("../migrations/0001_sync_tables.sql"))
            .await?;
        client
            .batch_execute(include_str!("../migrations/0002_scope_blocklist.sql"))
            .await?;
        client
            .batch_execute(include_str!("../migrations/0003_watermark.sql"))
            .await
    }
    .await;

    // Released whatever happened, so a failed migration does not hold the lock until the session
    // closes and leave every other instance waiting on it.
    let unlocked = client
        .execute("SELECT pg_advisory_unlock($1)", &[&MIGRATION_LOCK])
        .await;

    reset_lock_timeout(client).await;

    match applied {
        Ok(()) => match unlocked {
            Ok(_) => Attempt::Done,
            Err(e) => Attempt::Failed(e),
        },
        Err(e) if is_lock_unavailable(&e) => Attempt::Contended {
            by: "a transaction is holding a lock on sync_changes".to_owned(),
        },
        Err(e) => Attempt::Failed(e),
    }
}

/// Whether an error means "somebody else holds the lock", as opposed to a real failure.
///
/// `55P03 lock_not_available` is what `lock_timeout` raises. Retrying anything else would be
/// retrying a genuine problem, which turns a clear error into a slow one.
fn is_lock_unavailable(e: &tokio_postgres::Error) -> bool {
    e.code() == Some(&tokio_postgres::error::SqlState::LOCK_NOT_AVAILABLE)
}

/// Puts `lock_timeout` back to the session default.
///
/// Best-effort: this runs on the failure path too, and a connection too broken to reset a GUC has
/// larger problems than the GUC. Failing here would mask the error that actually mattered.
async fn reset_lock_timeout(client: &Client) {
    let _ = client.batch_execute("SET lock_timeout = DEFAULT").await;
}

/// The advisory-lock key that separates change-log writers from a reader establishing a watermark.
///
/// Arbitrary but stable, and distinct from [`MIGRATION_LOCK`]: two locks that shared a key would
/// serialise against each other for no reason, and the bug would look like a mysterious stall.
const WRITER_LOCK: i64 = 0x0000_c2ed_5900_0002;

/// Takes the writer side of the change-log lock, for the duration of the calling transaction.
///
/// **A host must call this in the transaction that writes its change-log rows**, before the first
/// `INSERT`. It is a *shared* lock, so writers never block each other; the only thing it blocks is
/// a reader trying to establish a watermark, and only for as long as the write takes.
///
/// # Why a host has to do anything at all
///
/// Because the ordering problem cannot be solved from the read side. See [`safe_watermark`]: `seq`
/// is allocated at `INSERT` and a row becomes visible at `COMMIT`, and nothing visible to a reader
/// reveals which `seq` values are currently allocated-but-uncommitted. The writer is the only party
/// that knows, and this is the cheapest way for it to say so.
///
/// Released automatically at commit or rollback, because it is transaction-scoped. There is no way
/// for a host to leak it.
///
/// # Errors
/// Returns [`ServerError::Database`] if the lock cannot be taken.
#[cfg(feature = "postgres")]
pub async fn lock_for_write(client: &Client) -> Result<(), ServerError> {
    client
        .execute("SELECT pg_advisory_xact_lock_shared($1)", &[&WRITER_LOCK])
        .await?;
    Ok(())
}

/// How long a reader will wait for in-flight change-log writes before serving slightly stale data.
///
/// Short on purpose. Waiting longer buys fresher data and costs latency on every pull; falling back
/// costs one cycle of staleness and nothing else, because the fallback watermark is still safe.
const WATERMARK_LOCK_TIMEOUT_MS: u32 = 250;

/// The highest `seq` that is certainly committed, and therefore safe to deliver up to.
///
/// # Why a watermark, and why the previous guard was wrong
///
/// `seq` is a `bigserial`, allocated when the `INSERT` runs; a row becomes visible when its
/// transaction commits. Those two orders differ, so a client handed a later `seq` while an earlier
/// one is still in flight would advance its cursor past a change it will never be given.
///
/// D-063 guarded that with `xmin < pg_snapshot_xmin(...)` — deliver only rows whose transaction
/// finished before the oldest running one began. **That does not work**, because it assumes xid
/// order bounds `seq` order and it does not: an xid is assigned at a transaction's *first write*,
/// a `seq` at its `INSERT` into `sync_changes`, and those are independent.
///
/// ```text
///   T0: BEGIN; INSERT elsewhere        -> takes xid 837
///   T1: BEGIN; INSERT sync_changes     -> takes xid 838, seq 102   (stays open)
///   T0:        INSERT sync_changes     -> seq 103; COMMIT
///
///   pg_snapshot_xmin is now 838. Row 103 has xmin 837 < 838, so it was admitted —
///   while 102 sat uncommitted underneath it.
/// ```
///
/// That shape is not exotic. A host writing domain state before its change-log row takes its xid
/// early and its `seq` late, which is exactly what `docs/spec.md` §1 prescribes: *"written by
/// domain handlers via the outbox in the same transaction as the state change"*. Measured and filed
/// as #74.
///
/// # How this one works
///
/// Writers hold [`lock_for_write`] — a *shared* advisory lock — for the length of their
/// transaction. Taking it **exclusively** therefore waits until no change-log write is in flight,
/// and at that instant every `seq` the sequence has handed out has committed. The sequence's
/// `last_value` read at that moment is a watermark with no hole beneath it, and it is recorded.
///
/// # A reader is never blocked
///
/// The exclusive attempt is bounded by a short lock timeout. If a writer is holding a long
/// transaction, the reader gives up and serves the **last recorded watermark** instead.
///
/// That is always safe: a watermark established at a quiescent instant means every `seq` at or
/// below it had committed, and committed rows stay committed. An older watermark is therefore
/// never *wrong*, only staler — which is the right way round. The first version of this waited
/// unconditionally and deadlocked the test suite against a writer holding a transaction open, which
/// would have traded a staleness bug for a hard stall.
///
/// # What it costs
///
/// Staleness bounded by how long writers keep the lock continuously held, and nothing else. A
/// `pg_dump`, a long analytics query, or a leaked idle-in-transaction connection that never touches
/// `sync_changes` now has no effect at all — which was #62.
///
/// A host that forgets to call [`lock_for_write`] does not corrupt anything; it reintroduces the
/// race for its own writes, which is why [`append_change`] calls it for the writes it performs.
///
/// # Errors
/// Returns [`ServerError::Database`] if the watermark can neither be established nor read back.
#[cfg(feature = "postgres")]
pub async fn safe_watermark(client: &Client) -> Result<i64, ServerError> {
    // An explicit transaction, because `pg_advisory_xact_lock` is released when it ends and the
    // read of `last_value` must happen while the lock is still held. In one implicit statement the
    // lock would already be gone.
    client.batch_execute("BEGIN").await?;

    let established = establish_watermark(client).await;

    // Committed either way: on the happy path it persists the new watermark, and on the timeout
    // path there is nothing to persist but the transaction still has to end.
    let closed = client.batch_execute("COMMIT").await;

    match established {
        Ok(Some(watermark)) => {
            closed?;
            Ok(watermark)
        }
        Ok(None) | Err(_) => {
            // Either a writer held the lock past the timeout, or the attempt failed outright.
            // Both fall back to the last watermark somebody did establish, which is stale rather
            // than wrong. A failure to read *that* is a real error.
            let _ = closed;
            let _ = client.batch_execute("ROLLBACK").await;
            recorded_watermark(client).await
        }
    }
}

/// Takes the lock, reads the sequence, and records the result. `Ok(None)` means a writer held it.
#[cfg(feature = "postgres")]
async fn establish_watermark(client: &Client) -> Result<Option<i64>, tokio_postgres::Error> {
    client
        .batch_execute(&format!(
            "SET LOCAL lock_timeout = {WATERMARK_LOCK_TIMEOUT_MS}"
        ))
        .await?;

    if let Err(e) = client
        .execute("SELECT pg_advisory_xact_lock($1)", &[&WRITER_LOCK])
        .await
    {
        if e.code() == Some(&tokio_postgres::error::SqlState::LOCK_NOT_AVAILABLE) {
            return Ok(None);
        }
        return Err(e);
    }

    // Read *after* the lock is granted. Reading it first would give a number a writer could still
    // be below, which is the whole bug this function exists to avoid.
    let row = client
        .query_one(
            "INSERT INTO sync_watermark (only_row, value)
             VALUES (true, COALESCE((SELECT last_value FROM sync_changes_seq_seq WHERE is_called), 0))
             ON CONFLICT (only_row)
             DO UPDATE SET value = GREATEST(sync_watermark.value, EXCLUDED.value)
             RETURNING value",
            &[],
        )
        .await?;
    Ok(Some(row.get::<_, i64>("value")))
}

/// The last watermark anybody managed to establish.
#[cfg(feature = "postgres")]
async fn recorded_watermark(client: &Client) -> Result<i64, ServerError> {
    let rows = client
        .query("SELECT value FROM sync_watermark WHERE only_row", &[])
        .await?;
    Ok(rows.first().map_or(0, |r| r.get::<_, i64>("value")))
}

/// One page of changes, and whether the row ceiling cut it short.
#[derive(Debug, Clone)]
pub struct Page {
    /// The changes read, in `seq` order.
    pub changes: Vec<Change>,
    /// Whether more rows exist beyond this page.
    ///
    /// Needed because the byte budget is not the only thing that can truncate a read. The row
    /// ceiling can too, and `fill_batch` cannot tell the difference from its side: a hundred small
    /// tombstones fit a 100 KB budget easily, so every candidate is chosen and `has_more` would
    /// read `false` while rows remain. The scope would then sit still until something else
    /// happened to it.
    pub more_beyond: bool,
}

/// Reads up to `limit` changes for one scope, strictly after `cursor`, in `seq` order.
///
/// `limit` is a **safety ceiling on rows read**, not the batch size — the batch is sized by
/// compressed bytes (`docs/spec.md` §2). Its job is to stop one request pulling a million rows
/// into memory before the budget is even measured. One extra row is read beyond the ceiling so
/// [`Page::more_beyond`] can be answered without a second query.
///
/// # Why the watermark, and not just `seq > cursor`
///
/// `seq` is a `bigserial`, allocated when the `INSERT` runs — but a row becomes **visible** when its
/// transaction commits, and those two orders are not the same:
///
/// ```text
///   A: BEGIN; INSERT -> seq 1; (still open)
///   B:        INSERT -> seq 2; COMMIT
///   reader sees: seq 2 only
/// ```
///
/// A client would take `seq 2`, advance its cursor past `1`, and **never be given change 1** —
/// silent loss of exactly the kind this project exists to prevent, invisible to every ordering
/// check because the batch it received was perfectly ordered.
///
/// [`safe_watermark`] is where that is prevented, and its docs explain why the previous guard
/// (`xmin < pg_snapshot_xmin`) did not: xid order does not bound `seq` order, so the old guard
/// admitted exactly the row it existed to withhold (#74). Rows above the watermark are withheld,
/// not reordered — the client gets them on the next pull, a moment later.
///
/// # The scope filter is not optional and not a convenience
///
/// `docs/spec.md` §8: pull can never cross scopes regardless of client input. The `WHERE scope =`
/// clause is that guarantee. A client controls `cursor` completely and can send any value it
/// likes; what it cannot do is reach another scope's rows, because the query never looks at them.
///
/// # Errors
/// Returns [`ServerError::Database`] on a query failure, or [`ServerError::Corrupt`] if a stored
/// row fails the protocol's validation on the way out.
pub async fn changes_after(
    client: &Client,
    scope: &ScopeId,
    cursor: u64,
    limit: i64,
) -> Result<Page, ServerError> {
    let watermark = safe_watermark(client).await?;
    changes_after_watermark(client, scope, cursor, limit, watermark).await
}

/// [`changes_after`] against a watermark the caller already established.
///
/// Serving several scopes in one pull should establish the watermark once, not once per scope: the
/// numbers would differ, so the batches would describe different instants, and a client applying
/// them would hold a mixture no single server state ever matched.
///
/// # Errors
/// As [`changes_after`].
#[cfg(feature = "postgres")]
pub async fn changes_after_watermark(
    client: &Client,
    scope: &ScopeId,
    cursor: u64,
    limit: i64,
    watermark: i64,
) -> Result<Page, ServerError> {
    let cursor = i64::try_from(cursor).map_err(|_| ServerError::Corrupt {
        detail: format!("cursor {cursor} does not fit a bigint"),
    })?;

    let rows = client
        .query(
            "SELECT seq, entity, entity_id, op, snapshot, row_version, schema_version
               FROM sync_changes
              WHERE scope = $1
                AND seq > $2
                AND seq <= $3
              ORDER BY seq ASC
              LIMIT $4",
            &[&scope.as_str(), &cursor, &watermark, &(limit + 1)],
        )
        .await?;

    let more_beyond = i64::try_from(rows.len()).unwrap_or(i64::MAX) > limit;
    let changes = rows
        .into_iter()
        .take(usize::try_from(limit).unwrap_or(usize::MAX))
        .map(|r| {
            Ok(ChangeRow {
                seq: r.try_get(0)?,
                entity: r.try_get(1)?,
                entity_id: r.try_get(2)?,
                op: r.try_get(3)?,
                snapshot: r.try_get(4)?,
                row_version: r.try_get(5)?,
                schema_version: r.try_get(6)?,
            })
        })
        .collect::<Result<Vec<ChangeRow>, ServerError>>()?
        .into_iter()
        .map(ChangeRow::into_change)
        .collect::<Result<Vec<Change>, ServerError>>()?;

    Ok(Page {
        changes,
        more_beyond,
    })
}

/// A change about to be written to the log.
///
/// A struct rather than eight positional arguments, because `(scope, entity, entity_id)` are three
/// adjacent strings and `(row_version, schema_version)` are two adjacent integers — a call site
/// that transposed either pair would compile and be wrong.
#[derive(Debug, Clone)]
pub struct NewChange<'a> {
    /// Which scope the row belongs to.
    pub scope: &'a str,
    /// Which entity.
    pub entity: &'a str,
    /// The row's identifier.
    pub entity_id: &'a str,
    /// `upsert` or `delete`.
    pub op: &'a str,
    /// The full row. Must be `None` exactly when `op` is `delete`; the schema enforces it.
    pub snapshot: Option<&'a serde_json::Value>,
    /// Server-assigned row version.
    pub row_version: i64,
    /// The schema this snapshot was written under.
    pub schema_version: i32,
}

/// Appends one change to the log, returning its assigned `seq`.
///
/// In production a host writes this through its outbox, in the same transaction as the state
/// change (`docs/spec.md` §1). This exists for tests and for the reference implementation.
///
/// # Errors
/// Returns [`ServerError::Database`] if the insert fails, including when a check constraint
/// refuses the row — an upsert with no snapshot, or a delete carrying one.
pub async fn append_change(client: &Client, change: &NewChange<'_>) -> Result<i64, ServerError> {
    // The writer side of the ordering contract. See `safe_watermark` for what it buys and #74 for
    // what happened without it.
    //
    // A host writing its own change-log rows in its own transaction must call `lock_for_write`
    // itself, before its first INSERT -- this function can only speak for the writes it performs.
    lock_for_write(client).await?;

    let row = client
        .query_one(
            "INSERT INTO sync_changes
                    (scope, entity, entity_id, op, snapshot, row_version, schema_version)
             VALUES ($1, $2, $3, $4, $5, $6, $7)
             RETURNING seq",
            &[
                &change.scope,
                &change.entity,
                &change.entity_id,
                &change.op,
                &change.snapshot,
                &change.row_version,
                &change.schema_version,
            ],
        )
        .await?;
    Ok(row.get(0))
}

/// Reads one page of a scope's **compacted** log, for bootstrap. `docs/spec.md` §3.1.
///
/// # Compaction is the whole point
///
/// Ordinary pull replays every change. A device joining a year-old scope does not need a year of
/// edits — it needs the current value of each row, once. So this returns, per `(entity, entity_id)`,
/// that row's single latest change, ordered by that change's `seq`.
///
/// Because the ordering key *is* a log `seq`, `after` and the resulting `next_cursor` are the same
/// currency as pull's. One `u64` resumes a partial bootstrap, and the device joins the log exactly
/// where the bootstrap left off.
///
/// # Tombstones, and the bug that made them necessary
///
/// A row whose latest change is a delete is included when `after > 0` and skipped when `after = 0`.
///
/// The asymmetry is not an optimisation. Consider a row delivered on page one and deleted while
/// page two is being computed. If bootstrap returned only *live* rows it would never resend that
/// row — it is not live any more — and its tombstone sits below the `next_cursor` the device joins
/// at, so the log would never deliver it either. The device would hold a deleted row permanently.
///
/// When `after = 0` the device holds nothing, so there is no row to correct and a historical
/// tombstone would be pure noise.
///
/// # The same watermark as pull
///
/// [`safe_watermark`] applies here for the same reason: a row whose inserting transaction may still
/// be in flight must not set `next_cursor` past a change the device will never be given.
///
/// # Errors
/// Returns [`ServerError::Database`] on a query failure, or [`ServerError::Corrupt`] if a stored
/// row fails the protocol's validation on the way out.
pub async fn bootstrap_after(
    client: &Client,
    scope: &ScopeId,
    after: u64,
    limit: i64,
) -> Result<Page, ServerError> {
    // The same watermark pull uses, for the same reason: a bootstrap that compacted over a row
    // whose insert had not committed would hand a device a `next_cursor` past a change it will
    // never receive -- and a bootstrap is the one moment a device trusts the server completely.
    let watermark = safe_watermark(client).await?;
    let after_i = i64::try_from(after).map_err(|_| ServerError::Corrupt {
        detail: format!("bootstrap cursor {after} does not fit a bigint"),
    })?;

    // `DISTINCT ON` takes the highest-seq row per entity pair, then the outer query orders that
    // compacted set by seq and pages it. Written this way rather than with a window function
    // because `DISTINCT ON` is the one Postgres does with a single index scan.
    let rows = client
        .query(
            "SELECT seq, entity, entity_id, op, snapshot, row_version, schema_version
               FROM (
                 SELECT DISTINCT ON (entity, entity_id)
                        seq, entity, entity_id, op, snapshot, row_version, schema_version
                   FROM sync_changes
                  WHERE scope = $1
                    AND seq <= $4
                  ORDER BY entity, entity_id, seq DESC
               ) AS latest
              WHERE seq > $2
                AND (op <> 'delete' OR $2 > 0)
              ORDER BY seq ASC
              LIMIT $3",
            &[&scope.as_str(), &after_i, &(limit + 1), &watermark],
        )
        .await?;

    let more_beyond = i64::try_from(rows.len()).unwrap_or(i64::MAX) > limit;
    let changes = rows
        .into_iter()
        .take(usize::try_from(limit).unwrap_or(usize::MAX))
        .map(|r| {
            Ok(ChangeRow {
                seq: r.try_get(0)?,
                entity: r.try_get(1)?,
                entity_id: r.try_get(2)?,
                op: r.try_get(3)?,
                snapshot: r.try_get(4)?,
                row_version: r.try_get(5)?,
                schema_version: r.try_get(6)?,
            })
        })
        .collect::<Result<Vec<ChangeRow>, ServerError>>()?
        .into_iter()
        .map(ChangeRow::into_change)
        .collect::<Result<Vec<Change>, ServerError>>()?;

    Ok(Page {
        changes,
        more_beyond,
    })
}
