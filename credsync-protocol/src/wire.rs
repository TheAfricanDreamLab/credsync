//! The wire types, one per shape in `docs/spec.md` §3.

use crate::canonical;
use crate::error::{ProtocolError, Result};
use crate::ids::{CommandId, CommandName, EntityId, EntityName, HexString, Reason, ScopeId};
use crate::limits;
use crate::nums::{Cursor, LimitBytes, ProtocolVersion, RowVersion, SchemaVersion, Seq};
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// A bounded, opaque JSON document belonging to the host.
///
/// The protocol does not model its contents but does bound its size and require it to be an
/// object. Size is measured over the **canonical** encoding, so the limit means the same thing
/// on both sides regardless of how the sender formatted its JSON.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "Value", into = "Value")]
pub struct Document<const MAX: usize, const FIELD: u8>(Value);

/// `snapshot`: at most 256 KB.
pub type Snapshot = Document<{ limits::SNAPSHOT_MAX_BYTES }, 0>;
/// `payload`: at most 64 KB.
pub type Payload = Document<{ limits::PAYLOAD_MAX_BYTES }, 1>;

impl<const MAX: usize, const FIELD: u8> Document<MAX, FIELD> {
    const fn field_name() -> &'static str {
        if FIELD == 0 { "snapshot" } else { "payload" }
    }

    /// Validates and wraps a host document.
    ///
    /// # Errors
    /// Returns [`ProtocolError::NotAnObject`] for a non-object, [`ProtocolError::FloatNotAllowed`]
    /// if any value is a floating-point number, or [`ProtocolError::TooLong`] if the canonical
    /// encoding exceeds the limit.
    pub fn new(v: Value) -> Result<Self> {
        if !v.is_object() {
            return Err(ProtocolError::NotAnObject {
                field: Self::field_name(),
            });
        }
        reject_floats(&v, Self::field_name())?;
        let len = canonical::canonicalize(&v)?.len();
        if len > MAX {
            return Err(ProtocolError::TooLong {
                field: Self::field_name(),
                limit: MAX,
                actual: len,
            });
        }
        Ok(Self(v))
    }

    /// Borrows the underlying JSON.
    #[must_use]
    pub const fn as_value(&self) -> &Value {
        &self.0
    }
}

impl<const MAX: usize, const FIELD: u8> TryFrom<Value> for Document<MAX, FIELD> {
    type Error = ProtocolError;
    fn try_from(v: Value) -> Result<Self> {
        Self::new(v)
    }
}

impl<const MAX: usize, const FIELD: u8> From<Document<MAX, FIELD>> for Value {
    fn from(d: Document<MAX, FIELD>) -> Self {
        d.0
    }
}

/// Refuses floating-point numbers anywhere in a host document.
///
/// Floats break canonical encoding, and therefore break scope digests. `serde_json`'s float
/// parsing is not exact — `2.2283095771495367e-21` re-parses one ULP away and then re-encodes as
/// `2.228309577149537e-21` — so a document containing a float does not survive a
/// serialize/parse/serialize cycle unchanged.
///
/// Since the digest (`docs/spec.md` §5) is computed over the canonical encoding, that would let
/// two sides holding the *same logical row* compute *different* digests. The client would declare
/// silent divergence that never happened and re-bootstrap against a phantom — the divergence
/// detector firing on itself.
///
/// Hosts needing fractional values encode them as scaled integers (cents, basis points) or as
/// decimal strings, both of which are exact and portable. JSON floats are not portable across
/// JavaScript, Python and Postgres in any case.
fn reject_floats(v: &Value, field: &'static str) -> Result<()> {
    match v {
        Value::Number(n) if n.is_f64() => Err(ProtocolError::FloatNotAllowed { field }),
        Value::Array(items) => items.iter().try_for_each(|i| reject_floats(i, field)),
        Value::Object(map) => map.values().try_for_each(|i| reject_floats(i, field)),
        _ => Ok(()),
    }
}

/// How a change acts on a row. `docs/spec.md` §1: deletes are tombstones.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Op {
    /// Insert or replace the row.
    Upsert,
    /// Tombstone the row.
    Delete,
}

/// The conflict class an entity is registered under. `docs/spec.md` §6.
///
/// Declared by the registry rather than by convention, so "no command may target a
/// server-authoritative entity" is enforceable rather than merely documented.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ConflictClass {
    /// Pull-only. No command may target these entities.
    ServerAuthoritative,
    /// Last-write-wins per field, decided by `row_version`.
    OwnerDraft,
    /// New versions never overwrite; ordering is server `seq`.
    AppendOnly,
}

