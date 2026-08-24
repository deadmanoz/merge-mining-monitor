-- Sparse canonical Bitcoin headers observed directly from the required Core node.
--
-- Reorg-safe epoch-boundary rows are retained. The current shallow epoch and
-- one non-boundary synced-tip horizon are replaced from Core on each refresh.

CREATE TABLE bitcoin_core_header (
    height      INTEGER PRIMARY KEY CHECK (height >= 0),
    block_hash  BYTEA NOT NULL UNIQUE CHECK (octet_length(block_hash) = 32),
    block_time  BIGINT NOT NULL CHECK (block_time >= 0),
    bits        BIGINT NOT NULL CHECK (bits BETWEEN 0 AND 4294967295),
    is_final    BOOLEAN NOT NULL DEFAULT FALSE
        CHECK (NOT is_final OR height % 2016 = 0)
);

CREATE UNIQUE INDEX bitcoin_core_header_single_horizon
    ON bitcoin_core_header ((true))
    WHERE height % 2016 <> 0;

COMMENT ON TABLE bitcoin_core_header IS
  'Sparse canonical Bitcoin headers fetched from the required Core node: immutable reorg-safe difficulty-epoch boundaries plus the replaceable current epoch and synced-tip horizon.';
