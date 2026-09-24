//! Reference credSync server: the router, and the authorization checks on its request path.
//!
//! A thin axum service around [`credsync_server`] that owns the wire protocol only: pull
//! pagination, command dedupe, result recording, scope-token validation and the scope blocklist.
//! Hosts that are themselves Rust may embed `credsync-server` as a library instead.
//!
//! The router lives here rather than in `main.rs` so the tests can drive it without binding a
//! port or starting a process. That is not tidiness: the property worth testing is that the
//! handler **calls** the authorization checks, and a test that can only reach this code through
//! a socket tends to become a test of the socket.
//!
//! # There is no domain logic here, and that is the point
//!
//! This binary knows nothing about reflections, deadlines, enrolments or institutions. It moves
//! opaque rows between a database and a client, and it forwards commands to the host, which is
//! where every domain judgement is made. Design v2.1 §4.2.
//!
//! The test for whether something belongs here: could it be written without knowing what the host's
//! application does? Pagination, dedupe and signature checking pass. "Is this student enrolled"
//! does not.
//!
//! # Authorization is three checks, in order
//!
//! 1. **Verify the token** — signature, pinned algorithm, expiry ([`credsync_server::auth`]).
//! 2. **Authorize the scope** — is it in the token's claims.
//! 3. **Consult the blocklist** — is it cut regardless ([`credsync_server::blocklist`]).
//!
//! The order matters: 1 and 2 are cheap and local, 3 costs a query. Doing the query first would let
//! anyone with a malformed token make this server hit its database.

#![forbid(unsafe_code)]

use axum::Router;
use axum::extract::{Query, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use credsync_protocol::ScopeId;
use credsync_server::auth::{AuthError, Claims, Verifier};
use credsync_server::{blocklist, db};
use serde::{Deserialize, Serialize};
use std::sync::Arc;

/// Everything a request handler needs.
#[derive(Clone)]
pub struct AppState {
    /// The database this server reads changes from.
    pub db: Arc<tokio_postgres::Client>,
    /// The one verifier, built once at start-up.
    pub verifier: Arc<Verifier>,
}

/// What a refusal looks like on the wire.
#[derive(Debug, Serialize)]
struct ErrorBody {
    error: &'static str,
}

/// Turns a refusal into a response.
///
/// Every authorization failure is a 401 carrying the same sentence, except expiry — see
/// [`AuthError::client_message`] for why distinguishing them would be an oracle. The operator's
/// detail goes to the log and nowhere near the client.
fn refuse(err: &AuthError) -> Response {
    // The log is where the detail belongs.
    eprintln!("refused: {err}");

    let status = if err.is_expired() {
        StatusCode::UNAUTHORIZED
    } else {
        StatusCode::FORBIDDEN
    };
    (
        status,
        axum::Json(ErrorBody {
            error: err.client_message(),
        }),
    )
        .into_response()
}

/// Pulls the bearer token out of the `Authorization` header.
fn bearer(headers: &HeaderMap) -> Result<&str, AuthError> {
    let raw = headers
        .get(header::AUTHORIZATION)
        .ok_or_else(|| AuthError::Malformed {
            detail: "no Authorization header".to_owned(),
        })?
        .to_str()
        .map_err(|_| AuthError::Malformed {
            detail: "Authorization header is not valid ASCII".to_owned(),
        })?;

    // RFC 7235 §2.1: `credentials = auth-scheme 1*SP token68`.
    //
    // Case-insensitive on the scheme, because the RFC says so and clients vary. One *or more*
    // spaces, likewise -- so `Bearer  x` is well-formed and accepted, while `Bearer\tx` is not,
    // because a tab is not SP. Being lenient about the separator costs nothing; being lenient
    // about what counts as the token would mean accepting whitespace inside it.
    let token = raw
        .split_once(' ')
        .filter(|(scheme, _)| scheme.eq_ignore_ascii_case("bearer"))
        .map(|(_, rest)| rest.trim_start_matches(' '))
        .ok_or_else(|| AuthError::Malformed {
            detail: "Authorization header is not a Bearer token".to_owned(),
        })?;

    if token.is_empty() {
        return Err(AuthError::Malformed {
            detail: "empty bearer token".to_owned(),
        });
    }
    // `token68` has no whitespace in it. Anything after a space is a second parameter, not part of
    // the credential, and quietly treating it as one would accept `Bearer <mine> <theirs>`.
    if token.contains(char::is_whitespace) {
        return Err(AuthError::Malformed {
            detail: "bearer token contains whitespace".to_owned(),
        });
    }
    Ok(token)
}

/// The three checks, in the order the module docs describe.
///
/// Returns the validated claims so a handler cannot accidentally proceed without them — the type
/// is the proof that authorization ran.
async fn authorize(
    state: &AppState,
    headers: &HeaderMap,
    scope: &ScopeId,
) -> Result<Claims, AuthError> {
    let token = bearer(headers)?;
    let claims = state.verifier.verify(token)?;
    claims.authorize(scope)?;
    blocklist::check(&state.db, scope).await?;
    Ok(claims)
}

/// `GET /v1/pull?scope=…&cursor=…`
#[derive(Debug, Deserialize)]
struct PullParams {
    scope: String,
    #[serde(default)]
    cursor: u64,
}

async fn pull(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(params): Query<PullParams>,
) -> Response {
    let Ok(scope) = ScopeId::new(params.scope) else {
        // A malformed scope is refused exactly like an unauthorized one. Saying "that is not a
        // valid scope id" would confirm which strings are worth trying.
        return refuse(&AuthError::ScopeNotClaimed {
            requested: "<invalid>".to_owned(),
        });
    };

    if let Err(e) = authorize(&state, &headers, &scope).await {
        return refuse(&e);
    }

    match db::changes_after(&state.db, &scope, params.cursor, 500).await {
        Ok(page) => axum::Json(serde_json::json!({
            "changes": page.changes,
            "has_more": page.more_beyond,
        }))
        .into_response(),
        Err(e) => {
            eprintln!("pull failed: {e}");
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                axum::Json(ErrorBody {
                    error: "Sync is temporarily unavailable.",
                }),
            )
                .into_response()
        }
    }
}

/// `GET /healthz` — liveness only, and deliberately unauthenticated.
///
/// It reports whether this process is up, not whether the database is. A health check that fails
/// when the database blips takes every instance out of rotation at the moment they are most needed.
async fn healthz() -> &'static str {
    "ok"
}

/// Builds the router.
///
/// Separate from `main` so the tests can drive it in-process. Every route that touches scoped data
/// must run `authorize` before it reads anything; the tests assert that by attacking the routes
/// rather than by reading the code.
pub fn app(state: AppState) -> Router {
    Router::new()
        .route("/healthz", get(healthz))
        .route("/v1/pull", get(pull))
        .with_state(state)
}
