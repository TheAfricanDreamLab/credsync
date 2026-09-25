//! Fuzzes the `CommandResult` decoder: one verdict, which decides whether a user's work is kept or dead-lettered.
//!
//! See `_shared.rs` for what is asserted and why rejection is the expected outcome.

#![no_main]

#[path = "_shared.rs"]
mod shared;

use credsync_protocol::CommandResult;
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    shared::decodes_and_round_trips::<CommandResult>(data);
});
