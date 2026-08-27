//! CS-4: batch checksums and the order-independent scope digest.
//!
//! The digest is what makes silent divergence *observable*. Every property here is one that must
//! hold for it to be trustworthy — and each is stated as a property rather than examples, because
//! "we tried three orderings and they matched" proves nothing about the fourth.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

mod common;

use credsync_protocol::{
    EntityId, EntityName, RowVersion, ScopeDigest, canonical, checksum, checksum_bytes,
    payload_checksum_bytes,
};
use proptest::prelude::*;

/// One live row: `(entity, entity_id, row_version)`.
type Row = (EntityName, EntityId, RowVersion);

fn row() -> impl Strategy<Value = Row> {
    (
        common::entity_name(),
        common::entity_id(),
        common::row_version(),
    )
}

/// A set of rows with distinct `(entity, entity_id)` keys, which is what a real scope holds.
fn rows() -> impl Strategy<Value = Vec<Row>> {
    proptest::collection::vec(row(), 0..24).prop_map(|mut v| {
        v.sort_by(|a, b| (a.0.as_str(), a.1.as_str()).cmp(&(b.0.as_str(), b.1.as_str())));
        v.dedup_by(|a, b| a.0 == b.0 && a.1 == b.1);
        v
    })
}

fn digest_of(rows: &[Row]) -> ScopeDigest {
    ScopeDigest::from_rows(rows.iter().map(|(e, i, v)| (e, i, *v)))
}

proptest! {
    #![proptest_config(ProptestConfig { cases: 256, ..ProptestConfig::default() })]

    /// DoD 1a: order-independence, across arbitrary permutations rather than a reversal.
    ///
    /// This is the property that lets two machines which built the same scope in different
    /// orders — a fresh bootstrap versus weeks of incremental pulls — agree.
    #[test]
    fn digest_is_order_independent(rows in rows(), seed in any::<u64>()) {
        let mut shuffled = rows.clone();
        // A deterministic shuffle driven by the generated seed: no ambient randomness, so a
        // failure replays exactly.
        let n = shuffled.len();
        if n > 1 {
            let mut state = seed | 1;
            for i in (1..n).rev() {
                state = state.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
                let j = (state >> 33) as usize % (i + 1);
                shuffled.swap(i, j);
            }
        }
        prop_assert_eq!(digest_of(&rows), digest_of(&shuffled));
    }

    /// DoD 4: incremental equals batch.
    ///
    /// Applying N changes one at a time must land exactly where computing the digest over the
    /// whole set does. Without this the client's running digest and the server's freshly computed
    /// one would drift apart and every scope would eventually report a phantom divergence.
    #[test]
    fn digest_is_incremental(rows in rows()) {
        let mut incremental = ScopeDigest::EMPTY;
        for (entity, entity_id, row_version) in &rows {
            incremental.add(entity, entity_id, *row_version);
        }
        prop_assert_eq!(incremental, digest_of(&rows));
    }

    /// DoD 1b: a tombstone leaves the digest as though the row had never existed.
    ///
    /// Exactly, not approximately — `remove` is the true inverse of `add`.
    #[test]
    fn removing_a_row_is_the_exact_inverse_of_adding_it(rows in rows(), extra in row()) {
        // Skip when the extra row collides with one already present: a scope holds one row per key.
        prop_assume!(!rows.iter().any(|(e, i, _)| *e == extra.0 && *i == extra.1));

        let before = digest_of(&rows);
        let mut d = before;
        d.add(&extra.0, &extra.1, extra.2);
        prop_assert_ne!(d, before, "adding a row did not change the digest");
        d.remove(&extra.0, &extra.1, extra.2);
        prop_assert_eq!(d, before, "removing a row did not restore the digest");
    }

    /// DoD 1c: `row_version` sensitivity.
    ///
    /// The same row at a different version must produce a different digest, or an edit that only
    /// bumps the version would be invisible to divergence detection.
    #[test]
    fn digest_is_row_version_sensitive(r in row(), other in common::row_version()) {
        prop_assume!(r.2 != other);
        let mut a = ScopeDigest::EMPTY;
        a.add(&r.0, &r.1, r.2);
        let mut b = ScopeDigest::EMPTY;
        b.add(&r.0, &r.1, other);
        prop_assert_ne!(a, b);
    }

    /// An update is remove-then-add, and lands where a from-scratch computation does.
    #[test]
    fn update_matches_a_fresh_computation(rows in rows(), to in common::row_version()) {
        prop_assume!(!rows.is_empty());
        let mut updated = rows.clone();
        let (entity, entity_id, from) = rows[0].clone();
        prop_assume!(from != to);
        updated[0].2 = to;

        let mut d = digest_of(&rows);
        d.update(&entity, &entity_id, from, to);
        prop_assert_eq!(d, digest_of(&updated));
    }

    /// DoD 2: a checksum detects single-bit corruption, in every wire type.
    ///
    /// One flipped bit anywhere in the encoding must change the checksum. This is the whole
    /// promise of "detected before apply, and the batch refetched, never half-applied".
    #[test]
    fn checksum_detects_single_bit_corruption(value in common::batch(), bit in any::<u16>()) {
        let bytes = canonical::to_vec(&value).expect("encodes");
        prop_assume!(!bytes.is_empty());

        let index = (bit as usize / 8) % bytes.len();
        let mask = 1u8 << (bit % 8);
        let mut corrupted = bytes.clone();
        corrupted[index] ^= mask;

        prop_assert_ne!(
            checksum_bytes(&bytes),
            checksum_bytes(&corrupted),
            "a single flipped bit did not change the checksum"
        );
    }

    /// The same, for the tamper-evident command checksum.
    #[test]
    fn payload_checksum_detects_single_bit_corruption(
        value in common::command(),
        bit in any::<u16>()
    ) {
        let bytes = canonical::to_vec(&value).expect("encodes");
        prop_assume!(!bytes.is_empty());

        let index = (bit as usize / 8) % bytes.len();
        let mut corrupted = bytes.clone();
        corrupted[index] ^= 1u8 << (bit % 8);

        prop_assert_ne!(
            payload_checksum_bytes(&bytes),
            payload_checksum_bytes(&corrupted)
        );
    }

    /// A checksum is stable: the same value always produces the same value.
    #[test]
    fn checksum_is_deterministic(value in common::pull_response()) {
        prop_assert_eq!(
            checksum(&value).expect("checksums"),
            checksum(&value).expect("checksums")
        );
    }
}

