-- A near-tip Bitcoin Core reorg changes several parent rows atomically, but
-- dependent read-model cascades deliberately run after that transaction
-- commits. Persist every old/new parent seed so a process exit at that
-- boundary cannot strand descendants on the displaced branch.
CREATE TABLE bitcoin_core_reconcile_queue (
    source_id              BIGINT NOT NULL REFERENCES source(id),
    btc_parent_header_hash BYTEA NOT NULL
                           CHECK (octet_length(btc_parent_header_hash) = 32),
    primary_pending        BOOLEAN NOT NULL DEFAULT TRUE,
    generation             BIGINT NOT NULL DEFAULT 1 CHECK (generation > 0),
    enqueued_at            BIGINT NOT NULL DEFAULT extract(epoch from now())::bigint,
    updated_at             BIGINT NOT NULL DEFAULT extract(epoch from now())::bigint,
    PRIMARY KEY (source_id, btc_parent_header_hash)
);

CREATE INDEX bitcoin_core_reconcile_queue_oldest
    ON bitcoin_core_reconcile_queue (source_id, enqueued_at, btc_parent_header_hash);

COMMENT ON TABLE bitcoin_core_reconcile_queue IS
  'Durable two-phase parent and expansion work from an atomic Bitcoin Core canonical suffix replacement.';
COMMENT ON COLUMN bitcoin_core_reconcile_queue.primary_pending IS
  'TRUE when this parent must be reconciled before its dependents are expanded; FALSE when only durable expansion remains.';
COMMENT ON COLUMN bitcoin_core_reconcile_queue.generation IS
  'Incremented when an already-queued hash changes again so an older drain cannot delete the newer work.';
