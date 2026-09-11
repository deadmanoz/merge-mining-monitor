-- 0021_add_capture_error.sql
--
-- Adds `capture_error`, producer-owned operational state recording a height a
-- producer could not capture. It exists because the poll cursor cannot express
-- a gap: `poll_cursor` is monotonic (upserted through GREATEST) and carries no
-- error state, and `source_health` is derived and written only by
-- `mmm-read-model`, so a producer cannot record a hole in either.
--
-- One row per (source_id, height). The producer writes the row BEFORE it
-- returns from the failing height, and deletes it only after that SAME height
-- is reprocessed successfully; an unrelated cursor advance never clears it.
-- `/api/v1/sources` reduces a source's rows to the lowest unresolved height,
-- which is the earliest gap and therefore the bound on trustworthy coverage.
--
-- `detail` is diagnostic only and is never projected onto the wire.

CREATE TABLE IF NOT EXISTS capture_error (
    source_id     BIGINT  NOT NULL REFERENCES source(id),
    height        INTEGER NOT NULL,
    block_hash    BYTEA   CHECK (block_hash IS NULL OR octet_length(block_hash) = 32),
    error_kind    TEXT    NOT NULL CHECK (error_kind IN ('malformed_auxpow_proof')),
    detail        TEXT,
    first_seen_at BIGINT  NOT NULL,
    last_seen_at  BIGINT  NOT NULL,

    PRIMARY KEY (source_id, height),
    CONSTRAINT chk_capture_error_height_non_negative CHECK (height >= 0),
    CONSTRAINT chk_capture_error_seen_order CHECK (last_seen_at >= first_seen_at)
);
