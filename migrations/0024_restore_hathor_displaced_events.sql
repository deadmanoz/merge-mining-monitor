-- Hathor's producer used to revoke the event of a block the child DAG replaced
-- (hathor_superseded) or voided (hathor_voided). Such an event is still valid
-- Bitcoin-side evidence: its parent's proof of work is real and its coinbase
-- committed to that child block. The producer now records a replaced block as
-- displaced (child_displaced_at / child_displaced_by, migration 0022) and never
-- revokes for either reason. This migration brings the existing rows to that
-- state: every event revoked for one of the two reasons is restored, and its
-- displacement is recorded where the replacing block can be named.
--
-- The replacing block of a restored event is the block the chain carried when
-- the producer revoked it: a block at the same height first seen no later
-- than that revocation and not itself revoked before it (an evidence
-- revocation at the same moment counts: such a block was still the chain's).
-- `discovered_at` is a row's first observation and never moves;
-- `confirmed_at` advances on every re-observation and cannot order events.
-- The producer wrote or restored the replacement, then revoked the replaced
-- event, so this names the block that took its place at that moment, and a
-- twice-replaced height keeps each event's first displacement; two
-- replacements first seen in the same second are told apart by insertion
-- order, the earlier being the one that caused the revocation. Only a
-- superseded event names a replacement. A voided event is restored with no
-- displacement: the void named no replacement, and the old producer voided
-- every event at the height at once, so inferring one from the others would
-- pair them with each other. An event that already carries a displacement
-- keeps it.
--
-- A supersession the old producer began but did not finish (a leftover
-- `supersede` marker in poll_pending_reconcile) is completed the same way: the
-- events it names are marked displaced by the replacement, but only if the
-- replacement's event exists and is active, the rule the old drain applied.
-- The marker was written before the capture; a marker whose replacement is
-- absent or revoked is one whose capture never committed, and the events it
-- names are still the chain's, so it is left for the next observation of the
-- height. The displacement time is the replacement's latest observation
-- (`confirmed_at`): the marker names the replacement, so no ordering is
-- needed, and for a flip-back to a block seen long before, its first
-- observation would predate the displacement. Migration 0025 then retires the
-- markers.
--
-- Only the Hathor source is touched: revocation reasons are free text, and an
-- event of another source revoked with one of these words by hand stays as it
-- is.
--
-- Restoring an event changes its parent's read model. After this migration,
-- with the new release binary, run
--   just reconcile-read-model --all --source auxpow:hathor
--   just rebuild-source-health
-- See docs/operations.md, "Hathor displacement repair (0024)".

DO $$
DECLARE
    v_hathor BIGINT;
    v_superseded_before BIGINT;
    v_voided_before BIGINT;
    v_markers BIGINT;
    v_completed BIGINT;
    v_restored BIGINT;
    v_displaced BIGINT;
    v_remaining BIGINT;
BEGIN
    SELECT id INTO v_hathor FROM source WHERE code = 'auxpow:hathor';

    SELECT count(*) INTO v_superseded_before
      FROM merge_mining_event
     WHERE source_id = v_hathor AND revocation_reason = 'hathor_superseded';
    SELECT count(*) INTO v_voided_before
      FROM merge_mining_event
     WHERE source_id = v_hathor AND revocation_reason = 'hathor_voided';
    SELECT count(*) INTO v_markers
      FROM poll_pending_reconcile WHERE source_id = v_hathor AND kind = 'supersede';

    WITH completed_events AS (
        UPDATE merge_mining_event e
           SET child_displaced_at = r.confirmed_at,
               child_displaced_by = r.child_block_hash
          FROM poll_pending_reconcile q
          JOIN merge_mining_event r
            ON r.source_id = q.source_id
           AND r.child_height = q.height
           AND r.child_block_hash = q.new_child_block_hash
           AND r.revoked_at IS NULL
         WHERE q.source_id = v_hathor
           AND q.kind = 'supersede'
           AND e.id = ANY (q.superseded_event_ids)
           AND e.child_displaced_at IS NULL
           AND e.child_block_hash IS DISTINCT FROM r.child_block_hash
        RETURNING e.id
    )
    SELECT count(*) INTO v_completed FROM completed_events;

    WITH replaced AS (
        SELECT e.id,
               e.revoked_at,
               (SELECT r.child_block_hash
                  FROM merge_mining_event r
                 WHERE e.revocation_reason = 'hathor_superseded'
                   AND r.source_id = e.source_id
                   AND r.child_height = e.child_height
                   AND r.id <> e.id
                   AND r.child_block_hash IS NOT NULL
                   AND r.child_block_hash IS DISTINCT FROM e.child_block_hash
                   AND r.discovered_at <= e.revoked_at
                   AND (r.revoked_at IS NULL OR r.revoked_at >= e.revoked_at)
                 ORDER BY r.discovered_at DESC, r.id ASC
                 LIMIT 1) AS replaced_by
          FROM merge_mining_event e
         WHERE e.source_id = v_hathor
           AND e.revocation_reason IN ('hathor_superseded', 'hathor_voided')
    ),
    restored_events AS (
        UPDATE merge_mining_event e
           SET revoked_at = NULL,
               revocation_reason = NULL,
               child_displaced_at = CASE
                   WHEN e.child_displaced_at IS NOT NULL THEN e.child_displaced_at
                   WHEN r.replaced_by IS NULL THEN NULL
                   ELSE r.revoked_at
               END,
               child_displaced_by = CASE
                   WHEN e.child_displaced_by IS NOT NULL THEN e.child_displaced_by
                   ELSE r.replaced_by
               END
          FROM replaced r
         WHERE e.id = r.id
        RETURNING e.child_displaced_by IS NOT NULL AS is_displaced
    )
    SELECT count(*), count(*) FILTER (WHERE is_displaced)
      INTO v_restored, v_displaced
      FROM restored_events;

    SELECT count(*) INTO v_remaining
      FROM merge_mining_event
     WHERE source_id = v_hathor
       AND revocation_reason IN ('hathor_superseded', 'hathor_voided');

    RAISE NOTICE '0024 before: hathor_superseded=% hathor_voided=% supersede markers=%',
        v_superseded_before, v_voided_before, v_markers;
    RAISE NOTICE '0024 after: markers completed as displacement=% events restored=% (displaced=%) still revoked for those reasons=%',
        v_completed, v_restored, v_displaced, v_remaining;
END
$$;