/// Duplicate rows must not cancel out.
///
/// This is why the digest sums rather than XORs. XOR is self-inverse, so two identical rows would
/// produce a digest identical to a scope where the row is absent — and the mechanism whose whole
/// purpose is noticing that state has silently diverged would report agreement. Addition doubles.
#[test]
fn duplicate_rows_do_not_cancel() {
    let entity = EntityName::new("lessons").expect("valid");
    let id = EntityId::new("l1").expect("valid");
    let version = RowVersion::new(1).expect("valid");

    let mut once = ScopeDigest::EMPTY;
    once.add(&entity, &id, version);

    let mut twice = ScopeDigest::EMPTY;
    twice.add(&entity, &id, version);
    twice.add(&entity, &id, version);

    assert_ne!(
        twice,
        ScopeDigest::EMPTY,
        "two identical rows cancelled to empty - the digest is XOR-like and cannot detect duplication"
    );
    assert_ne!(
        twice, once,
        "a duplicated row was indistinguishable from a single one"
    );
}

/// Field boundaries must be unambiguous.
///
/// Without length prefixes, `("lesson", "a1")` and `("lessona", "1")` concatenate to the same
/// bytes, so two genuinely different scopes would agree on a digest.
#[test]
fn field_boundaries_are_unambiguous() {
    let version = RowVersion::new(1).expect("valid");

    let mut a = ScopeDigest::EMPTY;
    a.add(
        &EntityName::new("lesson").expect("valid"),
        &EntityId::new("a1").expect("valid"),
        version,
    );

    let mut b = ScopeDigest::EMPTY;
    b.add(
        &EntityName::new("lessona").expect("valid"),
        &EntityId::new("1").expect("valid"),
        version,
    );

    assert_ne!(
        a, b,
        "field boundaries are ambiguous - concatenation collides"
    );
}

