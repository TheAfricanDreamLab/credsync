//! The four queries the server actually needs.
//!
//! Written as SQL rather than through a query builder. The server owns two tables, and the two
//! statements below are the ones the byte budget and the cursor walk depend on — hiding them
//! behind a builder would hide the only part worth reading closely.

use crate::error::ServerError;
use crate::pull::ChangeRow;
use credsync_protocol::{Change, ScopeId};
use tokio_postgres::Client;

impl From<tokio_postgres::Error> for ServerError {
    fn from(e: tokio_postgres::Error) -> Self {
        // `tokio_postgres::Error` renders as the useless string "db error"; everything worth
        // knowing -- the SQLSTATE, the constraint name, the message -- lives in its `source`.
        //
        // Not a cosmetic complaint. At CS-17 a foreign-key violation surfaced as
        // `Database { detail: "db error" }` and the cause had to be recovered from the server's
        // own log. An operator reading a production log does not have that option.
        use core::error::Error as _;
        let mut detail = e.to_string();
        if let Some(cause) = e.source() {
            detail = format!("{detail}: {cause}");
        }
        Self::Database { detail }
    }
}

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
/// # Why the snapshot guard, and not just `seq > cursor`
///
/// `seq` is a `bigserial`, which allocates when the `INSERT` runs — but a row becomes **visible**
/// when its transaction commits, and those two orders are not the same. Demonstrated against
/// Postgres 14:
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
/// So the query returns only rows whose inserting transaction finished before the oldest
/// currently-running one began. A row from a transaction that might still be in flight is
/// withheld, not reordered: the client simply gets it on the next pull, a moment later.
///
/// The cost is latency, bounded by how long the slowest concurrent writer holds its transaction
/// open. The alternative is losing writes, so it is not much of a trade.
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
    let cursor = i64::try_from(cursor).map_err(|_| ServerError::Corrupt {
        detail: format!("cursor {cursor} does not fit a bigint"),
    })?;

    let rows = client
        .query(
            "SELECT seq, entity, entity_id, op, snapshot, row_version, schema_version
               FROM sync_changes
              WHERE scope = $1
                AND seq > $2
                AND xmin::text::bigint < pg_snapshot_xmin(pg_current_snapshot())::text::bigint
              ORDER BY seq ASC
              LIMIT $3",
            &[&scope.as_str(), &cursor, &(limit + 1)],
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
