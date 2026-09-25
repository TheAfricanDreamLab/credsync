-- credSync server schema, version 3.
--
-- The last watermark a reader managed to establish: the highest `seq` known to be fully committed
-- with no hole beneath it.
--
-- Durable rather than in-memory because it is what a reader falls back to when it cannot take the
-- writer lock quickly. Holding it per-process would mean a freshly started instance had no fallback
-- and would serve nothing until it won the lock once -- worst during a deploy, when instances are
-- starting and traffic is highest.
--
-- One row, enforced by the primary key on a constant. A table with exactly one row is a slightly
-- odd shape, and the alternative -- a settings table keyed by name -- invites everything else to
-- move in and turns a hot read into a scan.

CREATE TABLE IF NOT EXISTS sync_watermark (
    only_row  boolean PRIMARY KEY DEFAULT true CHECK (only_row),
    value     bigint  NOT NULL DEFAULT 0 CHECK (value >= 0)
);

INSERT INTO sync_watermark (only_row, value) VALUES (true, 0)
ON CONFLICT (only_row) DO NOTHING;
