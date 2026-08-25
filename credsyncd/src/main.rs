//! Reference credSync server binary.
//!
//! A thin axum service around [`credsync_server`] that owns the wire protocol only: pull
//! pagination, command dedupe, result recording, scope-token validation and backpressure.
//! It is a reference implementation — hosts that are themselves Rust may embed
//! `credsync-server` as a library instead.
//!
//! # Status
//!
//! Scaffolded at CS-1. Wiring, scope-token validation and the scope blocklist arrive at CS-17.
//!
//! [`credsync_server`]: https://github.com/TheAfricanDreamLab/credsync/tree/main/credsync-server

#![forbid(unsafe_code)]

fn main() {
    // Wired at CS-17. Kept as a no-op rather than a `todo!()` so the workspace builds and the
    // clippy panic lints stay enforced from the first commit.
}
