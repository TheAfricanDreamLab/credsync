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

/// How much this client trusts its own copy of a scope. `docs/spec.md` §5.
///
/// A digest mismatch means the two sides walked the same log and hold different rows — silent
/// divergence, the class of bug that testing misses and users never report until trust is gone.
/// The response is to stop trusting the local copy and rebuild it, not to carry on applying
/// changes to a base that is already wrong.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScopeHealth {
    /// The client's digest matched the server's after the last apply.
    Healthy,

    /// Diverged, and a re-bootstrap is needed before this scope is trusted again.
    ///
    /// Ordinary pull is not useful here: it would apply new changes on top of a base already known
    /// to be wrong, and the digest would keep disagreeing for reasons that have nothing to do with
    /// the new changes.
    Tainted {
        /// How many times this scope has diverged, including this one.
        attempts: u32,
    },

    /// Diverged before, rebuilt, and now agrees again.
    ///
    /// Distinct from [`Healthy`](Self::Healthy) because the history matters: a scope that diverges,
    /// heals, and diverges again is the repeated divergence the escalation exists for. Forgetting
    /// on each apparent success would let that loop run for ever.
    Healed {
        /// How many times it has diverged.
        attempts: u32,
    },

    /// Diverged again after re-bootstrapping, repeatedly. Automatic healing has given up.
    ///
    /// **Escalation, not a louder retry.** A scope that diverges immediately after being rebuilt
    /// from the server's own snapshot is not suffering a transient fault — something is
    /// systematically wrong, in an adapter, a migration, or the server. Looping would hide that
    /// behind a device quietly re-downloading the same scope forever, burning the data budget of
    /// exactly the users this project exists for.
    Unhealable {
        /// How many times this scope has diverged.
        attempts: u32,
    },
}

impl ScopeHealth {
    /// Whether this scope can be synced normally.
    #[must_use]
    pub const fn is_healthy(self) -> bool {
        matches!(self, Self::Healthy | Self::Healed { .. })
    }

    /// Whether this scope should be re-bootstrapped.
    #[must_use]
    pub const fn needs_rebootstrap(self) -> bool {
        matches!(self, Self::Tainted { .. })
    }

    /// How many times this scope has diverged.
    #[must_use]
    pub const fn attempts(self) -> u32 {
        match self {
            Self::Healthy => 0,
            Self::Tainted { attempts }
            | Self::Healed { attempts }
            | Self::Unhealable { attempts } => attempts,
        }
    }
}
