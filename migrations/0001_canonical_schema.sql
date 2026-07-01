-- 0001_canonical_schema.sql
-- Canonical schema baseline. Fresh / reset databases apply this file directly,
-- then 0002_seed_sources.sql. Later schema changes are new append-only
-- migrations. See migrations/README.md.

CREATE TABLE pool (
    id                BIGINT GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    slug              TEXT  NOT NULL,
    canonical_name    TEXT  NOT NULL,
    coinbase_tags     JSONB NOT NULL,
    payout_addresses  JSONB NOT NULL,

    CONSTRAINT pool_slug_unique UNIQUE (slug)
);

CREATE TABLE source (
    id                       BIGINT GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    code                     TEXT  NOT NULL UNIQUE,
    kind                     TEXT  NOT NULL CHECK (kind IN ('auxpow','live-chaintip')),
    chain                    TEXT,
    instance                 TEXT,
    created_at               BIGINT NOT NULL
);

CREATE TABLE merge_mining_event (
    id                            BIGINT GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    source_id                     BIGINT  NOT NULL REFERENCES source(id),
    child_height                  INTEGER NOT NULL,
    child_block_hash              BYTEA   NOT NULL CHECK (octet_length(child_block_hash) = 32),
    child_block_time              BIGINT  NOT NULL,

    btc_parent_header_hash        BYTEA   NOT NULL CHECK (octet_length(btc_parent_header_hash) = 32),
    btc_parent_header_bytes       BYTEA   NOT NULL CHECK (octet_length(btc_parent_header_bytes) = 80),
    btc_parent_header_time        BIGINT  NOT NULL,
    btc_parent_height             INTEGER,
    btc_parent_kind               TEXT    NOT NULL CHECK (btc_parent_kind IN ('canonical','stale','near','unknown')),
    pow_validates_btc_target      BOOLEAN NOT NULL,
    pow_validates_child_target    BOOLEAN,
    difficulty_epoch_ok           BOOLEAN,

    btc_parent_coinbase_txid      BYTEA   CHECK (btc_parent_coinbase_txid IS NULL OR octet_length(btc_parent_coinbase_txid) = 32),
    btc_parent_coinbase_script    BYTEA,
    btc_parent_coinbase_outputs   BYTEA,
    child_coinbase_txid           BYTEA   CHECK (child_coinbase_txid IS NULL OR octet_length(child_coinbase_txid) = 32),
    child_coinbase_script         BYTEA,
    child_coinbase_outputs        BYTEA,
    child_miner_pool_id           BIGINT  REFERENCES pool(id),
    aux_merkle_proof              BYTEA,

    discovered_at                 BIGINT  NOT NULL,
    confirmed_at                  BIGINT  NOT NULL,
    revoked_at                    BIGINT,
    revocation_reason             TEXT,
    btc_parent_prev_header_hash   BYTEA   NOT NULL CHECK (
        btc_parent_prev_header_hash IS NULL
        OR octet_length(btc_parent_prev_header_hash) = 32
    ),

    UNIQUE (source_id, child_height, child_block_hash),

    CONSTRAINT chk_mme_parent_kind_consistency CHECK (
        CASE btc_parent_kind
            WHEN 'canonical' THEN btc_parent_height IS NOT NULL AND pow_validates_btc_target
            WHEN 'stale'     THEN btc_parent_height IS NOT NULL AND pow_validates_btc_target
            WHEN 'near'      THEN NOT pow_validates_btc_target
            ELSE TRUE
        END
    )
);
CREATE INDEX idx_mme_btc_parent ON merge_mining_event (btc_parent_header_hash) WHERE revoked_at IS NULL;
CREATE INDEX idx_mme_btc_kind ON merge_mining_event (btc_parent_kind, btc_parent_height) WHERE revoked_at IS NULL;
CREATE INDEX idx_mme_source_height ON merge_mining_event (source_id, child_height);
CREATE INDEX idx_mme_parent_prev_non_near
    ON merge_mining_event (btc_parent_prev_header_hash, child_height, id)
    WHERE btc_parent_kind <> 'near'
      AND pow_validates_btc_target
      AND revoked_at IS NULL;
