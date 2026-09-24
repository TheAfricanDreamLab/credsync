//! The server-side scope blocklist. `docs/spec.md` §8, Design v2.1 §8.
//!
//! # Why a blocklist exists at all
//!
//! Scope tokens are short-lived, and revocation is normally honoured by letting them expire. That
//! is the right default: it needs no shared state, no lookup on the hot path in the common case,
//! and no way for a revocation store to become a single point of failure.
//!
//! But "normally" is doing real work in that sentence. When a student is withdrawn, a staff member
//! is dismissed, or a device is reported stolen, waiting out a TTL is not an answer — the whole
//! point is that the cut happens *now*. So the blocklist is the escape hatch: a scope on it is
//! refused immediately, whatever a valid unexpired token says.
//!
//! # The check is fail-closed
//!
//! If the blocklist cannot be read, the request is refused rather than allowed. An availability
//! problem in this table must not become an authorization bypass, and the alternative — serving a
//! withdrawn student's data because a query timed out — is exactly the failure this table exists
//! to prevent.
//!
//! That is a deliberate trade: a database blip becomes a sync outage rather than a leak. It is the
//! right way round for coursework on shared devices.

use crate::error::ServerError;
use credsync_protocol::ScopeId;
use tokio_postgres::Client;

/// Whether a scope is currently cut.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Standing {
    /// Not blocked; the token's claims stand.
    Allowed,
    /// Blocked. Refuse regardless of what the token claims.
    Blocked,
}

impl Standing {
    /// Whether this standing permits the request.
    #[must_use]
    pub const fn is_allowed(self) -> bool {
        matches!(self, Self::Allowed)
    }
}

/// Looks up one scope's standing.
///
/// # Errors
/// Returns [`ServerError::Database`] if the table cannot be read. Callers must treat that as a
/// refusal — see the module docs on failing closed. [`check`] does this for you.
pub async fn standing(client: &Client, scope: &ScopeId) -> Result<Standing, ServerError> {
    let rows = client
        .query(
            "SELECT 1 FROM sync_scope_blocks WHERE scope = $1 LIMIT 1",
            &[&scope.as_str()],
        )
        .await?;

    if rows.is_empty() {
        Ok(Standing::Allowed)
    } else {
        Ok(Standing::Blocked)
    }
}

/// Refuses unless the scope is allowed, treating a database failure as a refusal.
///
/// This is the function to call on the request path. [`standing`] is for operators' tooling, where
/// distinguishing "not blocked" from "could not tell" is the point.
///
/// # Errors
/// Returns [`crate::auth::AuthError::ScopeBlocked`] if the scope is blocked **or** if its standing
/// could not be determined. The two are deliberately indistinguishable to the caller: an
/// authorization check that reports "I could not tell" invites somebody to treat it as a pass.
pub async fn check(client: &Client, scope: &ScopeId) -> Result<(), crate::auth::AuthError> {
    match standing(client, scope).await {
        Ok(Standing::Allowed) => Ok(()),
        Ok(Standing::Blocked) | Err(_) => Err(crate::auth::AuthError::ScopeBlocked {
            scope: scope.as_str().to_owned(),
        }),
    }
}

/// Cuts a scope immediately. Idempotent.
///
/// # Errors
/// Returns [`ServerError::Database`] if the write fails.
pub async fn block(client: &Client, scope: &ScopeId, reason: &str) -> Result<(), ServerError> {
    client
        .execute(
            "INSERT INTO sync_scope_blocks (scope, reason)
             VALUES ($1, $2)
             ON CONFLICT (scope) DO UPDATE SET reason = EXCLUDED.reason",
            &[&scope.as_str(), &reason],
        )
        .await?;
    Ok(())
}

/// Restores a scope. Idempotent.
///
/// # Errors
/// Returns [`ServerError::Database`] if the write fails.
pub async fn unblock(client: &Client, scope: &ScopeId) -> Result<(), ServerError> {
    client
        .execute(
            "DELETE FROM sync_scope_blocks WHERE scope = $1",
            &[&scope.as_str()],
        )
        .await?;
    Ok(())
}
