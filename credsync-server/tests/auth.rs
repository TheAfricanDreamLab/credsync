//! CS-17: scope-token validation, written adversarially.
//!
//! These are not "does a good token work" tests. A token check that accepts every valid token and
//! also a forged one passes every happy-path test ever written, so almost everything below is an
//! attack: a swapped algorithm, an unsigned token, the wrong key, a token for somebody else's
//! tenant, an expired one presented after revocation.
//!
//! The one happy-path test exists to prove the rest are not passing vacuously.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use base64::Engine as _;
use credsync_protocol::ScopeId;
use credsync_server::auth::{AuthError, Verifier};
use jsonwebtoken::{Algorithm, DecodingKey, EncodingKey, Header, encode};
use serde_json::json;

const SECRET: &[u8] = b"a-test-signing-secret-that-is-long-enough";
const OTHER_SECRET: &[u8] = b"a-completely-different-signing-secret!!!!!";

fn scope(s: &str) -> ScopeId {
    ScopeId::new(s).expect("valid scope")
}

/// Seconds since the Unix epoch, offset by `delta`.
fn epoch(delta: i64) -> i64 {
    let now = i64::try_from(
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("after 1970")
            .as_secs(),
    )
    .expect("fits");
    now + delta
}

fn verifier() -> Verifier {
    Verifier::new(DecodingKey::from_secret(SECRET), Algorithm::HS256, 0)
}

/// Mints a token the honest way.
fn mint(alg: Algorithm, secret: &[u8], claims: &serde_json::Value) -> String {
    encode(&Header::new(alg), claims, &EncodingKey::from_secret(secret)).expect("encodes")
}

fn valid_claims(scopes: &[&str]) -> serde_json::Value {
    json!({ "scopes": scopes, "exp": epoch(3600), "sub": "user-42" })
}

/// Assembles a token from raw parts, so a test can write a header no honest minter would.
///
/// Needed for the `alg: none` attack in particular: `jsonwebtoken` will not *produce* one, which
/// is correct of it and useless for testing whether we *reject* one.
fn forge(header: &serde_json::Value, claims: &serde_json::Value, signature: &str) -> String {
    let b64 = base64::engine::general_purpose::URL_SAFE_NO_PAD;
    format!(
        "{}.{}.{}",
        b64.encode(serde_json::to_vec(header).expect("encodes")),
        b64.encode(serde_json::to_vec(claims).expect("encodes")),
        signature
    )
}

// -------------------------------------------------------------------------------------------
// The one happy path, so the rest are not vacuous
// -------------------------------------------------------------------------------------------

#[test]
fn a_well_formed_token_yields_its_claims() {
    let token = mint(
        Algorithm::HS256,
        SECRET,
        &valid_claims(&["inst:acme", "inst:beta"]),
    );

    let claims = verifier().verify(&token).expect("verifies");

    assert_eq!(claims.scopes.len(), 2);
    assert!(claims.claims_scope(&scope("inst:acme")));
    assert!(claims.claims_scope(&scope("inst:beta")));
    assert_eq!(claims.subject.as_deref(), Some("user-42"));
    claims.authorize(&scope("inst:acme")).expect("authorized");
}

// -------------------------------------------------------------------------------------------
// DoD: a token is refused for any scope not in its claims
// -------------------------------------------------------------------------------------------

#[test]
fn a_scope_outside_the_claims_is_refused() {
    let token = mint(Algorithm::HS256, SECRET, &valid_claims(&["inst:acme"]));
    let claims = verifier().verify(&token).expect("verifies");

    let err = claims
        .authorize(&scope("inst:someone-else"))
        .expect_err("a scope outside the claims must be refused");
    assert!(matches!(err, AuthError::ScopeNotClaimed { .. }), "{err:?}");
}

