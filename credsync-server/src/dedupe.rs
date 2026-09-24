//! Command dedupe and the result store. `docs/spec.md` §1 and §5, Design §5.1.
//!
//! # The table is a security boundary, not a cache
//!
//! A dedupe table that returns the recorded success for any replay of a known command id is a way
//! to **launder tampered commands**: send a command, note that it succeeded, then resend the same
//! id with a different body and collect the original's success. The payload would never be looked
//! at.
//!
//! So a replay is only a replay when the body matches. Every record stores a BLAKE3 checksum over
//! the payload's canonical encoding (D-031), and a replay whose checksum differs is refused as a
//! **distinct invalid request** — not deduped, not applied, not silently ignored. `docs/spec.md`
//! §5 states this directly, and it is the reason the checksum is BLAKE3 rather than xxh3: xxh3
//! collisions are findable by construction, and a found collision here would be a forged replay.
//!
//! # Exactly once, under concurrency
//!
//! Two devices — or one device retrying over a flaky link — can submit the same command id
//! simultaneously. `INSERT … ON CONFLICT DO NOTHING` makes the database the arbiter: exactly one
//! insert wins, and the loser reads the winner's outcome instead of applying anything. Deciding
//! this in application code would mean a check-then-act race, which is the same shape as the
//! `CREATE TABLE IF NOT EXISTS` race that CI found in the migration.

use crate::error::ServerError;
use credsync_protocol::{Command, CommandResult, Reason, payload_checksum};
#[cfg(feature = "postgres")]
use credsync_protocol::{CommandId, Seq, Status};
#[cfg(feature = "postgres")]
use tokio_postgres::Client;

/// What the server decided about a submitted command.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Decision {
    /// No record existed; this submission is the first and should be applied.
    Fresh,

    /// A record exists and the payload matches. The recorded outcome is returned **without
    /// re-applying anything**.
    Replay(CommandResult),

    /// A record exists and the payload does **not** match.
    ///
    /// Refused as a distinct invalid request. Not deduped as a success, which would launder a
    /// tampered body; not applied, which would let one command id mean two different things.
    Mutated {
        /// The checksum recorded when the command was first accepted.
        recorded: String,
        /// The checksum of the body just submitted.
        submitted: String,
    },
}

/// Counts of what the dedupe table has been asked, for the hit-rate metric.
///
/// A rate rather than a raw count is what an operator can act on: a hit rate that climbs means
/// clients are retrying more, which usually means the network got worse or a push is failing
/// somewhere between the server's answer and the client recording it.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct DedupeStats {
    /// Submissions with no prior record.
    pub fresh: u64,
    /// Submissions that matched a record exactly and returned it.
    pub replays: u64,
    /// Submissions that reused an id with a different body.
    pub mutated: u64,
}

impl DedupeStats {
    /// Every submission seen.
    #[must_use]
    pub const fn total(&self) -> u64 {
        self.fresh + self.replays + self.mutated
    }

    /// The share of submissions that were replays, in the range `0.0..=1.0`.
    ///
    /// Zero when nothing has been submitted — a rate over no samples is not zero so much as
    /// meaningless, but reporting `NaN` to a metrics pipeline is worse than reporting nothing.
    #[must_use]
    pub fn hit_rate(&self) -> f64 {
        let total = self.total();
        if total == 0 {
            return 0.0;
        }
        #[expect(
            clippy::cast_precision_loss,
            reason = "a counter large enough to lose precision here has long since overflowed the \
                      operator's attention; the rate is for a dashboard, not for arithmetic"
        )]
        {
            self.replays as f64 / total as f64
        }
    }
}

/// What the store already holds for a command id.
///
/// The two fields a decision actually turns on. Deliberately not a database row: the same
/// judgement has to be made by the Postgres server and by the simulator, and a type that named
/// columns would force one of them to pretend.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Recorded {
    /// The checksum of the body that produced the recorded outcome.
    pub checksum: String,
    /// What was decided.
    pub result: CommandResult,
}

/// Decides what a submission is, given whatever the store holds for its id.
///
/// **The whole dedupe rule, as a pure function.** No database, no clock, no I/O — so the Postgres
/// server and the simulator run this exact code rather than two implementations that agree until
/// the day they do not.
///
/// The security property lives here (`docs/spec.md` §5): a replay is only a replay when the body
/// matches. Returning the recorded success for a mismatched body would make the dedupe table a way
/// to launder tampered commands — send a command, note that it succeeded, resend the id with a
/// different body, collect the original's success, payload never looked at.
///
/// # Errors
/// Returns [`ServerError::Corrupt`] if the submitted payload cannot be encoded for checksumming.
pub fn decide(command: &Command, recorded: Option<&Recorded>) -> Result<Decision, ServerError> {
    let submitted = payload_checksum(&command.payload).map_err(|_| ServerError::Corrupt {
        detail: "a submitted payload could not be encoded for checksumming".to_owned(),
    })?;

    let Some(recorded) = recorded else {
        return Ok(Decision::Fresh);
    };

    if recorded.checksum != submitted.as_str() {
        return Ok(Decision::Mutated {
            recorded: recorded.checksum.clone(),
            submitted: submitted.as_str().to_owned(),
        });
    }

    Ok(Decision::Replay(recorded.result.clone()))
}