/// An empty scope is the additive identity, so a scope emptied by tombstones returns to it.
#[test]
fn emptying_a_scope_returns_to_the_empty_digest() {
    let entity = EntityName::new("reflections").expect("valid");
    let version = RowVersion::new(7).expect("valid");
    let ids: Vec<EntityId> = (0..8)
        .map(|n| EntityId::new(format!("r{n}")).expect("valid"))
        .collect();

    let mut digest = ScopeDigest::EMPTY;
    for id in &ids {
        digest.add(&entity, id, version);
    }
    assert_ne!(digest, ScopeDigest::EMPTY);

    // Remove in a different order from insertion, since order must not matter.
    for id in ids.iter().rev() {
        digest.remove(&entity, id, version);
    }
    assert_eq!(
        digest,
        ScopeDigest::EMPTY,
        "a scope emptied by tombstones did not return to the empty digest"
    );
}

/// The wire form is always 32 lowercase hex characters, including when leading bytes are zero.
///
/// A width that varied with the value would let two different digests render identically once
/// leading zeros were dropped.
#[test]
fn hex_form_has_a_fixed_width() {
    assert_eq!(ScopeDigest::EMPTY.to_hex().as_str(), "0".repeat(32));

    let entity = EntityName::new("a").expect("valid");
    let id = EntityId::new("b").expect("valid");
    let version = RowVersion::new(1).expect("valid");
    let mut d = ScopeDigest::EMPTY;
    d.add(&entity, &id, version);
    assert_eq!(d.to_hex().as_str().len(), 32);
    assert!(d.to_hex().as_str().chars().all(|c| c.is_ascii_hexdigit()));
}

/// The two algorithms must not be interchangeable by accident: a value's corruption checksum and
/// its tamper-evident checksum are different things and must not be compared.
#[test]
fn corruption_and_tamper_checksums_are_distinct() {
    let bytes = b"the same input";
    assert_ne!(
        checksum_bytes(bytes),
        payload_checksum_bytes(bytes),
        "xxh3 and BLAKE3 produced the same value - one of them is not being used"
    );
}

/// Fixed vectors, pinning that these are *real* xxh3 and BLAKE3 rather than merely
/// self-consistent functions.
///
/// Every property test above compares our output against our own output. All of them would keep
/// passing if a dependency bump silently changed the algorithm, if a feature flag switched xxh3's
/// seed, or if the BLAKE3 truncation moved from the first 16 bytes to the last. Two sides running
/// different credSync versions would then disagree on every digest — the divergence detector
/// causing divergence.
///
/// These values were produced by this implementation and are now frozen. If one changes, the wire
/// format changed: that is a protocol break requiring a version bump, not a fixture update.
#[test]
fn checksum_vectors_are_frozen() {
    const CASES: &[(&[u8], &str, &str)] = &[
        // The empty-input rows are checkable against the algorithms' own published vectors:
        // XXH3-128("") and BLAKE3("") truncated to 16 bytes. If these two match, the crates are
        // computing real xxh3 and real BLAKE3 rather than something merely deterministic.
        (
            b"",
            "99aa06d3014798d86001c324468d497f",
            "af1349b9f5f9a1a6a0404dea36dcc949",
        ),
        (
            b"a",
            "a96faf705af16834e6c632b61e964e1f",
            "17762fddd969a453925d65717ac3eea2",
        ),
        (
            b"credsync",
            "254b202a78f16d981d7372cfbc385c64",
            "73ad155816af6db8ea75cd5ce99da282",
        ),
    ];
    for (input, want_xxh3, want_blake3) in CASES {
        assert_eq!(
            checksum_bytes(input).as_str(),
            *want_xxh3,
            "xxh3 vector changed for {input:?} - the wire format moved"
        );
        assert_eq!(
            payload_checksum_bytes(input).as_str(),
            *want_blake3,
            "BLAKE3 vector changed for {input:?} - the wire format moved"
        );
    }
}

/// The empty scope digest is the additive identity, and must stay that value on the wire.
#[test]
fn empty_digest_vector_is_frozen() {
    assert_eq!(ScopeDigest::EMPTY.to_hex().as_str(), "0".repeat(32));
}

/// A known row produces a known digest. Pins the row-hash construction — the length prefixes,
/// the big-endian row_version, and the field order — all of which are wire format.
#[test]
fn row_digest_vector_is_frozen() {
    let mut d = ScopeDigest::EMPTY;
    d.add(
        &EntityName::new("lessons").expect("valid"),
        &EntityId::new("l1").expect("valid"),
        RowVersion::new(1).expect("valid"),
    );
    assert_eq!(
        d.to_hex().as_str(),
        "895b94cd12efa8346b73a3122de3edb8",
        "the row-hash construction changed - this is a protocol break"
    );
}