/// Scope matching is exact: no prefixes, no wildcards, no hierarchy.
///
/// `inst:acme` must not reach `inst:acme-corp`. Any prefix rule puts a tenant breach one character
/// away from a plausible scope name, and these names are chosen by hosts, not by us.
#[test]
fn scope_matching_has_no_prefix_or_wildcard_semantics() {
    let token = mint(Algorithm::HS256, SECRET, &valid_claims(&["inst:acme"]));
    let claims = verifier().verify(&token).expect("verifies");

    for neighbour in [
        "inst:acme-corp", // the claimed scope is a prefix of this
        "inst:acm",       // this is a prefix of the claimed scope
        "inst:acme:sub",  // a plausible "child" scope
        "INST:ACME",      // case must matter
        "inst:acme ",     // would not even be a valid scope, but assert the intent
    ] {
        let Ok(s) = ScopeId::new(neighbour) else {
            continue; // invalid scope ids cannot be requested at all, which is also fine
        };
        assert!(
            claims.authorize(&s).is_err(),
            "a token for 'inst:acme' reached '{neighbour}'"
        );
    }
}

// -------------------------------------------------------------------------------------------
// DoD: expired, unsigned, wrong-key and algorithm-confusion tokens, one test per case
// -------------------------------------------------------------------------------------------

#[test]
fn an_expired_token_is_refused() {
    let token = mint(
        Algorithm::HS256,
        SECRET,
        &json!({ "scopes": ["inst:acme"], "exp": epoch(-60) }),
    );

    let err = verifier().verify(&token).expect_err("expired");
    assert_eq!(err, AuthError::Expired);
    assert!(
        err.is_expired(),
        "a client must be able to tell it needs a fresh token"
    );
}

/// A token with no `exp` at all is malformed, not eternal.
///
/// Revocation is honoured at expiry, so accepting this would mint a permanent grant from a missing
/// field — the most expensive kind of default.
#[test]
fn a_token_without_an_expiry_is_refused() {
    let token = mint(
        Algorithm::HS256,
        SECRET,
        &json!({ "scopes": ["inst:acme"] }),
    );

    let err = verifier().verify(&token).expect_err("no exp");
    assert!(
        !matches!(err, AuthError::Expired),
        "a missing exp must not be reported as expiry: {err:?}"
    );
}

/// `alg: none` — the unsigned token.
#[test]
fn an_unsigned_token_is_refused() {
    let token = forge(
        &json!({ "alg": "none", "typ": "JWT" }),
        &valid_claims(&["inst:acme"]),
        "",
    );

    let err = verifier().verify(&token).expect_err("alg none");
    assert!(
        matches!(
            err,
            AuthError::WrongAlgorithm { .. } | AuthError::Malformed { .. }
        ),
        "an unsigned token was not refused as such: {err:?}"
    );
}

/// `alg: none` with a signature attached anyway, in case emptiness was doing the work.
#[test]
fn an_unsigned_token_with_a_junk_signature_is_refused() {
    let token = forge(
        &json!({ "alg": "none", "typ": "JWT" }),
        &valid_claims(&["inst:acme"]),
        "bm90LWEtc2lnbmF0dXJl",
    );

    assert!(
        verifier().verify(&token).is_err(),
        "an unsigned token with a decorative signature was accepted"
    );
}

#[test]
fn a_token_signed_with_the_wrong_key_is_refused() {
    let token = mint(
        Algorithm::HS256,
        OTHER_SECRET,
        &valid_claims(&["inst:acme"]),
    );

    let err = verifier().verify(&token).expect_err("wrong key");
    assert_eq!(err, AuthError::BadSignature);
}

/// Algorithm confusion: a correctly-signed token whose algorithm is not the pinned one.
///
/// This is the shape that breaks servers which read `alg` from the token to decide how to verify.
/// Here the token is genuinely valid HS512 signed with the real secret — the only thing wrong with
/// it is that this verifier does not accept HS512.
#[test]
fn a_token_signed_with_a_different_algorithm_is_refused() {
    let token = mint(Algorithm::HS512, SECRET, &valid_claims(&["inst:acme"]));

    let err = verifier().verify(&token).expect_err("wrong algorithm");
    assert!(matches!(err, AuthError::WrongAlgorithm { .. }), "{err:?}");
}

