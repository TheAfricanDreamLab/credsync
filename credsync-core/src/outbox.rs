//! The outbox: where a client's writes wait until the server has acknowledged them.
//!
//! Platform Plan v1.1 names losing student work silently as risk R1 and calls it trust-fatal. The
//! outbox is what makes *"never loses an acknowledged write"* a mechanism rather than an
//! aspiration, so its rules are stated here as structure wherever possible, not as discipline.
//!
//! # Entries leave only through a recorded outcome
//!
//! There is deliberately **no storage op that deletes an outbox entry**. The only way out is
//! [`StorageOp::ResolveCommand`](crate::StorageOp::ResolveCommand), which removes the pending
//! entry and writes its [`Resolution`] in the same transaction. "Never silently dropped" is
//! therefore not a rule an adapter has to remember — there is no operation that could do it.
//!
//! That is the difference between a property a test checks and a property the type system makes
//! unavailable. Both are here; only the second survives an adapter written by someone who has not
//! read this file.
//!
//! # In-flight is not persisted, on purpose
//!
//! An entry handed to the transport stays `Pending` in storage. If the process dies mid-push, the
//! command is simply pushed again on restart — and the server's dedupe record (`docs/spec.md` §1)
//! returns the recorded outcome without re-applying it.
//!
//! Persisting an in-flight marker would invert the risk. A crash after marking but before the
//! result arrives would leave an entry that is neither pending nor resolved, and recovering it
//! means guessing whether the server saw it. Idempotent replay against a server that already
//! dedupes is strictly safer than a state machine trying to reason about what the network did.

use credsync_protocol::{CommandId, Reason, SchemaVersion, Seq};

/// How an outbox entry was resolved.
///
/// Every variant is a *recorded* outcome. An entry that leaves the outbox leaves as one of these,
/// which is what `docs/spec.md` §3.3 means by a rejection surfacing rather than being dropped.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum Resolution {
    /// The host applied it.
    Applied {
        /// Where the resulting change landed in the log, when the server said.
        server_seq: Option<Seq>,
    },

    /// A later command overtook it.
    ///
    /// A real resolution rather than a failure: the work was replaced, not lost. Kept distinct
    /// from [`Applied`](Self::Applied) because a UI showing "saved" for a superseded draft is
    /// telling the user something false, and telemetry that cannot separate the two cannot notice
    /// a server superseding far more than it should.
    Superseded,

    /// The host refused it, and here is why.
    ///
    /// **Dead-lettered, not deleted.** `docs/spec.md` §3.3: the entry moves to a dead-letter
    /// state visible to the user rather than being silently dropped. The reason is mandatory
    /// because a rejection reaches a person, and "rejected" with no explanation is a dead end for
    /// them — they wrote something, it did not save, and nothing on the screen says why.
    DeadLettered {
        /// The host's explanation, for the user.
        reason: Reason,
    },
}

impl Resolution {
    /// Whether this outcome needs surfacing to the user.
    #[must_use]
    pub const fn needs_attention(&self) -> bool {
        matches!(self, Self::DeadLettered { .. })
    }

    /// The reason, when there is one.
    #[must_use]
    pub const fn reason(&self) -> Option<&Reason> {
        match self {
            Self::DeadLettered { reason } => Some(reason),
            _ => None,
        }
    }
}

/// One queued command, waiting for the server to say what happened to it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OutboxEntry {
    /// The command as it will go on the wire.
    pub command: credsync_protocol::Command,
    /// The schema version it was authored under.
    ///
    /// `docs/spec.md` §7: an upgraded app migrates queued commands forward before pushing, so the
    /// version the command was *written* under must survive the upgrade that changes what the
    /// current version is. Migration itself is CS-20 (#21); recording what to migrate is here,
    /// because a command already sitting in the outbox when the app updates cannot be labelled
    /// retroactively.
    pub schema_version: SchemaVersion,
}

impl OutboxEntry {
    /// Wraps a command for queueing.
    #[must_use]
    pub const fn new(command: credsync_protocol::Command, schema_version: SchemaVersion) -> Self {
        Self {
            command,
            schema_version,
        }
    }

    /// The command's idempotency key.
    #[must_use]
    pub const fn id(&self) -> CommandId {
        self.command.id
    }
}

/// Why an outbox operation could not complete.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum OutboxError {
    /// The registry refused the command before it could be queued.
    ///
    /// `docs/spec.md` §6: server-authoritative entities are pull-only, refused by the registry
    /// rather than by convention.
    Registry(crate::registry::RegistryError),
    /// Storage refused the transaction, so nothing changed.
    Storage(crate::error::StorageError),
    /// A queued command could not be encoded.
    ///
    /// Should be unreachable: every field was validated on the way in. Kept as a value rather
    /// than a panic because a panic here would take down a host application over one bad row.
    Encoding,
}

impl core::fmt::Display for OutboxError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::Registry(e) => write!(f, "outbox refused the command: {e}"),
            Self::Storage(e) => write!(f, "outbox storage failure: {e}"),
            Self::Encoding => write!(f, "a queued command could not be encoded"),
        }
    }
}

impl core::error::Error for OutboxError {}

impl From<crate::registry::RegistryError> for OutboxError {
    fn from(e: crate::registry::RegistryError) -> Self {
        Self::Registry(e)
    }
}

impl From<crate::error::StorageError> for OutboxError {
    fn from(e: crate::error::StorageError) -> Self {
        Self::Storage(e)
    }
}

/// What one push response resolved.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct Resolved {
    /// Each command the response answered, with the outcome recorded for it.
    pub resolutions: Vec<(CommandId, Resolution)>,
    /// Results naming commands that were not queued.
    ///
    /// Ordinary rather than alarming: a retried push after a timeout returns the same results a
    /// second time, and the first copy already resolved them. Counted because a number that is
    /// *always* high means pushes are being retried far more than they should be.
    pub unknown: usize,
}

impl Resolved {
    /// The commands that need surfacing to the user.
    pub fn dead_lettered(&self) -> impl Iterator<Item = (&CommandId, &Reason)> {
        self.resolutions
            .iter()
            .filter_map(|(id, r)| r.reason().map(|reason| (id, reason)))
    }
}
