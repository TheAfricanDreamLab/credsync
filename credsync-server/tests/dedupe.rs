//! CS-15: command dedupe, the result store, and the checksum that stops replay laundering.
//!
//! Against a real Postgres, for the same reason as `pull.rs`: the claims worth testing here are
//! "exactly one insert wins under concurrency" and "the record survives a restart", and a mock
//! would agree with whatever the code expected about both.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use credsync_protocol::{
    Command, CommandId, CommandName, CommandResult, HexString, Payload, Reason, ScopeId, Seq,
    Status,
};
use credsync_server::dedupe::{self, Decision};
use credsync_server::{db, error::ServerError};
use tokio_postgres::{Client, NoTls};

fn url() -> String {
    std::env::var("CREDSYNC_TEST_DATABASE_URL").unwrap_or_else(|_| {
        panic!(
            "CREDSYNC_TEST_DATABASE_URL is not set.\n\
             Start one with `eval \"$(./scripts/test-postgres.sh)\"`."
        )
    })
}

/// A fresh connection. Each call is a distinct session, which is what makes the restart and
/// concurrency tests mean anything.
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

/// A scope unique to this test and this run. See `pull.rs` for why both halves are needed.
fn unique_scope(test: &str) -> ScopeId {
    ScopeId::new(format!("inst:{test}:{}", std::process::id())).expect("valid scope")
}

/// A command id derived from a name, so two tests never collide on the primary key.
fn command_id(test: &str, n: u8) -> CommandId {
    let mut bytes = [0u8; 16];
    bytes[0] = 0x01;
    bytes[6] = 0x70;
    // Process id and a hash of the test name, so ids are unique across runs and across tests.
    let pid = std::process::id().to_be_bytes();
    bytes[8..12].copy_from_slice(&pid);
    bytes[12] = u8::try_from(test.len() % 251).unwrap_or(0);
    bytes[13] = test.bytes().fold(0u8, |a, b| a.wrapping_add(b));
    bytes[15] = n;
    CommandId::from_bytes(bytes).expect("version nibble is 7")
}

fn command(test: &str, n: u8, body: &str) -> Command {
    Command {
        id: command_id(test, n),
        name: CommandName::new("submit_reflection").expect("valid name"),
        scope: unique_scope(test),
        payload: Payload::new(serde_json::json!({ "body": body })).expect("valid payload"),
        client_ts: 1_756_137_600_000,
        checksum: HexString::new("00000000000000000000000000000000").expect("valid hex"),
    }
}

fn applied(id: CommandId, seq: u64) -> CommandResult {
    CommandResult {
        id,
        status: Status::Applied,
        reason: None,
        server_seq: Some(Seq::new(seq).expect("valid seq")),
    }
}

/// Appends a change so `server_seq` references a row that exists.
async fn a_change(client: &Client, scope: &ScopeId) -> i64 {
    let snapshot = serde_json::json!({ "body": "x" });
    db::append_change(
        client,
        &db::NewChange {
            scope: scope.as_str(),
            entity: "reflections",
            entity_id: "r1",
            op: "upsert",
            snapshot: Some(&snapshot),
            row_version: 1,
            schema_version: 1,
        },
    )
    .await
    .expect("appends")
}

// -------------------------------------------------------------------------------------------
// Replay of an identical command
// -------------------------------------------------------------------------------------------

/// An identical replay returns the recorded outcome and applies nothing.
///
/// The second side effect is what this is really checking: the change log must not grow. A dedupe
/// that returned the right answer while re-applying the command would satisfy a client and corrupt
/// the host's data.
#[tokio::test]
async fn an_identical_replay_returns_the_record_without_re_applying() {
    let client = fresh().await;
    let test = "identical_replay";
    let scope = unique_scope(test);
    let cmd = command(test, 1, "the original body");

    assert_eq!(
        dedupe::classify(&client, &cmd).await.expect("classifies"),
        Decision::Fresh,
        "a command nobody has seen must be fresh"
    );

    let seq = a_change(&client, &scope).await;
    let outcome = applied(cmd.id, u64::try_from(seq).expect("positive"));
    dedupe::record(&client, &cmd, &outcome)
        .await
        .expect("records");

    let changes_before = db::changes_after(&client, &scope, 0, 100)
        .await
        .expect("reads")
        .changes
        .len();

    // The replay.
    match dedupe::classify(&client, &cmd).await.expect("classifies") {
        Decision::Replay(returned) => {
            assert_eq!(returned.id, cmd.id);
            assert_eq!(returned.status, Status::Applied);
            assert_eq!(returned.server_seq, outcome.server_seq);
        }
        other => panic!("an identical replay was classified as {other:?}"),
    }

    let changes_after = db::changes_after(&client, &scope, 0, 100)
        .await
        .expect("reads")
        .changes
        .len();
    assert_eq!(
        changes_before, changes_after,
        "the replay applied a second time: the change log grew"
    );
}