CREATE INDEX idx_mme_btc_parent_non_near
    ON merge_mining_event (btc_parent_header_hash, source_id, child_height, id)
    WHERE btc_parent_kind <> 'near';
CREATE INDEX idx_mme_child_chain_timeline_active
    ON merge_mining_event (
        child_block_time,
        source_id,
        btc_parent_kind,
        child_height,
        id
    )
    WHERE revoked_at IS NULL;

CREATE TABLE pool_identity (
    id          BIGINT GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    pool_id     BIGINT NOT NULL REFERENCES pool(id),
    namespace   TEXT   NOT NULL,
    identifier  TEXT   NOT NULL,

    CONSTRAINT pool_identity_namespace_check CHECK (namespace IN (
        'rsk_miner_address',
        'namecoin_payout_address',
        'syscoin_payout_address',
        'fractal_reward_address',
        'hathor_reward_address',
        'elastos_reward_address',
        'elastos_minerinfo'
    )),
    CONSTRAINT pool_identity_namespace_identifier_unique
        UNIQUE (namespace, identifier)
);
CREATE INDEX idx_pool_identity_pool ON pool_identity (pool_id);

CREATE TABLE rsk_merge_mining_evidence (
    id                       BIGINT GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    event_id                 BIGINT  NOT NULL REFERENCES merge_mining_event(id) ON DELETE CASCADE,

    rsk_block_hash           BYTEA   NOT NULL CHECK (octet_length(rsk_block_hash) = 32),
    rsk_height               INTEGER NOT NULL,
    is_uncle                 BOOLEAN NOT NULL,
    uncle_index              INTEGER,
    uncle_parent_height      INTEGER,
    rsk_miner                BYTEA   NOT NULL CHECK (octet_length(rsk_miner) = 20),
    pool_identity_id         BIGINT  REFERENCES pool_identity(id),

    merge_mining_hash        BYTEA   NOT NULL CHECK (octet_length(merge_mining_hash) = 32),
    merkle_proof             BYTEA,
    coinbase_tail            BYTEA,
    proof_format             TEXT    NOT NULL CHECK (proof_format IN ('rskj_rpc_opaque')),

    CONSTRAINT rsk_evidence_event_unique UNIQUE (event_id),
    CONSTRAINT chk_rsk_uncle_consistency CHECK (
        CASE is_uncle
            WHEN TRUE  THEN uncle_index IS NOT NULL AND uncle_parent_height IS NOT NULL
            WHEN FALSE THEN uncle_index IS NULL     AND uncle_parent_height IS NULL
        END
    )
);
CREATE INDEX idx_rsk_evidence_miner ON rsk_merge_mining_evidence (rsk_miner);
CREATE INDEX idx_rsk_evidence_block_hash ON rsk_merge_mining_evidence (rsk_block_hash);
CREATE INDEX idx_rsk_evidence_pool_ident ON rsk_merge_mining_evidence (pool_identity_id);
CREATE INDEX idx_rsk_evidence_height ON rsk_merge_mining_evidence (rsk_height);

CREATE TABLE hathor_merge_mining_evidence (
    id                  BIGINT  GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    event_id            BIGINT  NOT NULL REFERENCES merge_mining_event(id) ON DELETE CASCADE,

    hathor_block_hash   BYTEA   NOT NULL CHECK (octet_length(hathor_block_hash) = 32),
    hathor_height       INTEGER NOT NULL,
    aux_pow             BYTEA   NOT NULL,
    funds_graph         BYTEA   NOT NULL,
    funds_graph_split   INTEGER NOT NULL CHECK (funds_graph_split >= 2),
    expected_btc_nbits  BIGINT  NOT NULL,
    proof_format        TEXT    NOT NULL CHECK (proof_format IN ('hathor_rfc0006')),
    reward_output_details JSONB,
    reward_addresses      JSONB,

    CONSTRAINT hathor_evidence_event_unique UNIQUE (event_id),
    CONSTRAINT hathor_reward_output_details_array_check
        CHECK (
            reward_output_details IS NULL
            OR jsonb_typeof(reward_output_details) = 'array'
        ),
    CONSTRAINT hathor_reward_addresses_array_check
        CHECK (
            reward_addresses IS NULL
            OR jsonb_typeof(reward_addresses) = 'array'
        )
);

