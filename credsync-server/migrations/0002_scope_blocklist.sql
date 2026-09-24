-- credSync server schema, version 2.
--
-- The scope blocklist: immediate revocation ahead of token expiry (`docs/spec.md` §8).
--
-- Scope tokens are short-lived and revocation is normally honoured by expiry. This table is the
-- escape hatch for the cases where waiting out a TTL is not an answer -- a withdrawn student, a
-- dismissed staff member, a stolen device. A scope listed here is refused immediately, whatever a
-- valid unexpired token claims.
--
-- Deliberately tiny. It is read on the request path, so it holds one indexed column and nothing
-- that would tempt anyone to join against it.

CREATE TABLE IF NOT EXISTS sync_scope_blocks (
    scope       text        PRIMARY KEY,
    -- Why the cut was made. For the operator and the audit trail; never sent to a client, because
    -- "blocked because: withdrawn 14 September" tells the wrong person something.
    reason      text        NOT NULL,
    blocked_at  timestamptz NOT NULL DEFAULT now(),

    -- The same bound the protocol puts on a scope id. A blocklist row that could never match a
    -- real scope is a row that silently protects nothing.
    CONSTRAINT sync_scope_blocks_scope_len CHECK (octet_length(scope) BETWEEN 1 AND 128)
);
