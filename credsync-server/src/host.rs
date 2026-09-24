//! Forwarding commands to the host, and recording what it decided. Design v2.1 §4.2.
//!
//! # Domain logic never lives here
//!
//! credSync validates the wire, dedupes by command id, and records outcomes. It does not know what
//! a reflection is, when a deadline passes, or whether an enrolment is active. Every one of those
//! judgements belongs to the host, which applies its own rules and writes state plus change-log
//! rows in **one transaction**.
//!
//! That boundary is the whole reason any stack that can expose one HTTP endpoint and write two
//! tables can adopt credSync without surrendering its domain model. It is also load-bearing for
//! Dream Lab specifically: the platform plan puts domain code in NestJS, and this is what lets it
//! stay there.
//!
//! # A failure to reach the host is not a verdict
//!
//! The most important rule in this module, and the easiest to get wrong. A timeout, a 5xx, or a
//! refused connection tells you **nothing about whether the command was applied**. The host may
//! have committed the write and died before answering.
//!
//! So a transport failure is never recorded as an outcome. Recording `rejected` on a timeout would
//! tell a student their work was refused when it may well have saved; recording `applied` would
//! claim a write that may never have happened. The command stays unresolved, the client retries,
//! and the dedupe table answers correctly the moment the host is reachable again — because if the
//! host *did* apply it, the retry finds the recorded outcome rather than applying it twice.
//!
//! This is why [`Forwarded::Unresolved`] exists rather than a convenient `Rejected`.

#[cfg(feature = "postgres")]
use crate::dedupe::{self, Decision};
#[cfg(feature = "postgres")]
use crate::error::ServerError;
use core::future::Future;
#[cfg(feature = "postgres")]
use credsync_protocol::Status;
use credsync_protocol::{Command, CommandResult, Reason, Seq};
#[cfg(feature = "postgres")]
use tokio_postgres::Client;

/// What the host decided about a command.
///
/// The three outcomes `docs/spec.md` §3.3 defines, and nothing else. Note there is no variant for
/// "something went wrong talking to the host" — that is [`HostError`], and it is deliberately not
/// an outcome.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HostOutcome {
    /// The host applied it, writing state and the change-log row in one transaction.
    Applied {
        /// Where the resulting change landed in the log.
        server_seq: Seq,
    },
    /// The host refused it, and here is what to tell the user.
    Rejected {
        /// The host's explanation. Carried through to the client unchanged.
        reason: Reason,
    },
    /// A later command overtook it.
    Superseded,
}

/// Why the host could not be asked, or could not answer.
///
/// **None of these are verdicts.** See the module docs: a host that timed out may have applied the
/// command and died before saying so.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum HostError {
    /// The host did not answer within its deadline.
    Timeout,
    /// The host answered with a status that is not a verdict — 5xx, or anything unexpected.
    Status {
        /// The status code returned.
        code: u16,
    },
    /// The host could not be reached at all.
    Unreachable {
        /// The transport's description, for the operator.
        detail: String,
    },
    /// The host answered, but not with something this server can read.
    Malformed {
        /// What was wrong.
        detail: String,
    },
}

impl core::fmt::Display for HostError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::Timeout => write!(f, "the host did not answer in time"),
            Self::Status { code } => write!(f, "the host answered with status {code}"),
            Self::Unreachable { detail } => write!(f, "the host was unreachable: {detail}"),
            Self::Malformed { detail } => write!(f, "the host's answer was unreadable: {detail}"),
        }
    }
}

impl core::error::Error for HostError {}

/// The host application, as this server needs to see it.
///
/// One method. Everything credSync knows about a host is that it can be handed a validated command
/// and will eventually say what it did — which is exactly as much as it should know.
pub trait Host {
    /// Applies a command and reports the outcome.
    ///
    /// # Errors
    /// Returns [`HostError`] when the host could not be asked or could not answer. Implementations
    /// must **not** turn a transport failure into a [`HostOutcome`]: that decision belongs to the
    /// caller, which has the dedupe table and knows the difference between "refused" and "never
    /// heard back".
    fn apply(
        &self,
        command: &Command,
    ) -> impl Future<Output = Result<HostOutcome, HostError>> + Send;
}

