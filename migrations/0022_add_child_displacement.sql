-- 0022_add_child_displacement.sql
--
-- Adds child-side displacement to `merge_mining_event`. A child chain can
-- replace the block it carries at a height (a child-chain reorg). The event
-- captured for the replaced block is still valid Bitcoin-side evidence: its
-- parent header carries real proof of work and its coinbase committed to that
-- child block, and neither fact changes when the child chain later prefers a
-- different block at the same height.
--
-- Revocation (`revoked_at`) cannot express this. Revoked evidence is inert in
-- every classification, competition, attestation-proof, source-health, and
-- API query, and a parent whose only witness is revoked collapses to an
-- unknown husk unless Bitcoin Core attests it. Revocation keeps its single
-- meaning: the evidence itself is bad.
--
-- `child_displaced_at` records when a producer observed that the child chain
-- no longer carries this block at its height; `child_displaced_by` is the
-- 32-byte internal-order hash of the block now at that height. The two are set
-- together and cleared together, and a block with a known hash is never
-- displaced by itself (a hashless partial observation may be displaced by
-- any block).
-- Bitcoin-side aggregates never read these columns; only child-centric views
-- (which block the child chain carries at a height) consult them.
--
-- Nothing writes these columns yet. The producer write path and the read
-- projections land in later changes. Existing rows read as not displaced.
-- `idx_mme_source_height` already serves per-height lookups, so no new index.

ALTER TABLE merge_mining_event
    ADD COLUMN IF NOT EXISTS child_displaced_at BIGINT,
    ADD COLUMN IF NOT EXISTS child_displaced_by BYTEA
        CHECK (child_displaced_by IS NULL OR octet_length(child_displaced_by) = 32);

ALTER TABLE merge_mining_event
    DROP CONSTRAINT IF EXISTS chk_mme_child_displacement_pair,
    ADD CONSTRAINT chk_mme_child_displacement_pair
        CHECK ((child_displaced_at IS NULL) = (child_displaced_by IS NULL));

ALTER TABLE merge_mining_event
    DROP CONSTRAINT IF EXISTS chk_mme_child_displaced_by_other_block,
    ADD CONSTRAINT chk_mme_child_displaced_by_other_block
        CHECK (
            child_displaced_by IS NULL
            OR child_block_hash IS NULL
            OR child_displaced_by <> child_block_hash
        );

COMMENT ON COLUMN merge_mining_event.child_displaced_at IS
  'Epoch seconds when a producer observed the child chain no longer carrying this block at child_height; NULL while the block is the chain''s block at that height. Never read by Bitcoin-side aggregates.';
COMMENT ON COLUMN merge_mining_event.child_displaced_by IS
  'Internal-order hash of the child block now carried at child_height; set and cleared together with child_displaced_at.';
