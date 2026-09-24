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
        Self::Database {
            detail: e.to_string(),
        }
    }
}

/// A fixed key for the advisory lock that serialises migration.
///
/// Arbitrary but stable: every process that migrates this schema must pick the same number, or the
/// lock protects nothing. Derived from "credsync" so a collision with another application's
/// advisory lock on the same database is unlikely rather than merely hoped for.
const MIGRATION_LOCK: i64 = 0x0000_c2ed_5900_0001;

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
/// # Errors
/// Returns [`ServerError::Database`] if the lock cannot be taken or the statements cannot be
/// applied.
pub async fn migrate(client: &Client) -> Result<(), ServerError> {
    client
        .execute("SELECT pg_advisory_lock($1)", &[&MIGRATION_LOCK])
        .await?;

    let applied = client
        .batch_execute(include_str!("../migrations/0001_sync_tables.sql"))
        .await;

    // Released whatever happened, so a failed migration does not hold the lock until the session
    // closes and leave every other instance waiting on it.
    let unlocked = client
        .execute("SELECT pg_advisory_unlock($1)", &[&MIGRATION_LOCK])
        .await;

    applied?;
    unlocked?;
    Ok(())
}

/// Reads up to `limit` changes for one scope, strictly after `cursor`, in `seq` order.
///
/// `limit` is a **safety ceiling on rows read**, not the batch size — the batch is sized by
/// compressed bytes (`docs/spec.md` §2). Its job is to stop one request pulling a million rows
/// into memory before the budget is even measured. Read one more than could possibly fit so the
/// caller can set `has_more` without a second query.
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
) -> Result<Vec<Change>, ServerError> {
    let cursor = i64::try_from(cursor).map_err(|_| ServerError::Corrupt {
        detail: format!("cursor {cursor} does not fit a bigint"),
    })?;

    let rows = client
        .query(
            "SELECT seq, entity, entity_id, op, snapshot, row_version, schema_version
               FROM sync_changes
              WHERE scope = $1 AND seq > $2
              ORDER BY seq ASC
              LIMIT $3",
            &[&scope.as_str(), &cursor, &limit],
        )
        .await?;

    rows.into_iter()
        .map(|r| {
            ChangeRow {
                seq: r.get(0),
                entity: r.get(1),
                entity_id: r.get(2),
                op: r.get(3),
                snapshot: r.get(4),
                row_version: r.get(5),
                schema_version: r.get(6),
            }
            .into_change()
        })
        .collect()
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
