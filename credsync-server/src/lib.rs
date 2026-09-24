//! Server-side credSync: pull pagination, command dedupe, scope-token validation and host
//! forwarding.
//!
//! # Domain logic does not live here
//!
//! This crate owns the wire protocol and nothing else. Validated commands are forwarded to the
//! host application's registered endpoint, which applies its own business rules and writes state
//! plus change-log rows in one transaction. credSync records the outcome against the command id.
//!
//! That boundary is what keeps the engine backend-agnostic: any stack that can expose one HTTP
//! endpoint and write two tables can adopt credSync without surrendering its domain model.
//!
//! # Status
//!
//! Scaffolded at CS-1. Schema and pagination arrive at CS-14, dedupe at CS-15, host forwarding
//! at CS-16, scope-token validation and the blocklist at CS-17.
//!
//! # The `postgres` feature
//!
//! On by default. Turning it off leaves the parts of this crate that are **pure functions over
//! wire types** — the compressed byte budget ([`pull::fill_batch`]), the token verifier
//! ([`auth`]), and the dedupe rule ([`dedupe::decide`]) — with no database in the dependency
//! graph.
//!
//! That exists for the simulator. `credsync-sim` must be deterministic, so it cannot talk to a
//! database; but a simulator testing its *own* reimplementation of the byte budget is testing the
//! copy, not the code. With the feature off it runs this crate's real logic inside a seeded run
//! (CS-18).

#![forbid(unsafe_code)]

pub mod auth;
#[cfg(feature = "postgres")]
pub mod blocklist;
#[cfg(feature = "postgres")]
pub mod db;
pub mod dedupe;
pub mod error;
pub mod host;
pub mod pull;
pub mod version;

pub use auth::{AuthError, Claims, Verifier};
#[cfg(feature = "postgres")]
pub use blocklist::Standing;
#[cfg(feature = "postgres")]
pub use db::NewChange;
pub use dedupe::{Decision, DedupeStats, Recorded, decide};
pub use error::ServerError;
pub use host::{Forwarded, Host, HostError, HostOutcome};
pub use pull::{ChangeRow, Compressor, DEFAULT_BUDGET_BYTES, fill_batch};
pub use version::{Negotiated, VersionPolicy};
