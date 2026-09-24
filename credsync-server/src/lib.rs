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

#![forbid(unsafe_code)]

pub mod auth;
pub mod blocklist;
pub mod db;
pub mod dedupe;
pub mod error;
pub mod host;
pub mod pull;

pub use auth::{AuthError, Claims, Verifier};
pub use blocklist::Standing;
pub use db::NewChange;
pub use dedupe::{Decision, DedupeStats};
pub use error::ServerError;
pub use host::{Forwarded, Host, HostError, HostOutcome};
pub use pull::{ChangeRow, Compressor, DEFAULT_BUDGET_BYTES, fill_batch};