impl ConflictClass {
    /// Whether a client command may target an entity of this class.
    ///
    /// `docs/spec.md` §6: server-authoritative entities are pull-only.
    #[must_use]
    pub const fn accepts_commands(self) -> bool {
        !matches!(self, Self::ServerAuthoritative)
    }
}

/// An entity registered with credSync. `docs/spec.md` §1.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EntityRegistration {
    /// The entity's name.
    pub entity: EntityName,
    /// Which scope its rows belong to.
    pub scope: ScopeId,
    /// Its conflict class.
    pub conflict_class: ConflictClass,
    /// The schema version the registry currently expects.
    pub schema_version: SchemaVersion,
}

/// One entry in the append-only change log. `docs/spec.md` §1.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Change {
    /// Position in the log.
    pub seq: Seq,
    /// Which entity the row belongs to.
    pub entity: EntityName,
    /// The row's identifier.
    pub entity_id: EntityId,
    /// Upsert or tombstone.
    pub op: Op,
    /// The full row. Absent for a tombstone — snapshots, not diffs.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub snapshot: Option<Snapshot>,
    /// Server-assigned row version.
    pub row_version: RowVersion,
    /// The schema this snapshot was written under.
    pub schema_version: SchemaVersion,
}

impl Change {
    /// Checks the rule tying `op` to `snapshot`.
    ///
    /// # Errors
    /// Returns [`ProtocolError::Inconsistent`] if an upsert carries no snapshot or a delete
    /// carries one.
    pub const fn validate(&self) -> Result<()> {
        match (self.op, self.snapshot.is_some()) {
            (Op::Upsert, false) => Err(ProtocolError::Inconsistent {
                detail: "op=upsert requires a snapshot",
            }),
            (Op::Delete, true) => Err(ProtocolError::Inconsistent {
                detail: "op=delete must not carry a snapshot",
            }),
            _ => Ok(()),
        }
    }
}

/// One scope's worth of changes. `docs/spec.md` §3.2.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Batch {
    /// The scope these changes belong to.
    pub scope: ScopeId,
    /// Changes, strictly `seq`-ordered.
    pub changes: Vec<Change>,
    /// Where the client's cursor moves to.
    pub next_cursor: Cursor,
    /// Whether more changes remain.
    pub has_more: bool,
    /// Checksum over this batch's canonical encoding.
    pub checksum: HexString,
    /// The server's state digest for this scope after these changes.
    pub digest: HexString,
}

impl Batch {
    /// Checks that changes are strictly `seq`-ordered.
    ///
    /// # Errors
    /// Returns [`ProtocolError::Inconsistent`] on a repeated or decreasing `seq`. A gap is not
    /// detectable here — only the client, which knows its cursor, can see one.
    pub fn validate_ordering(&self) -> Result<()> {
        let ordered = self
            .changes
            .windows(2)
            .all(|w| w[0].seq.get() < w[1].seq.get());
        if ordered {
            Ok(())
        } else {
            Err(ProtocolError::Inconsistent {
                detail: "changes must be strictly seq-ordered within a scope",
            })
        }
    }
}

/// One scope-and-cursor pair in a pull request.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ScopeCursor {
    /// The scope to pull.
    pub scope: ScopeId,
    /// Where to resume from.
    pub cursor: Cursor,
}

/// `GET /sync/pull`. `docs/spec.md` §3.2.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PullRequest {
    /// Wire protocol version.
    pub protocol: ProtocolVersion,
    /// The scopes to pull, each with its cursor.
    pub scopes: Vec<ScopeCursor>,
    /// Client hint for batch size; the server may return less, never more.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub limit_bytes: Option<LimitBytes>,
}

/// Response to `GET /sync/pull`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PullResponse {
    /// Wire protocol version.
    pub protocol: ProtocolVersion,
    /// One batch per requested scope.
    pub batches: Vec<Batch>,
}

/// `GET /sync/bootstrap`. `docs/spec.md` §3.1.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BootstrapRequest {
    /// Wire protocol version.
    pub protocol: ProtocolVersion,
    /// The scope to bootstrap.
    pub scope: ScopeId,
    /// Resumes a partial bootstrap.
    pub after: Cursor,
}

