-- 0028_add_reconcile_queue_expand_unchanged.sql
--
-- Lets a durable Core suffix-cascade primary say whether its dependents must
-- be expanded even when reconciling it changes nothing.
--
-- Until now every successfully reconciled primary became an expansion row,
-- unconditionally, because the drain marked it reconciled in a statement
-- after the reconcile's own transaction: a crash between the two replayed
-- the idempotent primary, which then reported no change, and expansion had
-- to run regardless or the crash could strand a frontier. For the seeds a
-- suffix replacement enqueues that is also the intended meaning: the
-- replacement itself changed those rows, so their dependents need
-- re-examination whatever the primary reconcile finds. For a scheduled
-- unknown-parent recheck it is not: a candidate promoted deep in history
-- would have every stored successor in `block` queued, recursively, each a
-- strict Core reconcile, when the previous sweep stopped at the first
-- descendant that did not change.
--
-- The reconcile now moves its own queue row in the transaction that commits
-- its changes (to expansion when it changed something or the row says
-- `expand_unchanged`, deleted otherwise), so no crash window needs
-- unconditional expansion, and the enqueuer decides: a suffix replacement's
-- seeds carry TRUE, a recheck's candidates FALSE, and a row already waiting
-- for expansion keeps TRUE when it is queued again as a primary. The flag is
-- a property of one row and is never inherited: dependents enter with FALSE,
-- so a cascade continues past a dependent only where reconciling it changes
-- something. Rows queued before this migration were all suffix seeds, so
-- they convert to TRUE.

ALTER TABLE bitcoin_core_reconcile_queue
    ADD COLUMN expand_unchanged BOOLEAN NOT NULL DEFAULT TRUE;

ALTER TABLE bitcoin_core_reconcile_queue
    ALTER COLUMN expand_unchanged SET DEFAULT FALSE;

COMMENT ON TABLE bitcoin_core_reconcile_queue IS
  'Durable two-phase parent and expansion work from an atomic Bitcoin Core canonical suffix replacement or a scheduled unknown-parent recheck.';
COMMENT ON COLUMN bitcoin_core_reconcile_queue.expand_unchanged IS
  'TRUE when the dependents of this parent must be expanded even if reconciling it changes nothing (the enqueuer already changed it, as a suffix replacement does); FALSE stops the cascade at an unchanged parent. Never inherited by the dependents an expansion enqueues.';
