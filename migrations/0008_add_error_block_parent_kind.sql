-- Add the catalogued consensus-invalid full-PoW parent state. Error blocks are
-- not stale blocks or BTC orphans: their header hash meets Bitcoin's target,
-- but the pinned research catalogue re-derives a named consensus violation.

ALTER TABLE merge_mining_event
    DROP CONSTRAINT merge_mining_event_btc_parent_kind_check,
    ADD CONSTRAINT merge_mining_event_btc_parent_kind_check
        CHECK (btc_parent_kind IN ('canonical', 'stale', 'error_block', 'near', 'unknown')),
    DROP CONSTRAINT chk_mme_parent_kind_consistency,
    ADD CONSTRAINT chk_mme_parent_kind_consistency CHECK (
        CASE btc_parent_kind
            WHEN 'canonical' THEN btc_parent_height IS NOT NULL AND pow_validates_btc_target
            WHEN 'stale' THEN btc_parent_height IS NOT NULL AND pow_validates_btc_target
            WHEN 'error_block' THEN btc_parent_height IS NOT NULL AND pow_validates_btc_target
            WHEN 'near' THEN NOT pow_validates_btc_target
            ELSE TRUE
        END
    );

ALTER TABLE block
    ADD COLUMN error_block_reason TEXT,
    DROP CONSTRAINT block_btc_height_source_check,
    ADD CONSTRAINT block_btc_height_source_check
        CHECK (btc_height_source IN (
            'bitcoin-core', 'prev-canonical', 'prev-stale', 'error-block-catalog'
        )),
    DROP CONSTRAINT block_kind_check,
    ADD CONSTRAINT block_kind_check
        CHECK (kind IN ('canonical', 'stale', 'error_block', 'unknown')),
    DROP CONSTRAINT chk_block_kind_height,
    ADD CONSTRAINT chk_block_kind_height CHECK (
        CASE kind
            WHEN 'canonical' THEN btc_height IS NOT NULL
                              AND btc_height_source = 'bitcoin-core'
                              AND canonical_competitor_hash IS NULL
            WHEN 'stale' THEN btc_height IS NOT NULL
                         AND btc_height_source IN ('bitcoin-core', 'prev-canonical', 'prev-stale')
                         AND canonical_competitor_hash IS NOT NULL
            WHEN 'error_block' THEN btc_height IS NOT NULL
                               AND btc_height_source = 'error-block-catalog'
                               AND canonical_competitor_hash IS NULL
            WHEN 'unknown' THEN btc_height IS NULL
                         AND btc_height_source IS NULL
                         AND canonical_competitor_hash IS NULL
            ELSE FALSE
        END
    ),
    ADD CONSTRAINT chk_block_error_block_reason CHECK (
        (kind = 'error_block') = (error_block_reason IS NOT NULL)
    );

COMMENT ON COLUMN block.error_block_reason IS
  'Primary consensus violation from the pinned merge-mining-research error-block catalogue. Present only for kind=''error_block''.';

COMMENT ON COLUMN block.btc_orphan_class IS
  'Derived refinement of kind=''unknown'': strict_btc_orphan / weak_btc_orphan / excluded, set by the reconciler only for Core-attested-absent BTC-PoW-valid parents. NULL = pending (not yet Core-checked, or beyond the committed nBits table horizon). Always NULL for canonical/stale/error_block.';

ALTER TABLE source_health
    ADD COLUMN error_block_parents BIGINT NOT NULL DEFAULT 0;
