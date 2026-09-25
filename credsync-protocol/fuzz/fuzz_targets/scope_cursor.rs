//! Fuzzes the `ScopeCursor` decoder: one scope-and-cursor pair from a pull request.
//!
//! See `_shared.rs` for what is asserted and why rejection is the expected outcome.

#![no_main]

#[path = "_shared.rs"]
mod shared;

use credsync_protocol::ScopeCursor;
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    shared::decodes_and_round_trips::<ScopeCursor>(data);
});