COMMENT ON COLUMN hathor_merge_mining_evidence.reward_output_details IS
  'JSON array of Hathor funds-graph output audit rows: output_index, raw script hex, decoded address if standard, value, token/authority metadata, timelock, and skipped reason.';

COMMENT ON COLUMN hathor_merge_mining_evidence.reward_addresses IS
  'JSON array of unique HTR reward addresses decoded from funds_graph[..funds_graph_split] for child-side hathor_reward_address attribution.';

CREATE INDEX idx_hathor_evidence_block_hash ON hathor_merge_mining_evidence (hathor_block_hash);
CREATE INDEX idx_hathor_evidence_height ON hathor_merge_mining_evidence (hathor_height);

CREATE TABLE event_pool_attribution (
    id                 BIGINT GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    event_id           BIGINT NOT NULL REFERENCES merge_mining_event(id) ON DELETE CASCADE,
    side               TEXT   NOT NULL CHECK (side IN ('btc_parent','child_block')),
    namespace          TEXT   NOT NULL,
    match_kind         TEXT   NOT NULL,
    matched_value      TEXT   NOT NULL,
    pool_id            BIGINT REFERENCES pool(id),
    pool_identity_id   BIGINT REFERENCES pool_identity(id),
    source             TEXT   NOT NULL,
    confidence         TEXT   NOT NULL CHECK (confidence IN ('high','medium','low')),
    details            JSONB  NOT NULL DEFAULT '{}'::jsonb,
    first_seen_at      BIGINT NOT NULL,
    last_seen_at       BIGINT NOT NULL,

    CONSTRAINT event_pool_attribution_side_namespace_value_unique
        UNIQUE (event_id, side, namespace, matched_value),
    CONSTRAINT event_pool_attribution_pool_identity_consistency CHECK (
        pool_identity_id IS NULL OR pool_id IS NOT NULL
    )
);
CREATE INDEX idx_event_pool_attr_event_side ON event_pool_attribution (event_id, side);
CREATE INDEX idx_event_pool_attr_side_pool
    ON event_pool_attribution (side, pool_id)
    WHERE pool_id IS NOT NULL;
CREATE INDEX idx_event_pool_attr_namespace_value ON event_pool_attribution (namespace, matched_value);
CREATE INDEX idx_event_pool_attr_identity
    ON event_pool_attribution (pool_identity_id)
    WHERE pool_identity_id IS NOT NULL;

CREATE TABLE poll_cursor (
    source_id     BIGINT PRIMARY KEY REFERENCES source(id),
    cursor_height INTEGER NOT NULL,
    updated_at    TIMESTAMPTZ NOT NULL DEFAULT now(),
    target_height INTEGER,

    CONSTRAINT chk_poll_cursor_target_height_non_negative
        CHECK (target_height IS NULL OR target_height >= 0)
);

CREATE TABLE poll_pending_reconcile (
    id                    BIGINT  GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    source_id             BIGINT  NOT NULL REFERENCES source(id),
    height                INTEGER NOT NULL,
    kind                  TEXT    NOT NULL CHECK (kind IN ('reconcile', 'supersede')),
    new_child_block_hash  BYTEA   CHECK (new_child_block_hash IS NULL OR octet_length(new_child_block_hash) = 32),
    superseded_event_ids  BIGINT[],
    reason                TEXT,
    attempts              INTEGER NOT NULL DEFAULT 0,

    CONSTRAINT poll_pending_reconcile_unique UNIQUE (source_id, height, kind)
);
CREATE INDEX idx_poll_pending_reconcile_source ON poll_pending_reconcile (source_id);

