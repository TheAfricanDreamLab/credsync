//! Field limits, transcribed from `docs/spec.md` §2.1.
//!
//! These constants are the single source of truth in code for what the specification declares.
//! If a limit changes, it changes in `spec.md` and here **in the same pull request** — the spec
//! is law, and a limit that disagrees with it is a defect.
//!
//! Oversize input is **rejected, never truncated**: truncation turns hostile input into a
//! silently wrong value, which is strictly worse than a refusal.

/// `protocol`: 1..=65535.
pub const PROTOCOL_VERSION_MIN: u16 = 1;

/// `schema_version`: 1..=65535.
pub const SCHEMA_VERSION_MIN: u16 = 1;

/// `scope`: 128 bytes.
pub const SCOPE_MAX_BYTES: usize = 128;

/// `entity`: 64 bytes.
pub const ENTITY_MAX_BYTES: usize = 64;

/// `entity_id`: 64 bytes.
pub const ENTITY_ID_MAX_BYTES: usize = 64;

/// `name` (command): 64 bytes.
pub const COMMAND_NAME_MAX_BYTES: usize = 64;

/// `reason`: 256 bytes.
pub const REASON_MAX_BYTES: usize = 256;

/// `digest` and `checksum`: at most 64 lowercase hex characters.
///
/// The exact width is fixed by the algorithm chosen at CS-4 (`DECISIONS.md` O-001). Until then
/// the codec enforces the bound and the character set, not a specific width.
pub const HEX_MAX_CHARS: usize = 64;

/// `snapshot`: 256 KB of canonical encoding.
pub const SNAPSHOT_MAX_BYTES: usize = 256 * 1024;

/// `payload`: 64 KB of canonical encoding.
pub const PAYLOAD_MAX_BYTES: usize = 64 * 1024;

/// `limit_bytes`: 1..=1048576.
pub const LIMIT_BYTES_MIN: u32 = 1;

/// `limit_bytes`: 1..=1048576.
pub const LIMIT_BYTES_MAX: u32 = 1024 * 1024;

/// Default compressed batch budget, `docs/spec.md` §2.
pub const DEFAULT_BATCH_BUDGET_BYTES: u32 = 100_000;

/// `commands[]`: 256 entries per push request.
pub const COMMANDS_MAX_COUNT: usize = 256;

/// `commands[]`: 1 MB total canonical bytes per push request, whichever binds first.
pub const COMMANDS_MAX_TOTAL_BYTES: usize = 1024 * 1024;

/// `seq`, `next_cursor`, `server_seq`, `row_version`: 1..=2^63-1.
///
/// Capped at [`i64::MAX`] rather than [`u64::MAX`] because the change log's `seq` is a Postgres
/// `bigserial`, which is signed. A value the server cannot represent must not be expressible in
/// the wire types.
pub const SEQ_MAX: u64 = i64::MAX as u64;

/// `seq` and friends are 1-based; 0 is reserved to mean "no cursor yet".
pub const SEQ_MIN: u64 = 1;

/// A cursor of `0` means "start from the beginning" and is the one place 0 is legal.
pub const CURSOR_START: u64 = 0;
