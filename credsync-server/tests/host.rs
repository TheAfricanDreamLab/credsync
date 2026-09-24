//! CS-16: forwarding commands to the host, and the failure paths that matter more than the happy
//! one.
//!
//! The reference host below is in-process and deliberately hostile on request: it can time out,
//! return a 5xx, refuse the connection, or apply the command and *then* fail to answer. That last
//! one is the case the whole design turns on, and it is the one a naive implementation gets wrong.

// This file needs the `postgres` feature: it drives a real database. With the feature off (which
// is how `credsync-sim` depends on this crate) it compiles to an empty test binary rather than a
// build failure.
#![cfg(feature = "postgres")]
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use credsync_protocol::{
    Command, CommandId, CommandName, HexString, Payload, Reason, ScopeId, Seq, Status,
};
use credsync_server::host::{Forwarded, Host, HostError, HostOutcome, forward};
use credsync_server::{db, dedupe};
use std::cell::Cell;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use tokio_postgres::{Client, NoTls};

// -------------------------------------------------------------------------------------------
// A reference host
// -------------------------------------------------------------------------------------------

/// What the reference host should do when asked.
#[derive(Debug, Clone)]
enum Behaviour {
    Apply,
    Reject(String),
    Supersede,
    Timeout,
    ServerError(u16),
    Unreachable,
    /// Apply the command — writing the change-log row — and then fail to answer.
    ///
    /// The case the whole design turns on. The write happened; the server does not know it. Any
    /// implementation that records a verdict here is either claiming a write that may not exist or
    /// denying one that does.
    ApplyThenVanish,
}

/// An in-process host that writes to the same database the server reads.
///
/// In production this is the host's own HTTP endpoint applying its own domain rules. Here it does
/// the one thing credSync actually depends on — writing a change-log row in its own transaction —
/// and nothing else, because everything else is domain logic that must not live in this repo.
struct ReferenceHost {
    url: String,
    scope: ScopeId,
    behaviour: Behaviour,
    /// How many times the host was actually asked.
    ///
    /// The assertion that matters for replays: a deduped command must not reach the host at all.
    calls: Arc<AtomicU64>,
}

impl ReferenceHost {
    fn new(url: &str, scope: &ScopeId, behaviour: Behaviour) -> Self {
        Self {
            url: url.to_owned(),
            scope: scope.clone(),
            behaviour,
            calls: Arc::new(AtomicU64::new(0)),
        }
    }

    /// Writes the change-log row, as a real host would inside its own transaction.
    async fn write_change(&self) -> Seq {
        let (client, connection) = tokio_postgres::connect(&self.url, NoTls)
            .await
            .expect("host connects");
        tokio::spawn(async move {
            let _ = connection.await;
        });

        let snapshot = serde_json::json!({ "body": "applied by the host" });
        let seq = db::append_change(
            &client,
            &db::NewChange {
                scope: self.scope.as_str(),
                entity: "reflections",
                entity_id: "r1",
                op: "upsert",
                snapshot: Some(&snapshot),
                row_version: 1,
                schema_version: 1,
            },
        )
        .await
        .expect("host appends");

        Seq::new(u64::try_from(seq).expect("positive")).expect("valid seq")
    }
}

impl Host for ReferenceHost {
    async fn apply(&self, _command: &Command) -> Result<HostOutcome, HostError> {
        self.calls.fetch_add(1, Ordering::SeqCst);

        match &self.behaviour {
            Behaviour::Apply => Ok(HostOutcome::Applied {
                server_seq: self.write_change().await,
            }),
            Behaviour::Reject(why) => Ok(HostOutcome::Rejected {
                reason: Reason::new(why.clone()).expect("valid reason"),
            }),
            Behaviour::Supersede => Ok(HostOutcome::Superseded),
            Behaviour::Timeout => Err(HostError::Timeout),
            Behaviour::ServerError(code) => Err(HostError::Status { code: *code }),
            Behaviour::Unreachable => Err(HostError::Unreachable {
                detail: "connection refused".to_owned(),
            }),
            Behaviour::ApplyThenVanish => {
                // The write lands...
                let _ = self.write_change().await;
                // ...and the answer never arrives.
                Err(HostError::Timeout)
            }
        }
    }
}

// -------------------------------------------------------------------------------------------
// Fixtures
// -------------------------------------------------------------------------------------------

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

/// Unique per test and per run — see `pull.rs` for why both halves are needed.
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

