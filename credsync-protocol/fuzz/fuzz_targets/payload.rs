//! Fuzzes the `Payload` decoder: a command document: bounded at 64 KB, no floats.
//!
//! See `_shared.rs` for what is asserted and why rejection is the expected outcome.

#![no_main]

#[path = "_shared.rs"]
mod shared;

use credsync_protocol::Payload;
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    shared::decodes_and_round_trips::<Payload>(data);
});
