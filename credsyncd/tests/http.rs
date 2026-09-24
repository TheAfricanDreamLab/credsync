//! CS-17: the authorization checks as they actually sit on the request path.
//!
//! The unit tests in `credsync-server` prove the token verifier and the blocklist are correct in
//! isolation. They cannot prove the handler *calls* them — and a `pull` route that forgot to
//! authorize would pass every one of those tests while serving any scope to anyone.
//!
//! So these drive the real router, with real HTTP requests, against a real database. Each one
//! writes rows into a scope and then tries to read them with a token that should not work.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use axum::body::Body;
use axum::http::{Request, StatusCode};
use credsync_protocol::ScopeId;
use credsync_server::auth::Verifier;
use credsync_server::{blocklist, db};
use credsyncd::{AppState, app};
use jsonwebtoken::{Algorithm, DecodingKey, EncodingKey, Header, encode};
use serde_json::json;
use std::sync::Arc;
use tokio_postgres::{Client, NoTls};
use tower::ServiceExt as _;

const SECRET: &[u8] = b"a-test-signing-secret-that-is-long-enough";
const OTHER_SECRET: &[u8] = b"a-completely-different-signing-secret!!!!!";

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

async fn state() -> AppState {
    let client = connect().await;
    db::migrate(&client).await.expect("migrates");
    AppState {
        db: Arc::new(client),
        verifier: Arc::new(Verifier::new(
            DecodingKey::from_secret(SECRET),
            Algorithm::HS256,
            0,
        )),
    }
}

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

fn token(secret: &[u8], scopes: &[&str], exp_delta: i64) -> String {
    encode(
        &Header::new(Algorithm::HS256),
        &json!({ "scopes": scopes, "exp": epoch(exp_delta) }),
        &EncodingKey::from_secret(secret),
    )
    .expect("encodes")
}

async fn seed(client: &Client, scope: &ScopeId, n: usize) {
    for i in 0..n {
        let snapshot = json!({ "body": "confidential coursework" });
        db::append_change(
            client,
            &db::NewChange {
                scope: scope.as_str(),
                entity: "reflections",
                entity_id: &format!("r{i}"),
                op: "upsert",
                snapshot: Some(&snapshot),
                row_version: 1,
                schema_version: 1,
            },
        )
        .await
        .expect("appends");
    }
}

/// Sends one request through the real router.
async fn get(state: &AppState, uri: &str, auth: Option<&str>) -> (StatusCode, String) {
    let mut req = Request::builder().uri(uri).method("GET");
    if let Some(a) = auth {
        req = req.header("Authorization", a);
    }
    let response = app(state.clone())
        .oneshot(req.body(Body::empty()).expect("builds"))
        .await
        .expect("responds");

    let status = response.status();
    let bytes = axum::body::to_bytes(response.into_body(), 1 << 20)
        .await
        .expect("reads body");
    (status, String::from_utf8_lossy(&bytes).into_owned())
}

// -------------------------------------------------------------------------------------------
// The route really does authorize
// -------------------------------------------------------------------------------------------

#[tokio::test]
async fn a_valid_token_reaches_its_own_scope() {
    let state = state().await;
    let scope = unique_scope("valid_reaches_own");
    seed(&state.db, &scope, 3).await;

    let (status, body) = get(
        &state,
        &format!("/v1/pull?scope={}&cursor=0", scope.as_str()),
        Some(&format!(
            "Bearer {}",
            token(SECRET, &[scope.as_str()], 3600)
        )),
    )
    .await;

    assert_eq!(status, StatusCode::OK, "body: {body}");
    assert!(
        body.contains("changes"),
        "a valid pull should return a change list: {body}"
    );
}

/// No token at all.
#[tokio::test]
async fn pull_without_a_token_is_refused() {
    let state = state().await;
    let scope = unique_scope("no_token");
    seed(&state.db, &scope, 3).await;

    let (status, body) = get(
        &state,
        &format!("/v1/pull?scope={}&cursor=0", scope.as_str()),
        None,
    )
    .await;

    assert_eq!(status, StatusCode::FORBIDDEN);
    assert!(
        !body.contains("confidential"),
        "an unauthenticated pull returned data: {body}"
    );
}

/// The core tenant-isolation test, end to end: A's token, B's scope.
#[tokio::test]
async fn a_token_for_one_tenant_cannot_pull_another() {
    let state = state().await;
    let mine = unique_scope("http_mine");
    let theirs = unique_scope("http_theirs");
    seed(&state.db, &theirs, 5).await;

    let (status, body) = get(
        &state,
        &format!("/v1/pull?scope={}&cursor=0", theirs.as_str()),
        Some(&format!("Bearer {}", token(SECRET, &[mine.as_str()], 3600))),
    )
    .await;

    assert_eq!(status, StatusCode::FORBIDDEN, "body: {body}");
    assert!(
        !body.contains("confidential"),
        "a cross-tenant pull returned data: {body}"
    );
    assert!(
        !body.contains(theirs.as_str()),
        "the refusal echoed the scope back: {body}"
    );
}

#[tokio::test]
async fn a_token_signed_with_the_wrong_key_is_refused_over_http() {
    let state = state().await;
    let scope = unique_scope("http_wrong_key");
    seed(&state.db, &scope, 3).await;

    let (status, body) = get(
        &state,
        &format!("/v1/pull?scope={}&cursor=0", scope.as_str()),
        Some(&format!(
            "Bearer {}",
            token(OTHER_SECRET, &[scope.as_str()], 3600)
        )),
    )
    .await;

    assert_eq!(status, StatusCode::FORBIDDEN);
    assert!(!body.contains("confidential"), "body: {body}");
}

