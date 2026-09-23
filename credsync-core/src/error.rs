//! What the outside world is allowed to tell the engine went wrong.
//!
//! These are deliberately coarse. The core does not care whether a socket timed out or a proxy
//! returned nonsense — it cares whether retrying could plausibly help, because that is the only
//! question its answer depends on. A richer taxonomy would be a richer taxonomy the engine then
//! has to ignore.
//!
//! Every variant carries a host-supplied `detail` string for telemetry and logs. The engine
//! never branches on that string: matching on someone else's error text is how a refactor three
//! layers away silently changes retry behaviour here.

use core::fmt;

/// Why a storage transaction did not complete.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum StorageError {
    /// The transaction was rejected but the database is intact — a constraint violation, a
    /// serialization failure, a busy lock. Retrying is reasonable.
    Transient {
        /// The adapter's description, for telemetry only.
        detail: String,
    },
    /// The local database is unusable: corrupt file, unreadable device, a schema the adapter
    /// cannot open. Retrying the same transaction will fail the same way.
    ///
    /// Distinguished from [`Transient`](Self::Transient) because the recoveries differ
    /// completely. A transient failure wants backoff; this wants the scope rebuilt from the
    /// server, and the queued outbox preserved while that happens.
    Corrupt {
        /// The adapter's description, for telemetry only.
        detail: String,
    },
}

impl fmt::Display for StorageError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Transient { detail } => write!(f, "storage transient: {detail}"),
            Self::Corrupt { detail } => write!(f, "storage corrupt: {detail}"),
        }
    }
}

impl core::error::Error for StorageError {}

/// Why a request did not produce a usable response.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum TransportError {
    /// The request never reached a server, or its answer never came back: no connectivity,
    /// DNS failure, timeout, connection reset.
    ///
    /// **This is the ordinary case, not the exceptional one.** On the networks credSync is built
    /// for, a request failing this way is the median outcome of a sync attempt, and nothing about
    /// it should be treated as alarming.
    Unreachable {
        /// The adapter's description, for telemetry only.
        detail: String,
    },
    /// A response arrived with a status the engine must act on.
    ///
    /// The status is carried rather than pre-interpreted because the meanings are not
    /// interchangeable: `426` is the forced-upgrade envelope (`docs/spec.md` §7), `401` means the
    /// scope token needs minting again, `429` and `5xx` mean back off, `4xx` otherwise means the
    /// request was wrong and repeating it unchanged is pointless.
    Status {
        /// The HTTP status code.
        code: u16,
        /// The response body, when one was readable. Bounded by the adapter, not by the engine.
        body: Option<Vec<u8>>,
    },
    /// A response arrived but could not be decoded, or failed its checksum.
    ///
    /// Not merged with [`Status`](Self::Status): a corrupt batch is refetched (`docs/spec.md`
    /// §5), which is a different response from backing off, and folding the two together would
    /// lose the distinction exactly where it matters.
    Malformed {
        /// The decoder's description, for telemetry only.
        detail: String,
    },
}

impl fmt::Display for TransportError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Unreachable { detail } => write!(f, "transport unreachable: {detail}"),
            Self::Status { code, .. } => write!(f, "transport status: {code}"),
            Self::Malformed { detail } => write!(f, "transport malformed: {detail}"),
        }
    }
}

impl core::error::Error for TransportError {}