/// A rejection replays as a rejection, with its reason intact.
///
/// A dead letter is what the user is shown, so losing the reason on replay would leave them with
/// "it did not save" and nothing else.
#[tokio::test]
async fn a_rejection_replays_with_its_reason() {
    let client = fresh().await;
    let test = "rejection_replay";
    let cmd = command(test, 1, "body");

    let rejected = CommandResult {
        id: cmd.id,
        status: Status::Rejected,
        reason: Some(Reason::new("Submission deadline passed on 14 September.").expect("valid")),
        server_seq: None,
    };
    dedupe::record(&client, &cmd, &rejected)
        .await
        .expect("records");

    match dedupe::classify(&client, &cmd).await.expect("classifies") {
        Decision::Replay(returned) => {
            assert_eq!(returned.status, Status::Rejected);
            assert_eq!(
                returned
                    .reason
                    .expect("a rejection carries a reason")
                    .as_str(),
                "Submission deadline passed on 14 September."
            );
        }
        other => panic!("expected a replay, got {other:?}"),
    }
}

// -------------------------------------------------------------------------------------------
// The security property — `docs/spec.md` §5
// -------------------------------------------------------------------------------------------

/// Reusing an id with a different body is refused, not deduped as a success.
///
/// Without this the dedupe table is a way to launder tampered commands: send a command, note that
/// it succeeded, resend the id with a different body, collect the original's success. The payload
/// would never be looked at.
#[tokio::test]
async fn a_replay_with_a_mutated_payload_is_refused() {
    let client = fresh().await;
    let test = "mutated_payload";
    let scope = unique_scope(test);

    let original = command(test, 1, "the original body");
    let seq = a_change(&client, &scope).await;
    dedupe::record(
        &client,
        &original,
        &applied(original.id, u64::try_from(seq).expect("positive")),
    )
    .await
    .expect("records");

    // Same id, different body.
    let mut tampered = command(test, 1, "a body the user never wrote");
    tampered.id = original.id;
    assert_eq!(
        tampered.id, original.id,
        "the ids must match for this to be a replay"
    );

    match dedupe::classify(&client, &tampered)
        .await
        .expect("classifies")
    {
        Decision::Mutated {
            recorded,
            submitted,
        } => {
            assert_ne!(recorded, submitted, "the two checksums must differ");
            assert_eq!(
                recorded.len(),
                32,
                "a recorded checksum is 32 hex characters"
            );
        }
        Decision::Replay(_) => {
            panic!("a tampered body was deduped as a success — the table is laundering commands")
        }
        Decision::Fresh => panic!("a known command id was treated as fresh"),
    }
}

/// The refusal message does not leak the checksums to the user.
#[tokio::test]
async fn the_mutated_reason_is_for_a_person_not_an_operator() {
    let reason = dedupe::mutated_reason();
    assert!(!reason.as_str().is_empty());
    assert!(
        !reason
            .as_str()
            .contains(|c: char| c.is_ascii_hexdigit() && !c.is_ascii_alphabetic()),
        "the user-facing reason should not carry checksum digits: {reason}"
    );
}

// -------------------------------------------------------------------------------------------
// Concurrency
// -------------------------------------------------------------------------------------------

