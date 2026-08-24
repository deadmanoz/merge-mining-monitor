-- A pending Core-cache reclassification can be either a newly covered suffix
-- (which needs only pending parents) or a shallow reorg (which can change an
-- existing orphan verdict). Persist that distinction across a failed sweep.

ALTER TABLE bitcoin_core_header_cache_state
    ADD COLUMN orphan_recheck_needed BOOLEAN NOT NULL DEFAULT FALSE;

-- A database upgraded from the first cache migration cannot know why an older
-- pending marker was set. Recheck once conservatively, then future refreshes
-- preserve the exact reason.
UPDATE bitcoin_core_header_cache_state
   SET orphan_recheck_needed = TRUE
 WHERE reclassification_needed;

COMMENT ON TABLE bitcoin_core_header_cache_state IS
  'Singleton cache metadata: a non-regressing timestamp coverage bound plus separate pending-coverage and full-orphan-recheck retry markers.';

COMMENT ON COLUMN bitcoin_core_header_cache_state.orphan_recheck_needed IS
  'A failed shallow-reorg reclassification needs a full orphan recheck; ordinary coverage advances revisit pending rows only.';
