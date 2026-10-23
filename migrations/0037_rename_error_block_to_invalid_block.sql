-- Rename the consensus-invalid full-PoW parent classification from error_block
-- to invalid_block across merge_mining_event, block, and source_health.

ALTER TABLE merge_mining_event
    DROP CONSTRAINT IF EXISTS merge_mining_event_btc_parent_kind_check;

UPDATE merge_mining_event
   SET btc_parent_kind = 'invalid_block'
 WHERE btc_parent_kind = 'error_block';

ALTER TABLE merge_mining_event
    ADD CONSTRAINT merge_mining_event_btc_parent_kind_check
        CHECK (btc_parent_kind IN ('canonical', 'stale', 'invalid_block', 'unknown'));

COMMENT ON COLUMN merge_mining_event.btc_parent_kind IS
  'Bitcoin parent classification at capture/reconciliation time: canonical, stale, invalid_block, or unknown.';

ALTER TABLE block
    DROP CONSTRAINT IF EXISTS block_btc_height_source_check,
    DROP CONSTRAINT IF EXISTS block_kind_check,
    DROP CONSTRAINT IF EXISTS chk_block_kind_height,
    DROP CONSTRAINT IF EXISTS chk_block_error_block_reason,
    DROP CONSTRAINT IF EXISTS chk_block_invalid_block_reason;

DO $$
BEGIN
    IF EXISTS (
        SELECT 1
          FROM information_schema.columns
         WHERE table_schema = current_schema()
           AND table_name = 'block'
           AND column_name = 'error_block_reason'
    ) THEN
        ALTER TABLE block RENAME COLUMN error_block_reason TO invalid_block_reason;
    END IF;

    IF EXISTS (
        SELECT 1
          FROM information_schema.columns
         WHERE table_schema = current_schema()
           AND table_name = 'source_health'
           AND column_name = 'error_block_parents'
    ) THEN
        ALTER TABLE source_health RENAME COLUMN error_block_parents TO invalid_block_parents;
    END IF;
END
$$;

UPDATE block
   SET kind = 'invalid_block',
       btc_height_source = CASE
           WHEN btc_height_source = 'error-block-catalog' THEN 'invalid-block-catalog'
           ELSE btc_height_source
       END
 WHERE kind = 'error_block';

ALTER TABLE block
    ADD CONSTRAINT block_btc_height_source_check
        CHECK (btc_height_source IN (
            'bitcoin-core', 'prev-canonical', 'prev-stale', 'invalid-block-catalog'
        )),
    ADD CONSTRAINT block_kind_check
        CHECK (kind IN ('canonical', 'stale', 'invalid_block', 'unknown')),
    ADD CONSTRAINT chk_block_kind_height CHECK (
        CASE kind
            WHEN 'canonical' THEN btc_height IS NOT NULL
                              AND btc_height_source = 'bitcoin-core'
                              AND canonical_competitor_hash IS NULL
            WHEN 'stale' THEN btc_height IS NOT NULL
                         AND btc_height_source IN ('bitcoin-core', 'prev-canonical', 'prev-stale')
                         AND canonical_competitor_hash IS NOT NULL
            WHEN 'invalid_block' THEN btc_height IS NOT NULL
                                 AND btc_height_source IN (
                                     'invalid-block-catalog', 'prev-canonical', 'prev-stale'
                                 )
                                 AND canonical_competitor_hash IS NULL
            WHEN 'unknown' THEN btc_height IS NULL
                         AND btc_height_source IS NULL
                         AND canonical_competitor_hash IS NULL
            ELSE FALSE
        END
    ),
    ADD CONSTRAINT chk_block_invalid_block_reason CHECK (
        (kind = 'invalid_block') = (invalid_block_reason IS NOT NULL)
    );

COMMENT ON COLUMN block.invalid_block_reason IS
  'Primary consensus violation for kind=''invalid_block'', derived by the live Bitcoin Core classifier or the pinned fallback catalogue.';

COMMENT ON COLUMN block.btc_orphan_class IS
  'Derived refinement of kind=''unknown'': strict_btc_orphan / weak_btc_orphan / excluded, set by the reconciler only for Core-attested-absent BTC-PoW-valid parents. NULL = pending (not yet Core-checked, or beyond the committed nBits table horizon). Always NULL for canonical/stale/invalid_block.';

COMMENT ON COLUMN source_health.invalid_block_parents IS
  'Distinct non-revoked invalid_block Bitcoin parent headers observed by this source.';
