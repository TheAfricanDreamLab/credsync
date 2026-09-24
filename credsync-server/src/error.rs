//! What the server refuses, and why.

use credsync_protocol::ProtocolError;

/// Everything that can go wrong serving a request.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum ServerError {
    /// The database holds something the protocol says cannot exist.
    ///
    /// Refused loudly rather than forwarded. A row that failed validation on the way out is a row
    /// that was written without passing validation on the way in, and sending it to every client
    /// that asks would spread the damage rather than contain it.
    Corrupt {
        /// What was wrong, for the operator reading the log.
        detail: String,
    },

    /// A value read from the database failed the protocol's own validation.
    Protocol(ProtocolError),

    /// The database could not be reached or the query failed.
    Database {
        /// The driver's description.
        detail: String,
    },
}

impl core::fmt::Display for ServerError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::Corrupt { detail } => write!(f, "corrupt change log: {detail}"),
            Self::Protocol(e) => write!(f, "invalid value in the change log: {e}"),
            Self::Database { detail } => write!(f, "database failure: {detail}"),
        }
    }
}

impl core::error::Error for ServerError {}

impl From<ProtocolError> for ServerError {
    fn from(e: ProtocolError) -> Self {
        Self::Protocol(e)
    }
}