/// Looks up a command, deciding whether it is fresh, a replay, or a mutated reuse.
///
/// Does not write. Recording the outcome is [`record`], which happens after the host has decided
/// what the outcome *is*.
///
/// # Errors
/// Returns [`ServerError::Database`] if the lookup fails, or [`ServerError::Corrupt`] if a stored
/// record cannot be read back as a valid result.
#[cfg(feature = "postgres")]
pub async fn classify(client: &Client, command: &Command) -> Result<Decision, ServerError> {
    let rows = client
        .query(
            "SELECT payload_checksum, status, reason, server_seq
               FROM sync_command_results
              WHERE command_id = $1",
            &[&command.id.to_string()],
        )
        .await?;

    // Read the row, then let `decide` rule on it. The database's job is to remember; the rule
    // about what a memory *means* is the same rule the simulator runs.
    let recorded = match rows.first() {
        None => None,
        Some(row) => Some(Recorded {
            checksum: row.get(0),
            result: read_result(command.id, row)?,
        }),
    };

    decide(command, recorded.as_ref())
}

/// Rebuilds a [`CommandResult`] from a stored row.
#[cfg(feature = "postgres")]
fn read_result(id: CommandId, row: &tokio_postgres::Row) -> Result<CommandResult, ServerError> {
    let status: String = row.get(1);
    let reason: Option<String> = row.get(2);
    let server_seq: Option<i64> = row.get(3);

    let status = match status.as_str() {
        "applied" => Status::Applied,
        "rejected" => Status::Rejected,
        "superseded" => Status::Superseded,
        other => {
            return Err(ServerError::Corrupt {
                detail: format!("sync_command_results.status is '{other}'"),
            });
        }
    };

    let reason = reason.map(Reason::new).transpose()?;
    let server_seq = server_seq
        .map(|s| {
            u64::try_from(s)
                .map_err(|_| ServerError::Corrupt {
                    detail: format!("sync_command_results.server_seq {s} is negative"),
                })
                .and_then(|s| Seq::new(s).map_err(ServerError::from))
        })
        .transpose()?;

    let result = CommandResult {
        id,
        status,
        reason,
        server_seq,
    };

    // The same rule the wire enforces: a rejection carries a reason. The table has a check
    // constraint for it, so this can only fire if the constraint was dropped — which is worth
    // saying out loud rather than passing a malformed result to a client.
    result.validate().map_err(ServerError::from)?;
    Ok(result)
}

/// Records an outcome, or returns the one already recorded.
///
/// `INSERT … ON CONFLICT DO NOTHING` makes the database the arbiter of who wins a concurrent
/// submission of the same id. Exactly one insert lands; anyone who loses reads the winner's row
/// instead of applying anything. Deciding that in application code would be a check-then-act race.
///
/// # Errors
/// Returns [`ServerError::Database`] if the write fails, or [`ServerError::Corrupt`] if the row
/// that won cannot be read back.
#[cfg(feature = "postgres")]
pub async fn record(
    client: &Client,
    command: &Command,
    result: &CommandResult,
) -> Result<CommandResult, ServerError> {
    let checksum = payload_checksum(&command.payload).map_err(|_| ServerError::Corrupt {
        detail: "a payload could not be encoded for checksumming".to_owned(),
    })?;

    let status = match result.status {
        Status::Applied => "applied",
        Status::Rejected => "rejected",
        Status::Superseded => "superseded",
    };
    let reason = result.reason.as_ref().map(|r| r.as_str().to_owned());
    let server_seq = result
        .server_seq
        .map(|s| i64::try_from(s.get()))
        .transpose()
        .map_err(|_| ServerError::Corrupt {
            detail: "server_seq does not fit a bigint".to_owned(),
        })?;

    let inserted = client
        .execute(
            "INSERT INTO sync_command_results
                    (command_id, payload_checksum, status, reason, server_seq, scope)
             VALUES ($1, $2, $3, $4, $5, $6)
             ON CONFLICT (command_id) DO NOTHING",
            &[
                &command.id.to_string(),
                &checksum.as_str(),
                &status,
                &reason,
                &server_seq,
                &command.scope.as_str(),
            ],
        )
        .await?;

    if inserted == 1 {
        return Ok(result.clone());
    }

    // Somebody else won the race. Their outcome is the one that counts — returning ours would
    // mean two clients being told different things about one command.
    match classify(client, command).await? {
        Decision::Replay(existing) => Ok(existing),
        Decision::Mutated {
            recorded,
            submitted,
        } => Err(ServerError::Corrupt {
            detail: format!(
                "command {} lost an insert race to a record with checksum {recorded}, but this \
                 submission's payload hashes to {submitted}",
                command.id
            ),
        }),
        Decision::Fresh => Err(ServerError::Corrupt {
            detail: format!(
                "command {} lost an insert race and then found no record at all",
                command.id
            ),
        }),
    }
}

/// The reason returned for a command whose id was reused with a different body.
///
/// A fixed sentence rather than the checksums: the two hashes mean nothing to the person reading
/// the message, and putting them in front of a student would be noise. The checksums go to the
/// operator through [`Decision::Mutated`].
#[must_use]
pub fn mutated_reason() -> Reason {
    Reason::new("This request could not be verified. Please try again.")
        .unwrap_or_else(|_| unreachable!("literal is a valid reason"))
}