fn command_for(test: &str, scope: &ScopeId, body: &str) -> Command {
    thread_local! {
        static N: Cell<u8> = const { Cell::new(0) };
    }
    let n = N.with(|c| {
        let v = c.get().wrapping_add(1);
        c.set(v);
        v
    });

    let mut bytes = [0u8; 16];
    bytes[0] = 0x01;
    bytes[6] = 0x70;
    bytes[8] = u8::try_from(test.len() % 251).unwrap_or(0);
    bytes[9] = test.bytes().fold(0u8, |a, b| a.wrapping_add(b));
    let run = std::process::id().to_be_bytes();
    bytes[10..14].copy_from_slice(&run);
    bytes[15] = n;

    Command {
        id: CommandId::from_bytes(bytes).expect("version nibble is 7"),
        name: CommandName::new("submit_reflection").expect("valid name"),
        scope: scope.clone(),
        payload: Payload::new(serde_json::json!({ "body": body })).expect("valid payload"),
        client_ts: 1_756_137_600_000,
        checksum: HexString::new("00000000000000000000000000000000").expect("valid hex"),
    }
}

// -------------------------------------------------------------------------------------------
// The three outcome paths
// -------------------------------------------------------------------------------------------

#[tokio::test]
async fn an_applied_command_is_recorded_with_its_server_seq() {
    let client = fresh().await;
    let scope = unique_scope("applied");
    let host = ReferenceHost::new(&url(), &scope, Behaviour::Apply);
    let cmd = command_for("applied", &scope, "body");

    let out = forward(&client, &host, &cmd).await.expect("forwards");

    let Forwarded::Decided(result) = &out else {
        panic!("expected a decision, got {out:?}");
    };
    assert_eq!(result.status, Status::Applied);
    assert!(
        result.server_seq.is_some(),
        "an applied command records where it landed"
    );

    // And it is durable: a new session sees it.
    let after = connect().await;
    assert!(matches!(
        dedupe::classify(&after, &cmd).await.expect("classifies"),
        dedupe::Decision::Replay(_)
    ));
}

/// A rejection carries the host's own words through to the client, unchanged.
///
/// `docs/spec.md` §3.3 surfaces this to the user. A server that rewrote it would be putting words
/// in the host's mouth about the host's own domain rules.
#[tokio::test]
async fn a_rejection_carries_the_hosts_reason_through() {
    let client = fresh().await;
    let scope = unique_scope("rejected");
    let host = ReferenceHost::new(
        &url(),
        &scope,
        Behaviour::Reject("Submission deadline passed on 14 September.".to_owned()),
    );
    let cmd = command_for("rejected", &scope, "body");

    let out = forward(&client, &host, &cmd).await.expect("forwards");
    let Forwarded::Decided(result) = &out else {
        panic!("expected a decision, got {out:?}");
    };

    assert_eq!(result.status, Status::Rejected);
    assert_eq!(
        result
            .reason
            .as_ref()
            .expect("a rejection carries a reason")
            .as_str(),
        "Submission deadline passed on 14 September.",
        "the host's wording must reach the client unchanged"
    );

    // And it survives to the replay, so the user can still be told why.
    match dedupe::classify(&client, &cmd).await.expect("classifies") {
        dedupe::Decision::Replay(r) => assert_eq!(
            r.reason.expect("reason survives").as_str(),
            "Submission deadline passed on 14 September."
        ),
        other => panic!("expected a replay, got {other:?}"),
    }
}

#[tokio::test]
async fn a_superseded_command_is_recorded_as_superseded() {
    let client = fresh().await;
    let scope = unique_scope("superseded");
    let host = ReferenceHost::new(&url(), &scope, Behaviour::Supersede);
    let cmd = command_for("superseded", &scope, "body");

    let out = forward(&client, &host, &cmd).await.expect("forwards");
    let Forwarded::Decided(result) = &out else {
        panic!("expected a decision, got {out:?}");
    };
    assert_eq!(result.status, Status::Superseded);
    assert!(result.server_seq.is_none());
}

// -------------------------------------------------------------------------------------------
// The failure paths — a failure to reach the host is not a verdict
// -------------------------------------------------------------------------------------------

/// Timeout, 5xx and unreachable all leave the command unresolved and record nothing.
///
/// The rule the module exists to enforce. Recording `rejected` on a timeout would tell a student
/// their work was refused when it may well have saved; recording `applied` would claim a write
/// that may never have happened. Neither is knowable, so neither is written.
#[tokio::test]
async fn host_failures_record_nothing_and_leave_the_command_unresolved() {
    for (label, behaviour) in [
        ("timeout", Behaviour::Timeout),
        ("5xx", Behaviour::ServerError(503)),
        ("unreachable", Behaviour::Unreachable),
    ] {
        let client = fresh().await;
        let scope = unique_scope(&format!("fail_{label}"));
        let host = ReferenceHost::new(&url(), &scope, behaviour);
        let cmd = command_for(&format!("fail_{label}"), &scope, "body");

        let out = forward(&client, &host, &cmd).await.expect("forwards");

        assert!(
            matches!(out, Forwarded::Unresolved { .. }),
            "{label}: expected Unresolved, got {out:?}"
        );
        assert!(
            out.result().is_none(),
            "{label}: an unresolved command has no result to send"
        );
        assert!(
            matches!(
                dedupe::classify(&client, &cmd).await.expect("classifies"),
                dedupe::Decision::Fresh
            ),
            "{label}: a host failure was recorded as an outcome"
        );
    }
}

