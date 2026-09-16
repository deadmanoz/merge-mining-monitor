-- With a replaced Hathor block recorded as displaced inside the capture
-- transaction (0022, 0024), the write-before-revoke supersession and its
-- durable `supersede` marker have no job left: poll_pending_reconcile holds
-- only heights a producer must re-run. Retire the marker kind and its payload
-- columns; every row is one held height per source, and the unique key
-- replaces the per-source index it covers.

DELETE FROM poll_pending_reconcile WHERE kind = 'supersede';

ALTER TABLE poll_pending_reconcile
    DROP CONSTRAINT poll_pending_reconcile_unique,
    DROP COLUMN kind,
    DROP COLUMN new_child_block_hash,
    DROP COLUMN superseded_event_ids,
    ADD CONSTRAINT poll_pending_reconcile_source_height_unique UNIQUE (source_id, height);

DROP INDEX idx_poll_pending_reconcile_source;