/// Algorithm confusion by header edit: a valid HS256 token relabelled as something else.
#[test]
fn a_token_whose_header_algorithm_was_swapped_is_refused() {
    let claims = valid_claims(&["inst:acme"]);
    let genuine = mint(Algorithm::HS256, SECRET, &claims);
    let signature = genuine.rsplit('.').next().expect("three parts");

    for alg in [
        "HS384", "HS512", "RS256", "ES256", "EdDSA", "none", "NONE", "hs256",
    ] {
        let forged = forge(&json!({ "alg": alg, "typ": "JWT" }), &claims, signature);
        assert!(
            verifier().verify(&forged).is_err(),
            "a token relabelled as '{alg}' was accepted"
        );
    }
}

/// A `kid` header must not influence anything.
///
/// A key id chosen by the attacker that selects the verification key is algorithm confusion in a
/// different hat. credSync ignores `kid` entirely; this asserts that ignoring it does not mean
/// accepting whatever comes with it.
#[test]
fn a_kid_header_does_not_change_the_outcome() {
    let claims = valid_claims(&["inst:acme"]);

    let mut header = Header::new(Algorithm::HS256);
    header.kid = Some("../../etc/passwd".to_owned());
    let with_kid = encode(&header, &claims, &EncodingKey::from_secret(SECRET)).expect("encodes");
    assert!(
        verifier().verify(&with_kid).is_ok(),
        "a legitimate token was refused merely for carrying a kid"
    );

    let mut header = Header::new(Algorithm::HS256);
    header.kid = Some("any-key-you-like".to_owned());
    let wrong_key =
        encode(&header, &claims, &EncodingKey::from_secret(OTHER_SECRET)).expect("encodes");
    assert_eq!(
        verifier().verify(&wrong_key).expect_err("wrong key"),
        AuthError::BadSignature,
        "a kid header let a token signed with the wrong key through"
    );
}

// -------------------------------------------------------------------------------------------
// Tampering and malformed input
// -------------------------------------------------------------------------------------------

/// Editing the claims invalidates the signature — including editing the scope list.
///
/// The attack this rules out is the direct one: take your own valid token, add the tenant you want.
#[test]
fn adding_a_scope_to_a_valid_token_invalidates_it() {
    let genuine = mint(Algorithm::HS256, SECRET, &valid_claims(&["inst:acme"]));
    let mut parts = genuine.split('.');
    let header = parts.next().expect("header");
    let signature = parts.nth(1).expect("signature");

    let b64 = base64::engine::general_purpose::URL_SAFE_NO_PAD;
    let greedy = b64
        .encode(serde_json::to_vec(&valid_claims(&["inst:acme", "inst:victim"])).expect("encodes"));
    let forged = format!("{header}.{greedy}.{signature}");

    assert_eq!(
        verifier().verify(&forged).expect_err("tampered"),
        AuthError::BadSignature,
        "a scope was added to a token without invalidating it"
    );
}

#[test]
fn a_token_claiming_no_scopes_is_refused() {
    let token = mint(
        Algorithm::HS256,
        SECRET,
        &json!({ "scopes": [], "exp": epoch(3600) }),
    );

    assert_eq!(
        verifier().verify(&token).expect_err("no scopes"),
        AuthError::NoScopes
    );
}

/// A claimed "scope" that is not a valid scope id never becomes one.
#[test]
fn a_claimed_scope_that_is_not_a_valid_scope_id_is_refused() {
    for bad in [
        "inst:acme; DROP TABLE sync_changes",
        "inst:acme\0",
        "inst:\u{1F600}",
        "",
    ] {
        let token = mint(
            Algorithm::HS256,
            SECRET,
            &json!({ "scopes": [bad], "exp": epoch(3600) }),
        );
        let err = verifier().verify(&token).expect_err("invalid scope claim");
        assert!(
            matches!(err, AuthError::Malformed { .. }),
            "claimed scope {bad:?} was not refused as malformed: {err:?}"
        );
    }
}

