-- credSync server schema, version 1.
--
-- `docs/spec.md` §1 defines these tables; this file implements that definition and must not drift
-- from it. A schema change is a wire change: it updates spec.md, the migration, and the code that
-- reads it, in one pull request.
--
-- The host owns its own domain tables. credSync owns exactly what is below, which is why a host
-- can adopt it by exposing one endpoint and letting these two tables exist alongside whatever it
-- already has (Design §4.2).

-- ---------------------------------------------------------------------------------------------
-- The append-only change log.
-- ---------------------------------------------------------------------------------------------
CREATE TABLE IF NOT EXISTS sync_changes (
    -- `bigserial`, and therefore SIGNED and SHARED BY EVERY SCOPE. Both facts are load-bearing.
    --
    -- Signed is why the wire caps seq at 2^63-1 rather than 2^64-1 (limits.rs): a value the
    -- server cannot represent must not be expressible in the protocol.
    --
    -- Shared across scopes is why one scope's entries are sparse — 1, 2, 7, 19 is a healthy
    -- scope whose neighbours were busy — and why a client cannot detect a missing middle change
    -- by arithmetic (D-040). The scope digest catches what ordering cannot.
    seq             bigserial PRIMARY KEY,

    scope           text   NOT NULL,
    entity          text   NOT NULL,
    entity_id       text   NOT NULL,

    -- `upsert` or `delete`. Deletes are tombstones: the row is absent, never a snapshot of
    -- emptiness (`docs/spec.md` §1).
    op              text   NOT NULL CHECK (op IN ('upsert', 'delete')),

    -- Snapshots, not diffs. NULL exactly when op = 'delete', which the constraint below enforces
    -- rather than leaving to the writer to remember.
    snapshot        jsonb,

    row_version     bigint NOT NULL CHECK (row_version >= 1),
    schema_version  int    NOT NULL CHECK (schema_version >= 1),

    -- Written by domain handlers via the outbox, in the same transaction as the state change
    -- (`docs/spec.md` §1). Recorded for operators reading the log, never for ordering: ordering is
    -- `seq`, and a wall clock would be a second source of truth that disagrees under load.
    written_at      timestamptz NOT NULL DEFAULT now(),

    CONSTRAINT sync_changes_op_snapshot CHECK (
        (op = 'upsert' AND snapshot IS NOT NULL) OR
        (op = 'delete' AND snapshot IS NULL)
    )
);

-- Pull's only access pattern: "changes for this scope, after this cursor, in seq order".
--
-- `(scope, seq)` rather than `(seq)` because a pull is always scoped — an index on seq alone
-- would make the server scan every other scope's changes to find this one's, which on a shared
-- log is most of the table.
CREATE INDEX IF NOT EXISTS sync_changes_scope_seq ON sync_changes (scope, seq);

-- ---------------------------------------------------------------------------------------------
-- The dedupe record. `docs/spec.md` §1 and §5.
-- ---------------------------------------------------------------------------------------------
CREATE TABLE IF NOT EXISTS sync_command_results (
    -- The client's idempotency key, a UUIDv7. Stored as text because the protocol defines it as
    -- a hyphenated lowercase string and validates it there; converting to `uuid` here would mean
    -- two representations that can disagree about what is canonical.
    command_id      text        PRIMARY KEY,

    -- BLAKE3 over the payload's canonical encoding, truncated to 128 bits (D-031).
    --
    -- Recorded so a replay whose body has been MUTATED is refused as a distinct invalid request
    -- rather than deduped as a success. Without it the dedupe table becomes a way to launder
    -- tampered commands: send a command, note its id, resend with a different body and collect
    -- the original's success (`docs/spec.md` §5).
    payload_checksum text       NOT NULL CHECK (char_length(payload_checksum) = 32),

    status          text        NOT NULL CHECK (status IN ('applied', 'rejected', 'superseded')),

    -- Required when rejected, and the constraint says so. A rejection reaches a person, and
    -- "rejected" with no explanation is a dead end for them (`docs/spec.md` §3.3).
    reason          text,

    -- Where the resulting change landed, when applied.
    server_seq      bigint      REFERENCES sync_changes (seq),

    scope           text        NOT NULL,
    recorded_at     timestamptz NOT NULL DEFAULT now(),

    CONSTRAINT sync_command_results_reason CHECK (
        status <> 'rejected' OR reason IS NOT NULL
    )
);

-- Dead letters are surfaced to the user per scope, so that is how they are read.
CREATE INDEX IF NOT EXISTS sync_command_results_scope
    ON sync_command_results (scope, recorded_at);
