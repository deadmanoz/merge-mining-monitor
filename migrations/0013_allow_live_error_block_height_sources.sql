-- A Core-backed consensus classifier can now derive a full-PoW error block
-- from its predecessor's canonical/stale height. Keep the catalogue source for
-- rules it remains the sole authority for, while allowing the live MTP rule to
-- preserve exactly how its height was established.

ALTER TABLE block
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
                               AND btc_height_source IN (
                                   'error-block-catalog', 'prev-canonical', 'prev-stale'
                               )
                               AND canonical_competitor_hash IS NULL
            WHEN 'unknown' THEN btc_height IS NULL
                         AND btc_height_source IS NULL
                         AND canonical_competitor_hash IS NULL
            ELSE FALSE
        END
    );

COMMENT ON COLUMN block.error_block_reason IS
  'Primary consensus violation for kind=''error_block'', derived by the live Bitcoin Core classifier or the pinned fallback catalogue.';
