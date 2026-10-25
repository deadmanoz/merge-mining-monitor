-- A stored relationship-signal summary per block row, so the navigator can step through
-- mixed-signal blocks across all history. Derived only by mmm-read-model; the table starts
-- empty until `just rebuild-relationship-signals`.

CREATE TABLE relationship_summary (
    btc_header_hash BYTEA   PRIMARY KEY REFERENCES block(btc_header_hash) ON DELETE CASCADE,
    btc_header_time BIGINT  NOT NULL,
    btc_height      INTEGER,
    mixed           BOOLEAN NOT NULL,
    pool_ids        BIGINT[] NOT NULL
);

CREATE INDEX relationship_summary_mixed_order
    ON relationship_summary (btc_height, btc_header_time, btc_header_hash) WHERE mixed;

COMMENT ON TABLE relationship_summary IS
  'Each block row''s relationship signals summarized by the API''s rule (mmm_capture::relationship): whether they name two or more known pools, and those pools reference-first.';
COMMENT ON COLUMN relationship_summary.btc_height IS
  'The block''s height, copied as the navigator''s sort key with btc_header_time; NULL for a block without one, whose step position the navigator derives from the canonical rows when it reads.';

CREATE TABLE relationship_refresh_queue (
    id                  BIGSERIAL PRIMARY KEY,
    btc_header_hash     BYTEA CHECK (octet_length(btc_header_hash) = 32),
    identity_namespace  TEXT,
    identity_identifier TEXT,
    CONSTRAINT relationship_refresh_queue_one_target CHECK (
        (btc_header_hash IS NOT NULL
            AND identity_namespace IS NULL AND identity_identifier IS NULL)
        OR (btc_header_hash IS NULL
            AND identity_namespace IS NOT NULL AND identity_identifier IS NOT NULL)
    )
);

COMMENT ON TABLE relationship_refresh_queue IS
  'Headers whose relationship_summary inputs changed, or pool identities whose pool changed; writers add one row per write; the read-model sweep replaces identity rows with their headers and the drain refreshes headers.';

ALTER TABLE read_model_invariant
    ADD COLUMN relationship_rules_fingerprint TEXT,
    ADD COLUMN relationship_sweep_cursor BYTEA;

COMMENT ON COLUMN read_model_invariant.relationship_rules_fingerprint IS
  'Fingerprint of the compiled relationship rules (derivation version, pool snapshot, protocol decoder version) the last complete relationship_summary rebuild used.';
COMMENT ON COLUMN read_model_invariant.relationship_sweep_cursor IS
  'The last block hash the scheduled relationship_summary sweep checked; NULL starts a new pass.';
