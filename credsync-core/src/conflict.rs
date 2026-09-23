//! Conflict resolution per entity class. `docs/spec.md` §6.
//!
//! # The constraint that shapes everything here
//!
//! **Class policy may never make the client hold different rows from the server.**
//!
//! The scope digest is computed over the rows a client holds (`docs/spec.md` §5). Any rule that
//! caused the client to *skip* a change the server applied would put the two sides out of step
//! permanently — the client would report divergence on every pull, re-bootstrap, receive the same
//! change, skip it again, and loop forever. A conflict policy that quietly manufactures the exact
//! failure the divergence detector exists to catch is not a policy, it is a bug with a rationale.
//!
//! So the server remains the source of truth for row *content*, in every class. What the classes
//! actually govern is:
//!
//! | Class | What the registry enforces | What apply enforces |
//! |---|---|---|
//! | Server-authoritative | No command may target it | — rows apply normally |
//! | Owner draft | Commands allowed | a superseded edit is recovered, never dropped |
//! | Append-only | Commands allowed | an upsert over an existing row is a protocol violation |
//!
//! # Last-write-wins is decided by `row_version`, and only by `row_version`
//!
//! `client_ts` is a hint and nothing more. Device clocks are wrong constantly and wrong badly: a
//! phone whose date is three days fast would otherwise win every conflict it entered, and the
//! user whose clock happens to be correct would silently lose their work to whoever had the most
//! broken device. `docs/spec.md` §6 says `client_ts` is *"a tiebreaker hint only, because device
//! clocks lie"* — this module does not consult it at all, and [`lww_winner`] takes no timestamp
//! argument so that it cannot start to.

use credsync_protocol::RowVersion;

/// Which of two competing versions of a row wins.
///
/// Higher server-assigned `row_version` wins. There is deliberately no `client_ts` parameter:
/// a function that cannot see the clock cannot be corrupted by it, which is a stronger guarantee
/// than a comment asking the reader not to use it.
///
/// Equal versions return [`Lww::Tie`]. A tie means the same server version arrived twice, which
/// is a duplicate rather than a conflict — the caller keeps what it has.
#[must_use]
pub fn lww_winner(incoming: RowVersion, stored: RowVersion) -> Lww {
    match incoming.get().cmp(&stored.get()) {
        core::cmp::Ordering::Greater => Lww::Incoming,
        core::cmp::Ordering::Less => Lww::Stored,
        core::cmp::Ordering::Equal => Lww::Tie,
    }
}

/// The outcome of a last-write-wins comparison.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Lww {
    /// The arriving version is newer.
    Incoming,
    /// What is already stored is newer; the arriving version is stale.
    Stored,
    /// The same version. A duplicate, not a conflict.
    Tie,
}

impl Lww {
    /// Whether the arriving version should replace what is stored.
    #[must_use]
    pub const fn incoming_wins(self) -> bool {
        matches!(self, Self::Incoming)
    }
}
