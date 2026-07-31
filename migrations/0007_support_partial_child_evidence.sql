-- The previous identity allowed the same source/hash pair at several heights.
-- Exact child hashes are now source-global identities, so fail with an
-- actionable diagnostic before changing the schema if legacy data violates
-- that stronger contract. Never guess which observation is authoritative.
DO $$
DECLARE
    duplicate_exact_identities BIGINT;
BEGIN
    SELECT count(*)
      INTO duplicate_exact_identities
      FROM (
          SELECT source_id, child_block_hash
            FROM merge_mining_event
           WHERE child_block_hash IS NOT NULL
           GROUP BY source_id, child_block_hash
          HAVING count(*) > 1
      ) duplicates;

    IF duplicate_exact_identities > 0 THEN
        RAISE EXCEPTION
            'migration 0007 found % duplicate exact child identities',
            duplicate_exact_identities
            USING HINT =
                'Run the duplicate-identity audit query in docs/operations.md, resolve each source/hash conflict from authenticated evidence, then rerun the backup-first migration.';
    END IF;
END
$$;

ALTER TABLE merge_mining_event
    ALTER COLUMN child_height DROP NOT NULL,
    ALTER COLUMN child_block_hash DROP NOT NULL,
    ALTER COLUMN child_block_time DROP NOT NULL;

ALTER TABLE merge_mining_event
    ADD COLUMN child_header_bytes BYTEA
        CHECK (
            child_header_bytes IS NULL
            OR octet_length(child_header_bytes) = 80
        ),
    ADD COLUMN child_nbits BIGINT
        CHECK (
            child_nbits IS NULL
            OR child_nbits BETWEEN 0 AND 4294967295
        ),
    ADD COLUMN btc_parent_coinbase_outputs_text TEXT,
    ADD COLUMN btc_parent_coinbase_tx_bytes BYTEA;

ALTER TABLE merge_mining_event
    DROP CONSTRAINT merge_mining_event_source_id_child_height_child_block_hash_key,
    ADD CONSTRAINT merge_mining_event_child_identity_present
        CHECK (child_block_hash IS NOT NULL OR child_height IS NOT NULL);

CREATE UNIQUE INDEX merge_mining_event_exact_child_identity
    ON merge_mining_event (source_id, child_block_hash)
    WHERE child_block_hash IS NOT NULL;

CREATE UNIQUE INDEX merge_mining_event_partial_child_identity
    ON merge_mining_event (source_id, child_height, btc_parent_header_hash)
    WHERE child_block_hash IS NULL AND child_height IS NOT NULL;

CREATE TABLE historical_event_provenance (
    event_id               BIGINT NOT NULL
                           REFERENCES merge_mining_event(id) ON DELETE CASCADE,
    publication_ref        TEXT NOT NULL CHECK (
        publication_ref ~ '^[0-9a-f]{40}$'
        OR publication_ref = 'operator-csv'
    ),
    chain                  TEXT NOT NULL,
    source_kind            TEXT NOT NULL,
    source_path            TEXT NOT NULL,
    source_row_number      BIGINT NOT NULL CHECK (source_row_number > 0),
    artifact_scope         TEXT NOT NULL,
    provenance             TEXT NOT NULL,
    classification         TEXT NOT NULL CHECK (
        classification IN ('canonical', 'stale', 'stale_descendant', 'unknown')
    ),
    btc_height             INTEGER CHECK (btc_height IS NULL OR btc_height >= 0),
    validation_status      TEXT,
    btc_stale_relevance    TEXT CHECK (
        btc_stale_relevance IS NULL
        OR btc_stale_relevance IN ('strict_btc_orphan', 'weak_btc_orphan')
    ),
    relevance_reason       TEXT,

    PRIMARY KEY (publication_ref, chain, source_path, source_row_number)
);

CREATE INDEX historical_event_provenance_publication
    ON historical_event_provenance (publication_ref, chain);

CREATE INDEX historical_event_provenance_event
    ON historical_event_provenance (event_id);

-- Historical base rows are imported in one chain transaction, but parent
-- read-model reconciliation is deliberately drained afterwards in bounded
-- per-parent transactions. Persist both the primary work bit and the exact
-- changed-hash cascade seeds so an interruption at either boundary is
-- resumable. A new base mutation increments generation and preserves any
-- unconsumed cascade seeds from an earlier generation.
CREATE TABLE historical_reconcile_queue (
    btc_parent_header_hash BYTEA PRIMARY KEY
                           CHECK (octet_length(btc_parent_header_hash) = 32),
    primary_pending        BOOLEAN NOT NULL DEFAULT TRUE,
    changed_hashes         BYTEA[] NOT NULL DEFAULT ARRAY[]::BYTEA[],
    generation             BIGINT NOT NULL DEFAULT 1 CHECK (generation > 0),
    enqueued_at            TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at             TIMESTAMPTZ NOT NULL DEFAULT now()
);