/// What forwarding one command produced.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Forwarded {
    /// The host decided, and the outcome is recorded.
    Decided(CommandResult),

    /// A replay of a command already decided. The recorded outcome is returned and the host was
    /// never asked.
    Replayed(CommandResult),

    /// The command id was reused with a different body. Refused without asking the host
    /// (`docs/spec.md` §5).
    Mutated(CommandResult),

    /// The host could not be asked or could not answer, so **nothing was recorded**.
    ///
    /// The client should retry. If the host did apply the command before failing to answer, the
    /// retry finds the recorded outcome rather than applying it twice — which is the dedupe
    /// table's whole purpose.
    Unresolved {
        /// Why the host could not be reached, for the operator.
        error: HostError,
    },
}

impl Forwarded {
    /// The result to send back, when there is one.
    ///
    /// `None` for [`Unresolved`](Self::Unresolved): the protocol has no way to say "I do not know
    /// yet", and inventing one would mean a client treating a maybe as a verdict.
    #[must_use]
    pub const fn result(&self) -> Option<&CommandResult> {
        match self {
            Self::Decided(r) | Self::Replayed(r) | Self::Mutated(r) => Some(r),
            Self::Unresolved { .. } => None,
        }
    }
}

/// Forwards one command to the host, deduping first and recording the outcome after.
///
/// # The order is the correctness argument
///
/// 1. **Classify.** A replay returns its recorded outcome and the host is never asked — which is
///    what stops a retry over a flaky link from applying a command twice.
/// 2. **Refuse a mutated body** before the host sees it. The host would have no way to know the id
///    was reused, and applying it would let one command id mean two different things.
/// 3. **Ask the host**, which applies its own rules and writes state plus change-log rows in one
///    transaction.
/// 4. **Record**, atomically. One `INSERT` carries the status, the reason and the `server_seq`
///    together, so there is no window in which a command is half-resolved.
///
/// A failure at step 3 records nothing and returns [`Forwarded::Unresolved`]. See the module docs
/// for why that is not a rejection.
///
/// # Errors
/// Returns [`ServerError`] if the dedupe table cannot be read or written. A *host* failure is not
/// an error here — it is an outcome of forwarding, reported as [`Forwarded::Unresolved`].
#[cfg(feature = "postgres")]
pub async fn forward<H: Host>(
    client: &Client,
    host: &H,
    command: &Command,
) -> Result<Forwarded, ServerError> {
    match dedupe::classify(client, command).await? {
        Decision::Replay(existing) => return Ok(Forwarded::Replayed(existing)),
        Decision::Mutated { .. } => {
            // Refused without asking the host. It has no way to know the id was reused, and
            // applying it would let one command id mean two different things.
            //
            // Deliberately not recorded: the original record must survive untouched, or a
            // tampered replay could overwrite the outcome of the command it is impersonating.
            return Ok(Forwarded::Mutated(CommandResult {
                id: command.id,
                status: Status::Rejected,
                reason: Some(dedupe::mutated_reason()),
                server_seq: None,
            }));
        }
        Decision::Fresh => {}
    }

    let outcome = match host.apply(command).await {
        Ok(outcome) => outcome,
        Err(error) => return Ok(Forwarded::Unresolved { error }),
    };

    let result = match outcome {
        HostOutcome::Applied { server_seq } => CommandResult {
            id: command.id,
            status: Status::Applied,
            reason: None,
            server_seq: Some(server_seq),
        },
        HostOutcome::Rejected { reason } => CommandResult {
            id: command.id,
            status: Status::Rejected,
            // The host's own words, unchanged. `docs/spec.md` §3.3 surfaces this to the user, and
            // a server that rewrote it would be putting words in the host's mouth about its own
            // domain rules.
            reason: Some(reason),
            server_seq: None,
        },
        HostOutcome::Superseded => CommandResult {
            id: command.id,
            status: Status::Superseded,
            reason: None,
            server_seq: None,
        },
    };

    // Atomic: one INSERT carries status, reason and server_seq together. `record` also settles a
    // concurrent race, so two forwards of one id cannot record two different outcomes.
    let recorded = dedupe::record(client, command, &result).await?;
    Ok(Forwarded::Decided(recorded))
}
