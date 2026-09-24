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

#[cfg(feature = "postgres")]
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