#[test]
fn structurally_broken_tokens_are_refused_without_panicking() {
    let genuine = mint(Algorithm::HS256, SECRET, &valid_claims(&["inst:acme"]));

    let mut cases: Vec<String> = vec![
        String::new(),
        ".".to_owned(),
        "..".to_owned(),
        "not-a-token".to_owned(),
        "a.b.c".to_owned(),
        format!("{genuine}."),
        format!(".{genuine}"),
        format!("{genuine}.{genuine}"),
        genuine.replace('.', ""),
        "Bearer ".to_owned() + &genuine,
    ];
    // Every single-byte truncation, which is the cheap fuzz that finds slicing bugs.
    for cut in 1..genuine.len() {
        cases.push(genuine[..cut].to_owned());
    }

    for case in cases {
        assert!(
            verifier().verify(&case).is_err(),
            "a malformed token was accepted: {case:?}"
        );
    }
}

// -------------------------------------------------------------------------------------------
// What the client is told
// -------------------------------------------------------------------------------------------

/// The client message must not distinguish the failures.
///
/// An error that says "bad signature" versus "scope not claimed" versus "blocked" is an oracle: it
/// tells somebody holding a forged token which scopes exist and whether an edit got closer. The
/// operator gets the detail in the log; the client gets one door.
#[test]
fn the_client_message_is_not_an_oracle() {
    let refusals = [
        AuthError::BadSignature,
        AuthError::WrongAlgorithm {
            found: "none".to_owned(),
        },
        AuthError::NoScopes,
        AuthError::ScopeNotClaimed {
            requested: "inst:victim".to_owned(),
        },
        AuthError::ScopeBlocked {
            scope: "inst:victim".to_owned(),
        },
        AuthError::Malformed {
            detail: "whatever".to_owned(),
        },
    ];

    let first = refusals[0].client_message();
    for r in &refusals {
        assert_eq!(
            r.client_message(),
            first,
            "{r:?} is distinguishable from the others by its client message"
        );
    }

    // Expiry is the deliberate exception: every honest client hits it, and the attacker already
    // holds the `exp` claim, so concealing it breaks correct clients to inconvenience nobody.
    assert_ne!(AuthError::Expired.client_message(), first);

    // And no message leaks the scope that was asked for.
    for r in &refusals {
        assert!(
            !r.client_message().contains("inst:"),
            "{r:?} leaks a scope name to the client"
        );
    }
}

/// The operator's log, by contrast, must distinguish them — that is what it is for.
#[test]
fn the_operator_message_does_distinguish_the_failures() {
    let refusals = [
        AuthError::BadSignature,
        AuthError::NoScopes,
        AuthError::ScopeNotClaimed {
            requested: "inst:victim".to_owned(),
        },
        AuthError::ScopeBlocked {
            scope: "inst:victim".to_owned(),
        },
        AuthError::Expired,
    ];

    let rendered: Vec<String> = refusals.iter().map(ToString::to_string).collect();
    let mut unique = rendered.clone();
    unique.sort();
    unique.dedup();
    assert_eq!(
        unique.len(),
        rendered.len(),
        "two refusals render identically for the operator: {rendered:?}"
    );
}

/// The signing key must never appear in a debug rendering.
#[test]
fn the_verifier_does_not_print_its_key() {
    let rendered = format!("{:?}", verifier());
    assert!(
        !rendered.contains("secret") && !rendered.contains("a-test-signing"),
        "the verifier printed something key-shaped: {rendered}"
    );
}

// -------------------------------------------------------------------------------------------
// Clock skew
// -------------------------------------------------------------------------------------------