/// One row in a bootstrap response. Carries no `seq`: these are current rows, not log entries.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BootstrapRow {
    /// Which entity the row belongs to.
    pub entity: EntityName,
    /// The row's identifier.
    pub entity_id: EntityId,
    /// The full row.
    pub snapshot: Snapshot,
    /// Server-assigned row version.
    pub row_version: RowVersion,
    /// The schema this snapshot was written under.
    pub schema_version: SchemaVersion,
}

/// Response to `GET /sync/bootstrap`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BootstrapResponse {
    /// Wire protocol version.
    pub protocol: ProtocolVersion,
    /// The scope bootstrapped.
    pub scope: ScopeId,
    /// Current rows.
    pub rows: Vec<BootstrapRow>,
    /// The log position these rows are consistent with.
    pub next_cursor: Cursor,
    /// Whether more rows remain.
    pub has_more: bool,
    /// Checksum over this response's canonical encoding.
    pub checksum: HexString,
    /// The server's state digest for this scope.
    pub digest: HexString,
}

/// A client-originated domain mutation. `docs/spec.md` §1 and §3.3.
///
/// Carries no protocol version: that is the request envelope's job, and repeating it would give
/// two sources of truth for one fact.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Command {
    /// Idempotency key. The server dedupes on this.
    pub id: CommandId,
    /// The domain operation being requested.
    pub name: CommandName,
    /// Which scope it applies to.
    pub scope: ScopeId,
    /// Opaque host arguments.
    pub payload: Payload,
    /// The client's clock when authored. **A hint only** — `docs/spec.md` §6.
    pub client_ts: i64,
    /// Checksum over the payload, so a mutated replay is refused rather than deduped.
    pub checksum: HexString,
}

/// `POST /sync/push`. `docs/spec.md` §3.3.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PushRequest {
    /// Wire protocol version.
    pub protocol: ProtocolVersion,
    /// The commands to apply.
    pub commands: Vec<Command>,
}

impl PushRequest {
    /// Checks both push limits: entry count and total canonical bytes.
    ///
    /// # Errors
    /// Returns [`ProtocolError::TooManyItems`] above 256 commands, or
    /// [`ProtocolError::TooLong`] above 1 MB of canonical encoding.
    ///
    /// Both are needed: a count limit alone would permit 256 maximum-size payloads, which is
    /// 16 MB — an absurd request to put on a 2G link.
    pub fn validate(&self) -> Result<()> {
        if self.commands.len() > limits::COMMANDS_MAX_COUNT {
            return Err(ProtocolError::TooManyItems {
                field: "commands",
                limit: limits::COMMANDS_MAX_COUNT,
                actual: self.commands.len(),
            });
        }
        let bytes = canonical::encoded_len(&self.commands)?;
        if bytes > limits::COMMANDS_MAX_TOTAL_BYTES {
            return Err(ProtocolError::TooLong {
                field: "commands",
                limit: limits::COMMANDS_MAX_TOTAL_BYTES,
                actual: bytes,
            });
        }
        Ok(())
    }
}

/// What the host decided about a command. `docs/spec.md` §3.3.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Status {
    /// The host applied it.
    Applied,
    /// The host refused it. A reason is required.
    Rejected,
    /// A later command overtook it.
    Superseded,
}

/// The outcome of one command.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CommandResult {
    /// Which command this answers.
    pub id: CommandId,
    /// What happened.
    pub status: Status,
    /// Why, when rejected.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub reason: Option<Reason>,
    /// Where the resulting change landed in the log, when applied.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub server_seq: Option<Seq>,
}

impl CommandResult {
    /// Checks the rule tying `status` to `reason`.
    ///
    /// # Errors
    /// Returns [`ProtocolError::Inconsistent`] if a rejection carries no reason. A rejected
    /// command surfaces to a user, and "rejected" with no explanation is a dead end for them.
    pub const fn validate(&self) -> Result<()> {
        if matches!(self.status, Status::Rejected) && self.reason.is_none() {
            return Err(ProtocolError::Inconsistent {
                detail: "status=rejected requires a reason",
            });
        }
        Ok(())
    }
}

/// Response to `POST /sync/push`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PushResponse {
    /// Wire protocol version.
    pub protocol: ProtocolVersion,
    /// One result per submitted command.
    pub results: Vec<CommandResult>,
}

/// The `426` body sent to a client below the N-1 window. `docs/spec.md` §7.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ForcedUpgrade {
    /// The lowest protocol version the server still accepts.
    pub min_protocol: ProtocolVersion,
    /// The version the server prefers.
    pub current_protocol: ProtocolVersion,
    /// Human-readable explanation for the upgrade prompt.
    pub reason: Reason,
}
