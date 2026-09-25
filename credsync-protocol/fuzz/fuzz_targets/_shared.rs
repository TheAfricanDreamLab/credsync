//! What every target asserts, so the targets themselves stay one line of intent each.
//!
//! # What a decoder must never do
//!
//! **Never panic.** Every byte here arrives from the network, and `docs/spec.md` §2.1 is explicit
//! that oversize input is *rejected, not truncated*. A panic in a decoder is a remote crash: an
//! attacker, a broken proxy, or a corrupted flash page takes the app down, and on a phone that is
//! indistinguishable from the app simply not working.
//!
//! **Never accept what it cannot re-emit.** A value that decodes but whose canonical encoding
//! differs from what produced it is worse than a rejection: checksums and digests are computed over
//! that encoding (`docs/spec.md` §5), so the two sides would compute different digests for a value
//! they both believe they hold. That is silent divergence arriving through the front door.

#![allow(dead_code)]

use credsync_protocol::canonical;
use serde::Serialize;
use serde::de::DeserializeOwned;

/// Decodes arbitrary bytes, and if they decode, insists the value round-trips byte for byte.
///
/// The round-trip half is the part worth having. Checking only "does not panic" would pass a
/// decoder that quietly normalised its input — dropping an unknown field, coercing a number,
/// accepting two encodings of one value — and each of those puts two peers at different digests
/// while both report success.
pub fn decodes_and_round_trips<T>(data: &[u8])
where
    T: DeserializeOwned + Serialize,
{
    let Ok(value) = canonical::from_slice::<T>(data) else {
        // Rejection is the expected outcome for almost every input. It is also the *correct* one:
        // the point of the fuzzer is that rejecting is all this decoder ever does with garbage.
        return;
    };

    let encoded = canonical::to_vec(&value)
        .expect("a value that decoded must encode; the codec is not allowed to be one-way");

    let again = canonical::from_slice::<T>(&encoded)
        .expect("a value this decoder produced must decode again, or the codec disagrees with itself");

    let re_encoded = canonical::to_vec(&again).expect("re-encoding a decoded value must succeed");

    assert_eq!(
        encoded, re_encoded,
        "canonical encoding is not stable: the same logical value produced two different byte \
         strings, so two peers holding it would compute different digests"
    );
}
