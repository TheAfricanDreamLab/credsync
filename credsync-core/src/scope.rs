//! Per-scope sync state: how far this client has walked, and what it believes it holds.

use credsync_protocol::{Cursor, ScopeDigest};

/// What the engine tracks for one scope.
///
/// Both fields are also persisted — the cursor in `credsync_cursors`, the digest in
/// `credsync_meta` (Design §4.3) — and written in the same transaction as the rows they describe.
/// The copy here is a cache of what storage holds, never a second source of truth: on restart the
/// engine is handed the persisted values through
/// [`Engine::restore_scope`](crate::Engine::restore_scope), and if this copy ever disagrees with
/// the database, the database is right.
///
/// That ordering matters. The in-memory copy is only advanced *after* a transaction commits, so a
/// failed commit leaves it describing the state the database is actually in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ScopeState {
    /// The last `seq` this client has applied, or [`Cursor::START`] if it has applied none.
    pub cursor: Cursor,
    /// The client's digest over the scope's live rows.
    pub digest: ScopeDigest,
}

impl ScopeState {
    /// A scope this client has never synced.
    pub const NEW: Self = Self {
        cursor: Cursor::START,
        digest: ScopeDigest::EMPTY,
    };

    /// State restored from storage.
    #[must_use]
    pub const fn restored(cursor: Cursor, digest: ScopeDigest) -> Self {
        Self { cursor, digest }
    }
}

impl Default for ScopeState {
    fn default() -> Self {
        Self::NEW
    }
}