#[tokio::test]
async fn an_expired_token_is_refused_over_http_and_says_so() {
    let state = state().await;
    let scope = unique_scope("http_expired");
    seed(&state.db, &scope, 3).await;

    let (status, body) = get(
        &state,
        &format!("/v1/pull?scope={}&cursor=0", scope.as_str()),
        Some(&format!("Bearer {}", token(SECRET, &[scope.as_str()], -60))),
    )
    .await;

    // 401 rather than 403: this is the one refusal a correct client must be able to act on, by
    // fetching a new token.
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    assert!(!body.contains("confidential"), "body: {body}");
}

/// A blocked scope is cut over HTTP while its token is still perfectly valid.
#[tokio::test]
async fn a_blocked_scope_is_cut_over_http_before_expiry() {
    let state = state().await;
    let scope = unique_scope("http_blocked");
    seed(&state.db, &scope, 3).await;

    let auth = format!("Bearer {}", token(SECRET, &[scope.as_str()], 3600));
    let uri = format!("/v1/pull?scope={}&cursor=0", scope.as_str());

    let (status, _) = get(&state, &uri, Some(&auth)).await;
    assert_eq!(status, StatusCode::OK, "it should work before the block");

    blocklist::block(&state.db, &scope, "device reported stolen")
        .await
        .expect("blocks");

    let (status, body) = get(&state, &uri, Some(&auth)).await;
    assert_eq!(
        status,
        StatusCode::FORBIDDEN,
        "the same token still worked after the scope was blocked"
    );
    assert!(!body.contains("confidential"), "body: {body}");
    assert!(
        !body.contains("stolen"),
        "the block reason leaked to the client: {body}"
    );
}

// -------------------------------------------------------------------------------------------
// Header handling, adversarially
// -------------------------------------------------------------------------------------------

#[tokio::test]
async fn malformed_authorization_headers_are_refused() {
    let state = state().await;
    let scope = unique_scope("http_bad_headers");
    seed(&state.db, &scope, 3).await;
    let good = token(SECRET, &[scope.as_str()], 3600);
    let uri = format!("/v1/pull?scope={}&cursor=0", scope.as_str());

    let cases = [
        String::new(),
        "Bearer".to_owned(),
        "Bearer ".to_owned(),
        good.clone(),                    // no scheme
        format!("Basic {good}"),         // wrong scheme
        format!("Bearer {good} {good}"), // two tokens
        format!("Bearer\t{good}"),       // tab is not SP
        format!("Bearer {good} extra"),  // a trailing parameter is not part of the token
    ];

    for case in cases {
        let (status, body) = get(&state, &uri, Some(&case)).await;
        assert_ne!(
            status,
            StatusCode::OK,
            "Authorization header {case:?} was accepted"
        );
        assert!(
            !body.contains("confidential"),
            "header {case:?} returned data: {body}"
        );
    }
}

/// The scheme is matched case-insensitively and `1*SP` is honoured, per RFC 7235 §2.1.
///
/// `Bearer  x` with two spaces is **well-formed** — the grammar says one or more — so refusing it
/// would reject a conforming client. The first draft of this test asserted the opposite, which is
/// how the parser came to be checked against the grammar rather than against an assumption.
#[tokio::test]
async fn the_bearer_scheme_is_case_insensitive() {
    let state = state().await;
    let scope = unique_scope("http_scheme_case");
    seed(&state.db, &scope, 1).await;
    let good = token(SECRET, &[scope.as_str()], 3600);
    let uri = format!("/v1/pull?scope={}&cursor=0", scope.as_str());

    for scheme in ["Bearer", "bearer", "BEARER", "BeArEr"] {
        let (status, body) = get(&state, &uri, Some(&format!("{scheme} {good}"))).await;
        assert_eq!(status, StatusCode::OK, "scheme {scheme:?} rejected: {body}");
    }

    // `1*SP`: more than one space is still a conforming request.
    let (status, body) = get(&state, &uri, Some(&format!("Bearer   {good}"))).await;
    assert_eq!(status, StatusCode::OK, "1*SP not honoured: {body}");
}

/// A malformed scope is refused exactly like an unauthorized one.
///
/// Answering "that is not a valid scope id" would confirm which strings are worth trying.
#[tokio::test]
async fn an_invalid_scope_is_refused_indistinguishably() {
    let state = state().await;
    let scope = unique_scope("http_bad_scope");
    let auth = format!("Bearer {}", token(SECRET, &[scope.as_str()], 3600));

    let (bad_status, bad_body) = get(
        &state,
        "/v1/pull?scope=not%20a%20scope&cursor=0",
        Some(&auth),
    )
    .await;
    let (unauth_status, unauth_body) = get(
        &state,
        "/v1/pull?scope=inst:somebody-else&cursor=0",
        Some(&auth),
    )
    .await;

    assert_eq!(bad_status, unauth_status);
    assert_eq!(
        bad_body, unauth_body,
        "an invalid scope is distinguishable from an unauthorized one"
    );
}

/// Health is unauthenticated on purpose, and says nothing about the database.
#[tokio::test]
async fn health_is_open_and_says_nothing_useful() {
    let state = state().await;
    let (status, body) = get(&state, "/healthz", None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body, "ok");
}
