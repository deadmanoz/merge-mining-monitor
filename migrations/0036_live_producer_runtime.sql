-- Operational liveness is separate from evidence and durable progress.
ALTER TABLE bitcoin_core_header_cache_state
    ADD COLUMN publication_revision BIGINT NOT NULL DEFAULT 0;

CREATE TABLE producer_runtime_state (
    source_id BIGINT PRIMARY KEY REFERENCES source(id),
    run_id BIGINT GENERATED ALWAYS AS IDENTITY UNIQUE,
    phase TEXT NOT NULL CHECK (phase IN ('startup', 'running', 'stopped')),
    state TEXT NOT NULL CHECK (state IN ('recovering', 'healthy', 'waiting', 'failed', 'stopped')),
    dependency TEXT CHECK (dependency IN ('bitcoin-core')),
    reason_code TEXT,
    failure_since BIGINT,
    consecutive_failures BIGINT NOT NULL DEFAULT 0 CHECK (consecutive_failures >= 0),
    last_attempt_at BIGINT,
    last_success_at BIGINT,
    next_retry_at BIGINT,
    heartbeat_at BIGINT NOT NULL DEFAULT extract(epoch FROM clock_timestamp())::bigint
);
