//! Fuzzes the `Command` decoder: a client-authored command envelope, which the *server* decodes from an untrusted client.
//!
//! See `_shared.rs` for what is asserted and why rejection is the expected outcome.

#![no_main]

#[path = "_shared.rs"]
mod shared;

use credsync_protocol::Command;
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    shared::decodes_and_round_trips::<Command>(data);
});