/// A retry after a host failure reaches the host again and can then succeed.
///
/// The consequence of recording nothing: the command is still live, so the client's next push
/// resolves it properly rather than being told a maybe was a no.
#[tokio::test]
async fn a_command_unresolved_by_a_host_failure_can_be_retried() {
    let client = fresh().await;
    let scope = unique_scope("retry_after_failure");
    let cmd = command_for("retry_after_failure", &scope, "body");

    let failing = ReferenceHost::new(&url(), &scope, Behaviour::Timeout);
    let first = forward(&client, &failing, &cmd).await.expect("forwards");
    assert!(matches!(first, Forwarded::Unresolved { .. }));

    let working = ReferenceHost::new(&url(), &scope, Behaviour::Apply);
    let second = forward(&client, &working, &cmd).await.expect("forwards");

    let Forwarded::Decided(result) = &second else {
        panic!("the retry should have been decided, got {second:?}");
    };
    assert_eq!(result.status, Status::Applied);
    assert_eq!(
        working.calls.load(Ordering::SeqCst),
        1,
        "the retry must reach the host, since nothing was recorded the first time"
    );
}

/// The host applies the command and then fails to answer — and the retry does not apply it twice.
///
/// The case the whole design turns on. The write landed and the server never heard, so the command
/// is unresolved and the client retries. Whether that retry double-applies depends entirely on the
/// host recognising the command id, which is what its own dedupe is for.
///
/// What this server must guarantee is narrower and still essential: it records **no verdict** for
/// a command it never heard about, so it never tells the client a write succeeded or failed on the
/// strength of a guess.
#[tokio::test]
async fn a_host_that_applies_then_vanishes_leaves_no_verdict() {
    let client = fresh().await;
    let scope = unique_scope("apply_then_vanish");
    let host = ReferenceHost::new(&url(), &scope, Behaviour::ApplyThenVanish);
    let cmd = command_for("apply_then_vanish", &scope, "body");

    let out = forward(&client, &host, &cmd).await.expect("forwards");
    assert!(matches!(out, Forwarded::Unresolved { .. }));

    // The change-log row exists — the host really did apply it.
    let page = db::changes_after(&client, &scope, 0, 10)
        .await
        .expect("reads");
    assert_eq!(
        page.changes.len(),
        1,
        "the host's write should be in the log even though it never answered"
    );

    // And no verdict was invented about it.
    assert!(
        matches!(
            dedupe::classify(&client, &cmd).await.expect("classifies"),
            dedupe::Decision::Fresh
        ),
        "a verdict was recorded for a command the server never got an answer about"
    );
}

// -------------------------------------------------------------------------------------------
// Dedupe interacts with forwarding
// -------------------------------------------------------------------------------------------

/// A replay never reaches the host.
///
/// The assertion is the call count. Returning the right answer while still asking the host would
/// satisfy every other test here and double-apply on every retry over a flaky link.
#[tokio::test]
async fn a_replay_never_reaches_the_host() {
    let client = fresh().await;
    let scope = unique_scope("replay_skips_host");
    let host = ReferenceHost::new(&url(), &scope, Behaviour::Apply);
    let cmd = command_for("replay_skips_host", &scope, "body");

    forward(&client, &host, &cmd).await.expect("forwards");
    assert_eq!(host.calls.load(Ordering::SeqCst), 1);

    let again = forward(&client, &host, &cmd).await.expect("forwards");
    assert!(
        matches!(again, Forwarded::Replayed(_)),
        "expected a replay, got {again:?}"
    );
    assert_eq!(
        host.calls.load(Ordering::SeqCst),
        1,
        "the host was asked a second time about a command it had already decided"
    );
}

/// A mutated body is refused without asking the host, and does not overwrite the original record.
#[tokio::test]
async fn a_mutated_body_is_refused_without_asking_the_host() {
    let client = fresh().await;
    let scope = unique_scope("mutated_skips_host");
    let host = ReferenceHost::new(&url(), &scope, Behaviour::Apply);

    let original = command_for("mutated_skips_host", &scope, "the original body");
    forward(&client, &host, &original).await.expect("forwards");
    let calls_after_original = host.calls.load(Ordering::SeqCst);

    let mut tampered = command_for("mutated_skips_host", &scope, "a body the user never wrote");
    tampered.id = original.id;

    let out = forward(&client, &host, &tampered).await.expect("forwards");
    let Forwarded::Mutated(result) = &out else {
        panic!("expected a refusal, got {out:?}");
    };
    assert_eq!(result.status, Status::Rejected);
    assert_eq!(
        host.calls.load(Ordering::SeqCst),
        calls_after_original,
        "the host was asked about a command whose id had been reused"
    );

    // The original record must be untouched, or a tampered replay could overwrite the outcome of
    // the command it is impersonating.
    match dedupe::classify(&client, &original)
        .await
        .expect("classifies")
    {
        dedupe::Decision::Replay(r) => assert_eq!(r.status, Status::Applied),
        other => panic!("the original record was damaged: {other:?}"),
    }
}