CREATE TABLE block (
    btc_header_hash              BYTEA PRIMARY KEY CHECK (octet_length(btc_header_hash) = 32),
    btc_prev_header_hash         BYTEA NOT NULL CHECK (octet_length(btc_prev_header_hash) = 32),
    btc_height                   INTEGER,
    btc_height_source            TEXT CHECK (btc_height_source IN ('bitcoin-core','prev-canonical','prev-stale')),
    kind                         TEXT NOT NULL CHECK (kind IN ('canonical','stale','unknown')),
    btc_header_bytes             BYTEA NOT NULL CHECK (octet_length(btc_header_bytes) = 80),
    btc_header_time              BIGINT NOT NULL,
    bitcoin_miner_pool_id        BIGINT REFERENCES pool(id),
    canonical_competitor_hash    BYTEA REFERENCES block(btc_header_hash)
                                      CHECK (canonical_competitor_hash IS NULL OR octet_length(canonical_competitor_hash) = 32),
    total_attestations           INTEGER NOT NULL DEFAULT 0,
    distinct_sources             INTEGER NOT NULL DEFAULT 0,
    auxpow_chain_count           INTEGER NOT NULL DEFAULT 0,
    live_observed                BOOLEAN NOT NULL DEFAULT false,
    core_attested                BOOLEAN NOT NULL DEFAULT false,
    pow_validated                BOOLEAN NOT NULL DEFAULT false,
    difficulty_epoch_ok          BOOLEAN,
    first_attested_at            BIGINT,
    last_attested_at             BIGINT,
    created_at                   BIGINT NOT NULL,
    updated_at                   BIGINT NOT NULL,
    btc_orphan_class             TEXT CHECK (
        btc_orphan_class IS NULL
        OR btc_orphan_class IN ('strict_btc_orphan', 'weak_btc_orphan', 'btc_stale_excluded')
    ),
    btc_coinbase_txid            BYTEA CHECK (btc_coinbase_txid IS NULL OR octet_length(btc_coinbase_txid) = 32),
    btc_coinbase_script          BYTEA,
    btc_coinbase_outputs         BYTEA,
    btc_coinbase_status          TEXT NOT NULL DEFAULT 'not_attempted',

    CONSTRAINT chk_block_kind_height CHECK (
        CASE kind
            WHEN 'canonical' THEN btc_height IS NOT NULL
                              AND btc_height_source = 'bitcoin-core'
                              AND canonical_competitor_hash IS NULL
            WHEN 'stale' THEN btc_height IS NOT NULL
                         AND btc_height_source IN ('bitcoin-core','prev-canonical','prev-stale')
                         AND canonical_competitor_hash IS NOT NULL
            WHEN 'unknown' THEN btc_height IS NULL
                         AND btc_height_source IS NULL
                         AND canonical_competitor_hash IS NULL
            ELSE FALSE
        END
    ),
    CONSTRAINT chk_block_competitor_not_self CHECK (
        canonical_competitor_hash IS NULL OR canonical_competitor_hash <> btc_header_hash
    ),
    CONSTRAINT chk_block_orphan_class_unknown_only
        CHECK (btc_orphan_class IS NULL OR kind = 'unknown'),
    CONSTRAINT chk_block_btc_coinbase_status
        CHECK (btc_coinbase_status IN ('not_attempted','complete','failed')),
    CONSTRAINT chk_block_btc_coinbase_complete_has_script
        CHECK (btc_coinbase_status <> 'complete' OR btc_coinbase_script IS NOT NULL)
);

COMMENT ON COLUMN block.bitcoin_miner_pool_id IS
  'Bitcoin block miner/pool resolved only from this Bitcoin block coinbase evidence; NULL when unresolved.';
COMMENT ON COLUMN block.live_observed IS
  'Local read-model meaning: header has a local classifier observation from the live Bitcoin Core classifier. Phase 5 live observation will replace this with network first-seen semantics.';
COMMENT ON COLUMN block.core_attested IS
  'True when the local Bitcoin Core classifier is an explicit source of truth for this row; exempts Core-proven rows from all-events-revoked downgrade.';
COMMENT ON COLUMN block.difficulty_epoch_ok IS
  'NULL when no expected Bitcoin nBits context exists; false when inferred-height nBits mismatches the same-height canonical header.';
COMMENT ON COLUMN block.btc_orphan_class IS
  'Derived refinement of kind=''unknown'': strict_btc_orphan / weak_btc_orphan / btc_stale_excluded, set by the reconciler only for Core-attested-absent BTC-PoW-valid parents. NULL = pending (not yet Core-checked, or beyond the committed nBits table horizon). Always NULL for canonical/stale.';