/// Leeway is honoured, and is not infinite.
#[test]
fn leeway_covers_small_skew_but_not_a_stale_token() {
    let just_expired = mint(
        Algorithm::HS256,
        SECRET,
        &json!({ "scopes": ["inst:acme"], "exp": epoch(-30) }),
    );

    let strict = Verifier::new(DecodingKey::from_secret(SECRET), Algorithm::HS256, 0);
    assert_eq!(
        strict.verify(&just_expired).expect_err("no leeway"),
        AuthError::Expired
    );

    let tolerant = Verifier::new(DecodingKey::from_secret(SECRET), Algorithm::HS256, 120);
    assert!(
        tolerant.verify(&just_expired).is_ok(),
        "120s of leeway should cover 30s of skew"
    );

    let long_gone = mint(
        Algorithm::HS256,
        SECRET,
        &json!({ "scopes": ["inst:acme"], "exp": epoch(-86_400) }),
    );
    assert_eq!(
        tolerant.verify(&long_gone).expect_err("a day is not skew"),
        AuthError::Expired,
        "leeway must not become an unbounded grace period"
    );
}

// -------------------------------------------------------------------------------------------
// The canonical algorithm-confusion attack, against an asymmetric verifier
// -------------------------------------------------------------------------------------------

/// RS256 → HS256 confusion, the textbook JWT break.
///
/// The server verifies with an RSA **public** key, which is public — the attacker has it. If the
/// server reads `alg` from the token to decide how to verify, the attacker relabels the token
/// `HS256` and signs it with HMAC using those public key bytes as the shared secret. The server
/// dutifully verifies an HMAC against a key the attacker also holds, and it matches.
///
/// The defence is not "check the signature carefully"; the signature is genuinely valid. The
/// defence is refusing to let the token choose the algorithm at all.
///
/// This test uses a throwaway 2048-bit key generated for the repository. It signs nothing real and
/// is committed deliberately so the attack is reproducible — see `tests/keys/README.md`.
#[test]
fn an_hs256_token_signed_with_the_rsa_public_key_is_refused() {
    const PUBLIC_PEM: &[u8] = include_bytes!("keys/test-rsa-public.pem");
    const PRIVATE_PEM: &[u8] = include_bytes!("keys/test-rsa-private.pem");

    let rs256 = Verifier::new(
        DecodingKey::from_rsa_pem(PUBLIC_PEM).expect("valid public key"),
        Algorithm::RS256,
        0,
    );

    // A legitimate RS256 token verifies, so the test is not vacuous.
    let honest = encode(
        &Header::new(Algorithm::RS256),
        &valid_claims(&["inst:acme"]),
        &EncodingKey::from_rsa_pem(PRIVATE_PEM).expect("valid private key"),
    )
    .expect("encodes");
    rs256
        .verify(&honest)
        .expect("a genuine RS256 token must verify");

    // The attack: HS256, signed with the public key bytes the attacker already has.
    let forged = mint(
        Algorithm::HS256,
        PUBLIC_PEM,
        &valid_claims(&["inst:acme", "inst:victim"]),
    );

    let err = rs256
        .verify(&forged)
        .expect_err("RS256 -> HS256 algorithm confusion was not refused");
    assert!(
        matches!(err, AuthError::WrongAlgorithm { .. }),
        "the confusion was refused, but not by the algorithm pin: {err:?}"
    );
}

/// The pin holds for every algorithm the library knows, not just the ones a test remembered.
///
/// Signed correctly each time, so the only thing wrong with each token is its algorithm. A
/// verifier that checked signatures but not the pin would pass every one of these.
#[test]
fn a_pinned_verifier_refuses_every_other_hmac_algorithm() {
    for pinned in [Algorithm::HS256, Algorithm::HS384, Algorithm::HS512] {
        let verifier = Verifier::new(DecodingKey::from_secret(SECRET), pinned, 0);

        for signed_with in [Algorithm::HS256, Algorithm::HS384, Algorithm::HS512] {
            let token = mint(signed_with, SECRET, &valid_claims(&["inst:acme"]));
            let result = verifier.verify(&token);

            if signed_with == pinned {
                assert!(
                    result.is_ok(),
                    "a {pinned:?} verifier refused a genuine {signed_with:?} token"
                );
            } else {
                let err = result.expect_err("mismatched algorithm");
                assert!(
                    matches!(err, AuthError::WrongAlgorithm { .. }),
                    "a {pinned:?} verifier accepted a {signed_with:?} token, or refused it for \
                     the wrong reason: {err:?}"
                );
            }
        }
    }
}
