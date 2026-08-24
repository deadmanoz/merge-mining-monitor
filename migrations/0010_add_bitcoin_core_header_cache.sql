-- Sparse canonical Bitcoin headers observed directly from the required Core node.
--
-- Epoch-boundary rows are retained. The one non-boundary row is the latest
-- confirmed horizon used to bound timestamp-based classification.

CREATE TABLE bitcoin_core_header (
    height      INTEGER PRIMARY KEY CHECK (height >= 0),
    block_hash  BYTEA NOT NULL UNIQUE CHECK (octet_length(block_hash) = 32),
    block_time  BIGINT NOT NULL CHECK (block_time >= 0),
    bits        BIGINT NOT NULL CHECK (bits BETWEEN 0 AND 4294967295)
);

CREATE UNIQUE INDEX bitcoin_core_header_single_horizon
    ON bitcoin_core_header ((true))
    WHERE height % 2016 <> 0;

COMMENT ON TABLE bitcoin_core_header IS
  'Sparse canonical Bitcoin headers fetched from the required Core node: each difficulty-epoch boundary and the confirmed horizon.';