COMMENT ON COLUMN block.btc_coinbase_txid IS
  'Bitcoin Core full-block coinbase transaction txid for Core-attested canonical block evidence, stored in rust-bitcoin internal byte order.';
COMMENT ON COLUMN block.btc_coinbase_script IS
  'Bitcoin Core full-block coinbase scriptSig bytes for Core-attested canonical block evidence.';
COMMENT ON COLUMN block.btc_coinbase_outputs IS
  'Consensus-serialized Vec<TxOut> coinbase outputs for Core-attested canonical block evidence; used for payout-address pool fallback.';
COMMENT ON COLUMN block.btc_coinbase_status IS
  'Bitcoin Core coinbase completeness for this block row: not_attempted, complete, or failed.';

CREATE INDEX idx_block_kind_height ON block (kind, btc_height);
CREATE INDEX idx_block_bitcoin_miner_time ON block (bitcoin_miner_pool_id, btc_header_time);
CREATE INDEX idx_block_prev_hash ON block (btc_prev_header_hash);
CREATE INDEX idx_block_competitor
    ON block (canonical_competitor_hash)
    WHERE canonical_competitor_hash IS NOT NULL;
CREATE INDEX idx_block_multi_auxpow
    ON block (auxpow_chain_count)
    WHERE auxpow_chain_count >= 2;
CREATE INDEX idx_block_unknown_header_time
    ON block (btc_header_time DESC, btc_header_hash DESC)
    WHERE kind = 'unknown';
CREATE INDEX idx_block_orphan_class
    ON block (btc_orphan_class, btc_header_time DESC, btc_header_hash DESC)
    WHERE kind = 'unknown' AND pow_validated AND btc_orphan_class IS NOT NULL;
CREATE INDEX idx_block_canonical_complete_height
    ON block (btc_height)
    WHERE kind = 'canonical' AND btc_coinbase_status = 'complete';

CREATE FUNCTION proof_event_ids_are_canonical(evidence JSONB)
RETURNS BOOLEAN
LANGUAGE plpgsql
IMMUTABLE
AS $$
DECLARE
    elem JSONB;
    current_num NUMERIC;
    current_id BIGINT;
    previous_id BIGINT := NULL;
    saw_any BOOLEAN := false;
