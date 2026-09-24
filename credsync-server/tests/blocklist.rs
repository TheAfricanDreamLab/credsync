//! CS-17: the scope blocklist and tenant isolation, against a real Postgres.
//!
//! The blocklist exists for the cases where waiting out a token's TTL is not an answer — a
//! withdrawn student, a dismissed staff member, a stolen device. So the test that matters is not
//! "a blocked scope is blocked" but **"a blocked scope is cut while holding a perfectly valid,
//! unexpired token"**, which is the situation it was built for.

// This file needs the `postgres` feature: it drives a real database. With the feature off (which
// is how `credsync-sim` depends on this crate) it compiles to an empty test binary rather than a
// build failure.
#![cfg(feature = "postgres")]
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use credsync_protocol::ScopeId;
use credsync_server::auth::{AuthError, Verifier};
use credsync_server::{blocklist, db};
use jsonwebtoken::{Algorithm, DecodingKey, EncodingKey, Header, encode};
use serde_json::json;
use tokio_postgres::{Client, NoTls};

const SECRET: &[u8] = b"a-test-signing-secret-that-is-long-enough";

fn url() -> String {
    std::env::var("CREDSYNC_TEST_DATABASE_URL").unwrap_or_else(|_| {
        panic!(
            "CREDSYNC_TEST_DATABASE_URL is not set.\n\
             Start one with `eval \"$(./scripts/test-postgres.sh)\"`."
        )
    })
}

async fn connect() -> Client {
    let (client, connection) = tokio_postgres::connect(&url(), NoTls)
        .await
        .expect("connects");
    tokio::spawn(async move {
        let _ = connection.await;
    });
    client
}

async fn fresh() -> Client {
    let client = connect().await;
    db::migrate(&client).await.expect("migrates");
    client
}

/// Unique per test and per run, so concurrent tests never see each other's rows.
fn unique_scope(test: &str) -> ScopeId {
    use std::sync::OnceLock;
    static RUN: OnceLock<u128> = OnceLock::new();
    let run = RUN.get_or_init(|| {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_nanos())
    });
    ScopeId::new(format!("inst:{test}:{run}")).expect("valid scope")
}

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

/// A genuine, unexpired token for the given scopes.
fn token_for(scopes: &[&ScopeId]) -> String {
    let names: Vec<&str> = scopes.iter().map(|s| s.as_str()).collect();
    encode(
        &Header::new(Algorithm::HS256),
        &json!({ "scopes": names, "exp": epoch(3600) }),
        &EncodingKey::from_secret(SECRET),
    )
    .expect("encodes")
}

fn verifier() -> Verifier {
    Verifier::new(DecodingKey::from_secret(SECRET), Algorithm::HS256, 0)
}

async fn append(client: &Client, scope: &ScopeId, entity_id: &str) {
    let snapshot = json!({ "body": "some work" });
    db::append_change(
        client,
        &db::NewChange {
            scope: scope.as_str(),
            entity: "reflections",
            entity_id,
            op: "upsert",
            snapshot: Some(&snapshot),
            row_version: 1,
            schema_version: 1,
        },
    )
    .await
    .expect("appends");
}

// -------------------------------------------------------------------------------------------
// DoD: a blocklisted scope is cut immediately, before token expiry
// -------------------------------------------------------------------------------------------

/// The whole point: a valid unexpired token stops working the moment the scope is blocked.
#[tokio::test]
async fn a_blocked_scope_is_cut_while_its_token_is_still_valid() {
    let client = fresh().await;
    let scope = unique_scope("cut_immediately");
    let token = token_for(&[&scope]);

    // The token is and remains valid throughout — nothing here touches expiry.
    let claims = verifier().verify(&token).expect("verifies");
    claims.authorize(&scope).expect("claimed");
    blocklist::check(&client, &scope)
        .await
        .expect("not blocked yet");

    blocklist::block(&client, &scope, "withdrawn 14 September")
        .await
        .expect("blocks");

    // Same token, same claims, still unexpired.
    let claims = verifier().verify(&token).expect("still verifies");
    claims
        .authorize(&scope)
        .expect("the token still claims the scope");
    let err = blocklist::check(&client, &scope)
        .await
        .expect_err("a blocked scope must be refused");
    assert!(matches!(err, AuthError::ScopeBlocked { .. }), "{err:?}");
}

/// Blocking one scope does not touch another, including one in the same token.
#[tokio::test]
async fn blocking_one_scope_leaves_the_others_alone() {
    let client = fresh().await;
    let blocked = unique_scope("multi_blocked");
    let allowed = unique_scope("multi_allowed");

    blocklist::block(&client, &blocked, "stolen device")
        .await
        .expect("blocks");

    assert!(blocklist::check(&client, &blocked).await.is_err());
    blocklist::check(&client, &allowed)
        .await
        .expect("an unrelated scope must be unaffected");
}

