-- 0027_schedule_core_rechecks_by_generation.sql
--
-- Replaces the two retry booleans on the Core-header-cache state with a
-- generation counter, so scheduled reclassification work is consumed by an
-- explicit resumable job instead of inside every producer's cache refresh.
--
-- Until now `reclassification_needed` and `orphan_recheck_needed` were set by
-- a cache refresh (a horizon advance, a shallow reorg across a retarget
-- boundary, an empty cache) or by a migration such as 0019, and consumed by
-- the next refresh, which ran the whole unknown-parent reclassification
-- under the exclusive cache lock before releasing it. On the production
-- host that pass re-evaluated every unknown parent against Bitcoin Core one
-- round trip at a time and held live capture on every chain for fifteen
-- hours before it was abandoned; and because the booleans were cleared
-- unconditionally at the end of a pass, a trigger that arrived mid-pass
-- would have been erased with the work it asked for undone.
--
-- Now a trigger increments `recheck_pending_generation` and widens the
-- pending scope: `recheck_pending_orphans` says already classified orphans
-- must be revisited (a shallow reorg or an epoch boundary inside existing
-- coverage), and `recheck_pending_sources` limits the candidates to events
-- witnessed by those sources (NULL means every source, an empty array none;
-- a union with NULL is NULL). Work is pending while the pending generation
-- exceeds `recheck_acknowledged_generation`.
--
-- Triggers differ in what they do to work already done. The job binds one
-- pass to the pending generation and scope (`recheck_pass_*`, with its
-- keyset cursor), clearing the pending scope it consumed, and continues it
-- from its cursor. A shallow reorg, a boundary inside existing coverage, an
-- empty cache, or a migration can change verdicts already given: those are
-- invalidating, and clear the pass in flight, folding its scope back into
-- the pending scope, so the next bind starts over with the merged scope. An
-- ordinary horizon advance only lets rows that had no verdict be decided: it
-- is additive, leaves the pass running, and must not restart a sweep that
-- live producers would otherwise keep restarting every Bitcoin block. On
-- completion the job acknowledges the pass generation, and a follow-up pass
-- then covers only the scope accumulated since the bind. A migration that
-- needs a recheck schedules one the same way; see migrations/README.md.
--
-- Existing state converts in place: a set boolean becomes pending
-- generation 1 with the matching scope; nothing pending stays at 0.

ALTER TABLE bitcoin_core_header_cache_state
    ADD COLUMN recheck_pending_generation BIGINT NOT NULL DEFAULT 0
        CHECK (recheck_pending_generation >= 0),
    ADD COLUMN recheck_pending_orphans BOOLEAN NOT NULL DEFAULT FALSE,
    ADD COLUMN recheck_pending_sources TEXT[] DEFAULT '{}',
    ADD COLUMN recheck_acknowledged_generation BIGINT NOT NULL DEFAULT 0
        CHECK (recheck_acknowledged_generation >= 0),
    ADD COLUMN recheck_pass_generation BIGINT,
    ADD COLUMN recheck_pass_orphans BOOLEAN,
    ADD COLUMN recheck_pass_sources TEXT[],
    ADD COLUMN recheck_cursor_height BIGINT,
    ADD COLUMN recheck_cursor_id BIGINT,
    ADD CONSTRAINT bitcoin_core_header_cache_state_acknowledged_le_pending
        CHECK (recheck_acknowledged_generation <= recheck_pending_generation),
    ADD CONSTRAINT bitcoin_core_header_cache_state_pass_shape CHECK (
        (recheck_pass_generation IS NULL
            AND recheck_pass_orphans IS NULL
            AND recheck_pass_sources IS NULL
            AND recheck_cursor_height IS NULL
            AND recheck_cursor_id IS NULL)
        OR (recheck_pass_generation IS NOT NULL
            AND recheck_pass_orphans IS NOT NULL
            AND ((recheck_cursor_height IS NULL) = (recheck_cursor_id IS NULL)))
    );

UPDATE bitcoin_core_header_cache_state
   SET recheck_pending_generation = CASE
           WHEN reclassification_needed OR orphan_recheck_needed THEN 1 ELSE 0 END,
       recheck_pending_orphans = orphan_recheck_needed,
       recheck_pending_sources = CASE
           WHEN reclassification_needed OR orphan_recheck_needed THEN NULL ELSE '{}'::text[] END
 WHERE singleton;

ALTER TABLE bitcoin_core_header_cache_state
    DROP COLUMN reclassification_needed,
    DROP COLUMN orphan_recheck_needed;

COMMENT ON TABLE bitcoin_core_header_cache_state IS
  'Singleton cache metadata: a non-regressing timestamp coverage bound and the generation-scheduled unknown-parent recheck (pending generation and scope, acknowledged generation, and the bound pass with its cursor).';
COMMENT ON COLUMN bitcoin_core_header_cache_state.recheck_pending_generation IS
  'Incremented by every trigger that requires unknown parents to be reclassified; work is pending while it exceeds recheck_acknowledged_generation.';
COMMENT ON COLUMN bitcoin_core_header_cache_state.recheck_pending_orphans IS
  'Whether the pending recheck must revisit already classified orphans (a shallow reorg or a boundary inside existing coverage), not only rows with no orphan class yet.';
COMMENT ON COLUMN bitcoin_core_header_cache_state.recheck_pending_sources IS
  'Witness sources whose events the pending recheck covers, accumulated since the last bind; NULL covers every source, an empty array none, and a union with NULL is NULL.';
COMMENT ON COLUMN bitcoin_core_header_cache_state.recheck_pass_generation IS
  'The generation the current pass is bound to and acknowledges; an invalidating trigger clears the pass and folds its scope back into the pending scope.';
