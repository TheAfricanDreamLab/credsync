//! Performance characteristics of the engine, measured where a clock is allowed.
//!
//! This lives in `credsync-sim` rather than in `credsync-core/tests` deliberately. `CLAUDE.md` §3
//! forbids clocks in `credsync-core` and states no exemption for its tests; the source check
//! happens to scan only `src/`, but a timing test sitting inside the crate whose defining property
//! is "consults no clock" is the wrong shape regardless of what the script notices. The simulator
//! is already the place where wall-clock behaviour is measured — time compression is one of its
//! own tests — so a performance regression check belongs here.
//!
//! Review caught this on #58, along with the reason the first version of the test measured the
//! wrong thing.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use credsync_core::{Compressor, Engine, OutboxEntry, Registry};
use credsync_protocol::{
    Command, CommandId, CommandName, ConflictClass, EntityName, EntityRegistration, HexString,
    Payload, ProtocolVersion, SchemaVersion, ScopeId,
};

/// A compressor that measures and records nothing.
///
/// Recording the buffer would copy it on every call — O(n^2) across a batch, which is enough to
/// swamp the very thing being measured. The instrument stays out of the measurement.
struct Plain;

impl Compressor for Plain {
    fn compressed_len(&self, bytes: &[u8]) -> usize {
        bytes.len()
    }
}

struct Frozen;
impl credsync_core::Clock for Frozen {
    fn now(&self) -> credsync_core::Timestamp {
        credsync_core::Timestamp::from_millis(0)
    }
}

struct Counter(u8);
impl credsync_core::Entropy for Counter {
    fn fill(&mut self, buf: &mut [u8]) {
        for b in buf.iter_mut() {
            self.0 = self.0.wrapping_add(1);
            *b = self.0;
        }
    }
}

#[derive(Default)]
struct Sink;
impl credsync_core::Storage for Sink {
    fn transact(
        &mut self,
        _ops: &[credsync_core::StorageOp],
    ) -> Result<credsync_core::TxOutcome, credsync_core::StorageError> {
        Ok(credsync_core::TxOutcome::new(0))
    }
    fn row_version(
        &self,
        _entity: &EntityName,
        _entity_id: &credsync_protocol::EntityId,
    ) -> Result<Option<credsync_protocol::RowVersion>, credsync_core::StorageError> {
        Ok(None)
    }

    /// Holds no scopes: this file measures `build_push`, not storage behaviour.
    fn scope_state(
        &self,
        _scope: &ScopeId,
    ) -> Result<Option<credsync_core::StoredScope>, credsync_core::StorageError> {
        Ok(None)
    }

    /// Holds no outbox: this file measures `build_push`, not storage behaviour.
    fn outbox(&self) -> Result<Vec<OutboxEntry>, credsync_core::StorageError> {
        Ok(Vec::new())
    }
}

#[derive(Default)]
struct Nowhere;
impl credsync_core::Transport for Nowhere {
    fn enqueue(
        &mut self,
        _req: credsync_core::WireRequest,
    ) -> Result<credsync_core::RequestId, credsync_core::TransportError> {
        Ok(credsync_core::RequestId::new(1))
    }
}

fn scope() -> ScopeId {
    ScopeId::new("inst:perf").expect("valid scope")
}

fn registry() -> Registry {
    let mut r = Registry::new();
    let entity = EntityName::new("reflections").expect("valid entity");
    r.register_entity(EntityRegistration {
        entity: entity.clone(),
        scope: scope(),
        conflict_class: ConflictClass::OwnerDraft,
        schema_version: SchemaVersion::new(1).expect("valid schema version"),
    });
    r.register_command(
        CommandName::new("submit_reflection").expect("valid name"),
        entity,
    );
    r
}

fn command(n: usize) -> Command {
    let mut bytes = [0u8; 16];
    bytes[0] = 0x01;
    bytes[6] = 0x70;
    bytes[14] = u8::try_from(n / 256).unwrap_or(0);
    bytes[15] = u8::try_from(n % 256).unwrap_or(0);
    Command {
        id: CommandId::from_bytes(bytes).expect("version nibble is 7"),
        name: CommandName::new("submit_reflection").expect("valid name"),
        scope: scope(),
        payload: Payload::new(serde_json::json!({ "body": "x".repeat(200) }))
            .expect("valid payload"),
        client_ts: 0,
        checksum: HexString::new("00000000000000000000000000000000").expect("valid hex"),
    }
}

fn time_with(count: usize) -> std::time::Duration {
    let mut engine = Engine::new(Frozen, Counter(0), Sink, Nowhere, Plain);
    *engine.registry_mut() = registry();

    let schema = SchemaVersion::new(1).expect("valid schema version");
    for n in 0..count {
        engine
            .enqueue(OutboxEntry::new(command(n), schema))
            .expect("enqueues");
    }

    let protocol = ProtocolVersion::new(1).expect("valid protocol");
    let started = std::time::Instant::now();
    for _ in 0..10 {
        engine.build_push(protocol, usize::MAX).expect("builds");
    }
    started.elapsed()
}

/// Building a push is linear in the size of the batch, not quadratic.
///
/// Found by the simulator (#53). The previous implementation cloned the chosen list and re-encoded
/// all of it per entry, so filling a batch of *n* performed *n* encodings averaging *n/2*. The
/// loop breaks at `COMMANDS_MAX_COUNT`, so *n* is capped at 256 — this was never unbounded — but
/// 256 squared is still ~65,000 command-encodings per push attempt, paid on every attempt
/// including the ones that fail and get retried.
///
/// Measured on a 2019 x86 laptop, 20 builds of a full batch with 200-byte payloads:
///
/// ```text
///                32 queued      300 queued (256 chosen)     ratio
///   before          58 ms             4,739 ms               81x
///   after          6.3 ms                43 ms              6.8x
/// ```
///
/// The counts straddle the 256 cap on purpose. An earlier version used 400 and 4,000 — both above
/// the cap, so neither run exercised the growth, and the test passed against the broken code. A
/// performance test that cannot fail is the same trap as an invariant that has never fired.
#[test]
fn building_a_push_is_linear_in_batch_size() {
    // Warm the allocator and the code path so the first measurement is not the outlier.
    let _ = time_with(16);

    let small = time_with(32);
    let large = time_with(300);

    let ratio = large.as_secs_f64() / small.as_secs_f64().max(f64::EPSILON);
    assert!(
        ratio < 30.0,
        "roughly 8x the commands took {ratio:.1}x the time ({small:?} -> {large:?}); \
         the quadratic shape #53 removed measured 81x here"
    );
}