BEGIN
    IF jsonb_typeof(evidence) <> 'object'
       OR NOT (evidence ? 'contributing_event_ids')
       OR (evidence - 'contributing_event_ids') <> '{}'::jsonb
       OR jsonb_typeof(evidence -> 'contributing_event_ids') <> 'array' THEN
        RETURN false;
    END IF;

    FOR elem IN SELECT jsonb_array_elements(evidence -> 'contributing_event_ids') LOOP
        IF jsonb_typeof(elem) <> 'number' THEN
            RETURN false;
        END IF;

        current_num := (elem #>> '{}')::NUMERIC;
        IF current_num <= 0
           OR scale(current_num) <> 0
           OR current_num > 9223372036854775807::NUMERIC THEN
            RETURN false;
        END IF;

        current_id := current_num::BIGINT;
        IF previous_id IS NOT NULL AND current_id <= previous_id THEN
            RETURN false;
        END IF;

        previous_id := current_id;
        saw_any := true;
    END LOOP;

    RETURN saw_any;
END;
$$;

CREATE TABLE attestation_proof (
    id                    BIGINT GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    btc_header_hash       BYTEA NOT NULL REFERENCES block(btc_header_hash),
    source_id             BIGINT NOT NULL REFERENCES source(id),
    proof_kind            TEXT NOT NULL CHECK (proof_kind IN ('auxpow')),
    evidence              JSONB NOT NULL,
    pow_validated         BOOLEAN NOT NULL,
    discovered_at         BIGINT NOT NULL,
    confirmed_at          BIGINT NOT NULL,
    revoked_at            BIGINT,
    revocation_reason     TEXT,
    UNIQUE (btc_header_hash, source_id, proof_kind),
    CONSTRAINT chk_auxpow_evidence_shape CHECK (proof_event_ids_are_canonical(evidence))
);
CREATE INDEX idx_proof_source_live
    ON attestation_proof (source_id, proof_kind, discovered_at)
    WHERE revoked_at IS NULL;
CREATE INDEX idx_proof_block_source ON attestation_proof (btc_header_hash, source_id, proof_kind);

CREATE TABLE source_health (
    source_id          BIGINT  PRIMARY KEY REFERENCES source(id),
    events             BIGINT  NOT NULL DEFAULT 0,
    last_event_seen    BIGINT,
    near_parents       BIGINT  NOT NULL DEFAULT 0,
    unknown_parents    BIGINT  NOT NULL DEFAULT 0,
    canonical_parents  BIGINT  NOT NULL DEFAULT 0,
    stale_parents      BIGINT  NOT NULL DEFAULT 0,
    updated_at         BIGINT  NOT NULL DEFAULT extract(epoch from now())::bigint,
    strict_orphan_parents BIGINT NOT NULL DEFAULT 0,
    weak_orphan_parents   BIGINT NOT NULL DEFAULT 0
);

CREATE TABLE read_model_invariant (
    id                       BOOLEAN PRIMARY KEY DEFAULT TRUE CHECK (id),
    invalid_unknown_parents  BIGINT  NOT NULL DEFAULT 0,
    source_health_ready      BOOLEAN NOT NULL DEFAULT FALSE,
    updated_at               BIGINT  NOT NULL DEFAULT extract(epoch from now())::bigint
);
INSERT INTO read_model_invariant (id) VALUES (TRUE);

CREATE TABLE rsk_reclassify_watermark (
    id                   BOOLEAN PRIMARY KEY DEFAULT TRUE CHECK (id),
    registry_hash        TEXT    NOT NULL,
    active_event_count   BIGINT  NOT NULL,
    active_event_digest  BIGINT  NOT NULL,
    completed_at         BIGINT  NOT NULL
);

CREATE TABLE bitcoin_core_sync_state (
    source_id                   BIGINT NOT NULL REFERENCES source(id),
    sync_mode                   TEXT NOT NULL CHECK (sync_mode IN ('contiguous')),
    target_tip_height           INTEGER,
    target_tip_hash             BYTEA CHECK (target_tip_hash IS NULL OR octet_length(target_tip_hash) = 32),
    contiguous_complete_height  INTEGER NOT NULL DEFAULT -1,
    last_scanned_height         INTEGER,
    last_attempted_height       INTEGER,
    last_error_code             TEXT,
    last_error_height           INTEGER,
    last_error                  TEXT,
    last_error_details          JSONB NOT NULL DEFAULT '{}'::jsonb,
    created_at                  BIGINT NOT NULL,
    updated_at                  BIGINT NOT NULL,
    PRIMARY KEY (source_id, sync_mode),
    CONSTRAINT chk_bitcoin_core_sync_state_tip
        CHECK (
            (target_tip_height IS NULL AND target_tip_hash IS NULL)
            OR (target_tip_height IS NOT NULL AND target_tip_hash IS NOT NULL)
        ),
    CONSTRAINT chk_bitcoin_core_sync_state_contiguous_non_negative_or_seed
        CHECK (contiguous_complete_height >= -1)
);

COMMENT ON TABLE bitcoin_core_sync_state IS
  'Operator progress and error telemetry for the Bitcoin Core backbone sync. Actual /tree coverage is derived from block rows.';
COMMENT ON COLUMN bitcoin_core_sync_state.sync_mode IS
  'Currently contiguous: the default operator path advances from the first incomplete Bitcoin height.';
COMMENT ON COLUMN bitcoin_core_sync_state.contiguous_complete_height IS
  'Highest height in the genesis-to-tip prefix proven complete and link-consistent by sync-bitcoin-core.';
COMMENT ON COLUMN bitcoin_core_sync_state.last_scanned_height IS
  'Latest height scanned by the most recent sync invocation; telemetry only, not proof of prefix completeness.';
COMMENT ON COLUMN bitcoin_core_sync_state.last_attempted_height IS
  'Latest height attempted by the most recent sync invocation; telemetry only, not proof of prefix completeness.';
COMMENT ON COLUMN bitcoin_core_sync_state.last_error_details IS
  'JSON details for the latest backbone sync error, such as conflicting hashes or a link mismatch.';