#[tokio::test]
async fn blocking_is_idempotent_and_reversible() {
    let client = fresh().await;
    let scope = unique_scope("idempotent");

    blocklist::block(&client, &scope, "first")
        .await
        .expect("blocks");
    blocklist::block(&client, &scope, "again")
        .await
        .expect("blocks again");
    assert!(blocklist::check(&client, &scope).await.is_err());

    blocklist::unblock(&client, &scope).await.expect("unblocks");
    blocklist::check(&client, &scope).await.expect("restored");

    blocklist::unblock(&client, &scope)
        .await
        .expect("unblocking twice is not an error");
}

/// The blocklist check fails closed.
///
/// If the table cannot be read, the request is refused rather than allowed. An availability problem
/// here must not become an authorization bypass — serving a withdrawn student's data because a
/// query failed is precisely what this table exists to prevent.
#[tokio::test]
async fn an_unreadable_blocklist_refuses_rather_than_allows() {
    let client = connect().await;
    let scope = unique_scope("fail_closed");

    // A session whose connection is gone cannot answer the question.
    drop(client);
    let dead = connect().await;
    dead.batch_execute("SELECT pg_terminate_backend(pg_backend_pid())")
        .await
        .ok();

    let err = blocklist::check(&dead, &scope)
        .await
        .expect_err("an unanswerable blocklist check must refuse");
    assert!(
        matches!(err, AuthError::ScopeBlocked { .. }),
        "failing closed must look like a block, not like an allow: {err:?}"
    );
}

/// The block reason is for the operator and never reaches the client.
///
/// "Blocked because: withdrawn 14 September" tells the wrong person something true.
#[tokio::test]
async fn the_block_reason_never_reaches_the_client() {
    let client = fresh().await;
    let scope = unique_scope("reason_stays_private");
    blocklist::block(&client, &scope, "withdrawn after disciplinary panel")
        .await
        .expect("blocks");

    let err = blocklist::check(&client, &scope)
        .await
        .expect_err("blocked");
    let message = err.client_message();
    assert!(
        !message.contains("withdrawn") && !message.contains("disciplinary"),
        "the block reason leaked to the client: {message}"
    );
    assert!(
        !message.contains(scope.as_str()),
        "the scope name leaked to the client: {message}"
    );
}

// -------------------------------------------------------------------------------------------
// DoD: tenant isolation — a token for tenant A returns zero rows for tenant B
// -------------------------------------------------------------------------------------------

/// A token for tenant A cannot read tenant B, and A's rows are real so the test is not vacuous.
#[tokio::test]
async fn a_token_for_one_tenant_returns_nothing_for_another() {
    let client = fresh().await;
    let tenant_a = unique_scope("tenant_a");
    let tenant_b = unique_scope("tenant_b");

    for n in 0..5 {
        append(&client, &tenant_a, &format!("a{n}")).await;
        append(&client, &tenant_b, &format!("b{n}")).await;
    }

    let claims = verifier()
        .verify(&token_for(&[&tenant_a]))
        .expect("verifies");

    // A reaches its own rows.
    claims
        .authorize(&tenant_a)
        .expect("authorized for its own tenant");

    // And is refused for B, before any query runs.
    assert!(
        claims.authorize(&tenant_b).is_err(),
        "a token for tenant A was authorized for tenant B"
    );

    // Even if the authorization check were bypassed entirely, the query cannot cross: this is the
    // second half of the guarantee, and the reason the scope filter is not merely a convenience.
    let leaked = db::changes_after(&client, &tenant_b, 0, 100)
        .await
        .expect("reads")
        .changes;
    for change in &leaked {
        assert!(
            change.entity_id.as_str().starts_with('b'),
            "tenant B's page contained {}, which belongs to tenant A",
            change.entity_id.as_str()
        );
    }
}

/// Pull cannot cross scopes whatever cursor the client sends — adversarially, with real neighbours.
///
/// The client controls `cursor` completely. `seq` is shared by every scope in one `bigserial`, so
/// a cursor taken from tenant B's rows sits right in the middle of tenant A's range. If the scope
/// filter were ever dropped, this is the input that would expose it.
#[tokio::test]
async fn pull_cannot_cross_scopes_for_any_cursor_the_client_chooses() {
    let client = fresh().await;
    let mine = unique_scope("adversarial_mine");
    let theirs = unique_scope("adversarial_theirs");

    // Interleaved, so the two scopes' seqs are genuinely intermingled rather than in two blocks.
    for n in 0..6 {
        append(&client, &mine, &format!("m{n}")).await;
        append(&client, &theirs, &format!("t{n}")).await;
    }

    let their_page = db::changes_after(&client, &theirs, 0, 100)
        .await
        .expect("reads")
        .changes;
    assert!(
        !their_page.is_empty(),
        "the neighbour must have rows to leak"
    );

    let mut cursors: Vec<u64> = vec![0, 1, u64::from(u32::MAX), i64::MAX as u64];
    cursors.extend(their_page.iter().map(|c| c.seq.get()));
    cursors.extend(their_page.iter().map(|c| c.seq.get().saturating_sub(1)));

    for cursor in cursors {
        let page = db::changes_after(&client, &mine, cursor, 100)
            .await
            .expect("reads");
        for change in &page.changes {
            assert!(
                change.entity_id.as_str().starts_with('m'),
                "cursor {cursor} leaked {} from the neighbouring scope",
                change.entity_id.as_str()
            );
        }
    }
}