/// Concurrent submission of the same id applies exactly once.
///
/// One device retrying over a flaky link produces this constantly. Deciding it in application
/// code would be a check-then-act race; `ON CONFLICT DO NOTHING` makes the database the arbiter,
/// and every loser reads the winner's outcome rather than writing its own.
#[tokio::test]
async fn concurrent_submissions_of_one_id_apply_exactly_once() {
    let client = fresh().await;
    let test = "concurrent_once";
    let scope = unique_scope(test);
    let cmd = command(test, 1, "body");
    // Two real changes, and their *actual* seqs.
    //
    // An earlier version wrote one change and used `seq` and `seq + 1`, which assumed the next
    // number was also a row. `seq` is a `bigserial` shared by every scope and every concurrently
    // running test, so `seq + 1` belonged to whichever test happened to insert next -- or to
    // nothing at all, and then the foreign key on `server_seq` refused the write. The test passed
    // only while some other test was busy, which is not a property worth depending on.
    let seq_a = u64::try_from(a_change(&client, &scope).await).expect("positive");
    let seq_b = u64::try_from(a_change(&client, &scope).await).expect("positive");
    let seqs = [seq_a, seq_b];

    // Eight independent sessions, each recording a *different* server_seq, all at once. Only one
    // can win, and everyone must be told the same thing afterwards.
    let mut handles = Vec::new();
    for n in 0..8usize {
        let cmd = cmd.clone();
        let seq = seqs[n % 2];
        handles.push(tokio::spawn(async move {
            let client = connect().await;
            let outcome = applied(cmd.id, seq);
            dedupe::record(&client, &cmd, &outcome).await
        }));
    }

    let mut returned = Vec::new();
    for h in handles {
        returned.push(h.await.expect("task joins").expect("records"));
    }

    // Every caller was told the same outcome.
    let first = returned.first().expect("eight results").clone();
    for r in &returned {
        assert_eq!(
            r.server_seq, first.server_seq,
            "two concurrent submissions were told different outcomes for one command"
        );
    }

    // And exactly one row exists.
    let rows = client
        .query(
            "SELECT count(*) FROM sync_command_results WHERE command_id = $1",
            &[&cmd.id.to_string()],
        )
        .await
        .expect("counts");
    let count: i64 = rows[0].get(0);
    assert_eq!(count, 1, "the same command id was recorded {count} times");
}

// -------------------------------------------------------------------------------------------
// Durability
// -------------------------------------------------------------------------------------------

/// The result store survives a restart.
///
/// Modelled as a new connection — a new session with nothing carried over in memory, which is what
/// a restarted process is from the database's point of view. If the record only existed in a cache
/// this would return `Fresh` and the command would be applied a second time.
#[tokio::test]
async fn the_result_store_survives_a_restart() {
    let test = "survives_restart";
    let scope = unique_scope(test);
    let cmd = command(test, 1, "body");

    {
        let client = fresh().await;
        let seq = u64::try_from(a_change(&client, &scope).await).expect("positive");
        dedupe::record(&client, &cmd, &applied(cmd.id, seq))
            .await
            .expect("records");
    } // the session ends here

    // A completely new session, as a restarted server would have.
    let after = connect().await;
    match dedupe::classify(&after, &cmd).await.expect("classifies") {
        Decision::Replay(returned) => {
            assert_eq!(returned.id, cmd.id);
            assert_eq!(returned.status, Status::Applied);
        }
        other => panic!("the record did not survive a restart: {other:?}"),
    }
}

/// A server-side `server_seq` must reference a change that exists.
///
/// The foreign key is what makes "applied at seq N" mean something. Without it a result could
/// point at a change that was never written, and a client following it would ask for a cursor
/// position that does not exist.
#[tokio::test]
async fn a_result_cannot_reference_a_change_that_does_not_exist() {
    let client = fresh().await;
    let test = "dangling_seq";
    let cmd = command(test, 1, "body");

    let outcome = applied(cmd.id, 9_999_999);
    let err = dedupe::record(&client, &cmd, &outcome).await;

    assert!(
        matches!(err, Err(ServerError::Database { .. })),
        "a result referencing a non-existent change was accepted"
    );
}

// -------------------------------------------------------------------------------------------
// The metric
// -------------------------------------------------------------------------------------------

/// The hit rate reports what it claims to.
#[test]
fn the_hit_rate_is_replays_over_submissions() {
    let mut stats = credsync_server::DedupeStats::default();
    assert!(
        (stats.hit_rate() - 0.0).abs() < f64::EPSILON,
        "a rate over no samples must not be NaN"
    );

    stats.fresh = 3;
    stats.replays = 1;
    assert_eq!(stats.total(), 4);
    assert!((stats.hit_rate() - 0.25).abs() < 1e-9);

    stats.mutated = 4;
    assert_eq!(stats.total(), 8);
    assert!(
        (stats.hit_rate() - 0.125).abs() < 1e-9,
        "mutated submissions count towards the denominator: they are traffic too"
    );
}
