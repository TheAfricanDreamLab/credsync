//! Reference credSync server binary.
//!
//! Start-up only: read the configuration, connect, migrate, serve. Everything the requests actually
//! touch lives in the library beside this file, so it can be tested without a socket.

#![forbid(unsafe_code)]

use credsync_server::auth::Verifier;
use credsync_server::db;
use credsyncd::{AppState, app};
use jsonwebtoken::{Algorithm, DecodingKey};
use std::sync::Arc;
use tokio_postgres::NoTls;

/// Reads configuration from the environment, refusing to start on anything missing.
///
/// No defaults for the signing key. A server that falls back to a built-in secret is a server that
/// someone will deploy without noticing, and the whole authorization boundary would be decorative.
fn config() -> Result<(String, Vec<u8>, String), String> {
    let database_url = std::env::var("CREDSYNC_DATABASE_URL")
        .map_err(|_| "CREDSYNC_DATABASE_URL is not set".to_owned())?;

    let secret = std::env::var("CREDSYNC_TOKEN_SECRET")
        .map_err(|_| "CREDSYNC_TOKEN_SECRET is not set".to_owned())?;
    if secret.len() < 32 {
        return Err("CREDSYNC_TOKEN_SECRET must be at least 32 bytes".to_owned());
    }

    let bind = std::env::var("CREDSYNC_BIND").unwrap_or_else(|_| "127.0.0.1:8080".to_owned());
    Ok((database_url, secret.into_bytes(), bind))
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let (database_url, secret, bind) = config().map_err(|e| -> Box<dyn std::error::Error> {
        eprintln!("credsyncd: {e}");
        e.into()
    })?;

    let (client, connection) = tokio_postgres::connect(&database_url, NoTls).await?;
    tokio::spawn(async move {
        if let Err(e) = connection.await {
            eprintln!("database connection lost: {e}");
        }
    });

    db::migrate(&client).await?;

    let state = AppState {
        db: Arc::new(client),
        verifier: Arc::new(Verifier::new(
            DecodingKey::from_secret(&secret),
            Algorithm::HS256,
            // 60 seconds of clock skew between the host minting tokens and this server. Small
            // enough that a revoked token is not usable for long, large enough that two machines
            // with ordinary NTP drift agree.
            60,
        )),
    };

    let listener = tokio::net::TcpListener::bind(&bind).await?;
    eprintln!("credsyncd listening on {bind}");
    axum::serve(listener, app(state)).await?;
    Ok(())
}
