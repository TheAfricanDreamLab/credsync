//! The sans-IO deterministic state machine at the heart of a credSync client.
//!
//! # The sans-IO rule
//!
//! This crate performs **no I/O and never consults the real world**. Everything that touches
//! reality arrives through four traits — [`Clock`], [`Entropy`], [`Storage`], [`Transport`] and [`Compressor`] —
//! supplied by the caller.
//!
//! In production those are real implementations. In [`credsync_sim`] they are seeded fakes. The
//! same core bytes run in both worlds, and that is the entire trick that makes deterministic
//! simulation possible.
//!
//! This is not a stylistic preference. A single hidden clock read destroys deterministic replay
//! silently: the simulator keeps passing while quietly losing the ability to find bugs.
//!
//! # The shape
//!
//! ```text
//!   Event  ──▶  Engine  ──▶  Effect
//!                 │
//!                 └── holds Clock, Entropy, Storage, Transport, Compressor
//! ```
//!
//! Events go in; effects come out; the four implementations are held by the engine and used by it
//! directly. Storage answers immediately, so a write's outcome is a return value. Transport does
//! not, so a request's outcome arrives later as [`Event::TransportResponse`]. See D-037 in
//! `docs/DECISIONS.md` for why the boundary is drawn there.
//!
//! An [`Effect`] is therefore specifically *what the four traits cannot express*: being woken
//! later, and telling the host something worth knowing.
//!
//! # Single-threaded by construction
//!
//! Nothing here is bounded by `Send` or `Sync`. One engine is driven by one thread. React
//! Native's Hermes is single-threaded, and a `Send` bound would force every binding to wrap its
//! database handle in a mutex to satisfy a constraint the design never had.
//!
//! # What must never appear in this crate
//!
//! No executor, no ambient clock read, no sleeping, no randomness that did not come from
//! [`Entropy`], no HTTP client, no filesystem or database access, and no `unsafe`. Two CI gates
//! hold this: one asserts the dependency graph is free of I/O crates, the other greps the source
//! itself, because a ban list checked only against `Cargo.toml` misses `std::time`, which needs
//! no dependency at all. Run `./scripts/check-sans-io.sh` locally; see
//! `.claude/skills/rust-sans-io/SKILL.md` for the full list and the reasoning behind each entry.
//!
//! # Status
//!
//! Scaffolded at CS-1. The four traits and the event/effect surface landed at CS-6. State
//! transitions begin at CS-7 — there is no `handle` method yet, deliberately.
//!
//! [`credsync_sim`]: https://github.com/TheAfricanDreamLab/credsync/tree/main/credsync-sim

#![forbid(unsafe_code)]

pub mod apply;
pub mod conflict;
pub mod effect;
pub mod engine;
pub mod error;
pub mod event;
pub mod outbox;
pub mod registry;
pub mod scope;
pub mod storage;
pub mod traits;
pub mod types;
pub mod wire;

pub use apply::{Applied, ApplyError};
pub use conflict::{Lww, lww_winner};
pub use effect::{Effect, Telemetry};
pub use engine::Engine;
pub use error::{StorageError, TransportError};
pub use event::Event;
pub use outbox::{OutboxEntry, OutboxError, Resolution, Resolved};
pub use registry::{Registry, RegistryError};
pub use scope::ScopeState;
pub use storage::{StorageOp, TxOutcome};
pub use traits::{Clock, Compressor, Entropy, Storage, Transport};
pub use types::{RequestId, Timestamp};
pub use wire::{WireRequest, WireResponse};
