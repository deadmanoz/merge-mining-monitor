-- The mixed-signal navigator steps an orphan at the height of the canonical block with the
-- latest header time at or before its own; this index answers that lookup per orphan.

CREATE INDEX idx_block_canonical_header_time
    ON block (btc_header_time, btc_height)
    WHERE kind = 'canonical';
