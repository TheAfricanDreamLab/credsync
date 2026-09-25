//! Fuzzes the `ForcedUpgrade` decoder: the 426 envelope, decoded by a client that is already out of date.
//!
//! See `_shared.rs` for what is asserted and why rejection is the expected outcome.

#![no_main]

#[path = "_shared.rs"]
mod shared;

use credsync_protocol::ForcedUpgrade;
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    shared::decodes_and_round_trips::<ForcedUpgrade>(data);
});