// -------------------------------------------------------------------------------------------
// Atomicity
// -------------------------------------------------------------------------------------------

/// Two concurrent forwards of one command id cannot produce two different verdicts.
///
/// The hosts here disagree on purpose: one applies, one rejects. Both see a fresh command and both
/// decide, because neither knows about the other — that race is real and cannot be prevented at
/// this layer. What must hold is that **both callers are told the same thing**, because the record
/// is settled by a single `INSERT … ON CONFLICT DO NOTHING` and the loser reads the winner's row.
///
/// The failure this rules out: a client and its retry being told opposite things about one command,
/// which is exactly what a check-then-act `SELECT`-then-`INSERT` would produce under load. The
/// migration race CI caught at CS-14 was the same shape.
#[tokio::test]
async fn concurrent_forwards_of_one_id_agree_on_a_single_verdict() {
    let setup = fresh().await;
    let scope = unique_scope("concurrent_verdict");
    let cmd = command_for("concurrent_verdict", &scope, "body");
    drop(setup);

    let applying = ReferenceHost::new(&url(), &scope, Behaviour::Apply);
    let rejecting = ReferenceHost::new(
        &url(),
        &scope,
        Behaviour::Reject("the host said no".to_owned()),
    );

    // Separate connections, so this is genuine concurrency rather than two turns on one session.
    let (a, b) = (connect().await, connect().await);
    let (left, right) = tokio::join!(forward(&a, &applying, &cmd), forward(&b, &rejecting, &cmd),);

    let left = left.expect("forwards");
    let right = right.expect("forwards");

    let (Some(l), Some(r)) = (left.result(), right.result()) else {
        panic!("both forwards should have produced a result: {left:?} / {right:?}");
    };

    assert_eq!(
        l.status, r.status,
        "two callers were told different things about one command id: {l:?} vs {r:?}"
    );
    assert_eq!(l.reason, r.reason, "the reasons disagree: {l:?} vs {r:?}");
    assert_eq!(
        l.server_seq, r.server_seq,
        "the seqs disagree: {l:?} vs {r:?}"
    );

    // And the durable record is that same single verdict — not a third answer.
    match dedupe::classify(&a, &cmd).await.expect("classifies") {
        dedupe::Decision::Replay(stored) => {
            assert_eq!(
                stored.status, l.status,
                "the stored verdict is a third answer"
            );
            assert_eq!(stored.reason, l.reason);
        }
        other => panic!("expected exactly one stored verdict, got {other:?}"),
    }
}

/// A recorded outcome is never half-written: status, reason and `server_seq` land together.
///
/// One `INSERT` carries all three, so there is no window in which a command reads back as applied
/// with no seq, or rejected with no reason. The check constraints in `0001_sync_tables.sql` would
/// reject a half-row outright, which is what makes this a real assertion rather than a hope.
#[tokio::test]
async fn a_recorded_outcome_is_never_half_written() {
    let client = fresh().await;
    let scope = unique_scope("no_half_rows");

    for (label, behaviour) in [
        ("applied", Behaviour::Apply),
        ("rejected", Behaviour::Reject("no".to_owned())),
        ("superseded", Behaviour::Supersede),
    ] {
        let host = ReferenceHost::new(&url(), &scope, behaviour);
        let cmd = command_for(&format!("no_half_rows_{label}"), &scope, label);
        forward(&client, &host, &cmd).await.expect("forwards");

        match dedupe::classify(&client, &cmd).await.expect("classifies") {
            dedupe::Decision::Replay(r) => match r.status {
                Status::Applied => assert!(
                    r.server_seq.is_some() && r.reason.is_none(),
                    "{label}: applied rows carry a seq and no reason, got {r:?}"
                ),
                Status::Rejected => assert!(
                    r.reason.is_some() && r.server_seq.is_none(),
                    "{label}: rejected rows carry a reason and no seq, got {r:?}"
                ),
                Status::Superseded => assert!(
                    r.reason.is_none() && r.server_seq.is_none(),
                    "{label}: superseded rows carry neither, got {r:?}"
                ),
            },
            other => panic!("{label}: expected a stored verdict, got {other:?}"),
        }
    }
}
