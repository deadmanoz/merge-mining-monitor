-- 0034_retire_near_events.sql
--
-- Finishes the child-block split that 0033 started: merge_mining_event keeps
-- only Bitcoin-relevant evidence (canonical, stale, error_block, unknown), and
-- a near parent lives only on its child_block_observation. The event, RSK
-- sidecar and attribution tables are copied into their slim shapes and
-- swapped in, keeping every id, rather than deleting near rows in place.
--
-- After this migration:
-- - merge_mining_event holds no near row and its kind CHECK rejects one. It
--   drops the child columns the observation now holds (child_block_time,
--   child_header_bytes, child_nbits, the child coinbase, the child target
--   verdict and displacement) and child_miner_pool_id, and every row links its
--   observation through a NOT NULL, unique observation_id.
-- - rsk_merge_mining_evidence keeps the RSK proof only (merge_mining_hash,
--   merkle_proof, coinbase_tail, proof_format); the block hash, height, uncle
--   placement, miner and pool identity are on the observation.
-- - event_pool_attribution keeps the rows of the remaining events.
-- - An observation whose only event was a revoked near becomes
--   unverified_parent: the child block stays for its chain's cadence, and its
--   parent fields are cleared, since a near can no longer be revoked.
-- - An RSK near whose merge-mining proof shows a coinbase-only parent (an
--   empty hash list, or the early one-transaction partial merkle tree
--   01000000 01 <root> 01 01) records the header's merkle root as its parent
--   coinbase txid, as live capture does, so empty templates are detectable on
--   every chain.
-- - source_health_ready is FALSE: /api/v1/sources fails closed until the
--   window's `just rebuild-source-health` runs.
--
-- Disk prerequisite: before the old tables (about 44 GB) are dropped at
-- commit, this transaction writes the slim copies (about 2.5 GB), a temporary
-- event-to-observation link (about 1 GB, truncated once the slim event table
-- is built), and new row versions for the converted and filled observations
-- (about 0.5 GB per million rows); all are estimates until the rehearsal
-- measures the peak. Do not start the window unless free space exceeds that
-- peak plus the wrapper's pre-migration archive with a 10% margin.
--
-- Writers must be stopped (stop, migrate, start). The SHARE locks keep a
-- writer that is still running from changing the copied tables; the drops
-- below then take them exclusively.
LOCK TABLE merge_mining_event, rsk_merge_mining_evidence, event_pool_attribution,
           child_block_observation, hathor_merge_mining_evidence,
           historical_event_provenance
    IN SHARE MODE;

-- child_block_observation and merge_mining_event.observation_id are new: the
-- production wrapper (the deploy repo's migrate-safe.sh) applies the pending 0033
-- and 0034 as one transaction, and elsewhere autovacuum may not have analyzed them
-- yet. Without statistics the planner would misjudge the link's two branches below.
ANALYZE child_block_observation;
ANALYZE merge_mining_event (observation_id);

-- Each event and its observation: through observation_id, or through the equal
-- id 0033 gave the events it filled. An observation of another block leaves
-- observation_id NULL here, which the check below refuses.
CREATE TEMPORARY TABLE migration_0034_link ON COMMIT DROP AS
SELECT e.id AS event_id, o.id AS observation_id, e.source_id, e.btc_parent_kind,
       e.revoked_at IS NOT NULL AS revoked, e.child_block_hash IS NULL AS hashless
  FROM merge_mining_event e
  LEFT JOIN child_block_observation o
    ON o.id = e.id
   AND o.source_id = e.source_id
   AND (e.child_block_hash IS NULL OR o.child_block_hash = e.child_block_hash)
 WHERE e.observation_id IS NULL
UNION ALL
SELECT e.id, o.id, e.source_id, e.btc_parent_kind,
       e.revoked_at IS NOT NULL, e.child_block_hash IS NULL
  FROM merge_mining_event e
  LEFT JOIN child_block_observation o
    ON o.id = e.observation_id
   AND o.source_id = e.source_id
   AND (e.child_block_hash IS NULL OR o.child_block_hash = e.child_block_hash)
 WHERE e.observation_id IS NOT NULL;
ANALYZE migration_0034_link;

-- One row of typed counts: a misspelt column fails instead of reading NULL.
CREATE TEMPORARY TABLE migration_0034_count ON COMMIT DROP AS
SELECT events.near AS near_events,
       events.unlinked AS unlinked_events,
       events.hashless_revoked_nears,
       0::bigint AS revoked_nears_converted,
       0::bigint AS rsk_near_coinbase_txids_filled,
       0::bigint AS hathor_near_sidecars_removed,
       0::bigint AS near_provenance_removed,
       observations.total AS observations_before,
       observations.near AS near_observations_before,
       observations.unverified AS unverified_observations_before
  FROM (
      SELECT count(*) FILTER (WHERE btc_parent_kind = 'near') AS near,
             count(*) FILTER (WHERE observation_id IS NULL) AS unlinked,
             -- unverified_parent needs a child hash; no live source captures a hashless near.
             count(*) FILTER (WHERE btc_parent_kind = 'near' AND revoked AND hashless)
                 AS hashless_revoked_nears
        FROM migration_0034_link
  ) events
 CROSS JOIN (
      SELECT count(*) AS total,
             count(*) FILTER (WHERE observation_kind = 'near') AS near,
             count(*) FILTER (WHERE observation_kind = 'unverified_parent') AS unverified
        FROM child_block_observation
  ) observations;

CREATE TEMPORARY TABLE migration_0034_evidence_before ON COMMIT DROP AS
SELECT source_id, btc_parent_kind, count(*) AS events
  FROM migration_0034_link
 WHERE btc_parent_kind <> 'near'
 GROUP BY source_id, btc_parent_kind;

DO $$
DECLARE
    c migration_0034_count%ROWTYPE;
BEGIN
    SELECT * INTO c FROM migration_0034_count;
    IF c.unlinked_events > 0 THEN
        RAISE EXCEPTION
            'migration 0034: % events have no matching child_block_observation', c.unlinked_events;
    END IF;
    IF c.hashless_revoked_nears > 0 THEN
        RAISE EXCEPTION
            'migration 0034: % revoked near events have no child hash to keep as unverified_parent',
            c.hashless_revoked_nears;
    END IF;
END
$$;

WITH converted AS (
    UPDATE child_block_observation o
       SET observation_kind = 'unverified_parent',
           btc_parent_header_hash = NULL,
           btc_parent_header_bytes = NULL,
           btc_parent_header_time = NULL,
           btc_parent_coinbase_txid = NULL,
           btc_parent_coinbase_script = NULL,
           btc_parent_coinbase_outputs = NULL,
           btc_parent_pool_id = NULL
      FROM migration_0034_link l
     WHERE o.id = l.observation_id
       AND l.btc_parent_kind = 'near'
       AND l.revoked
       AND o.observation_kind = 'near'
    RETURNING 1
)
UPDATE migration_0034_count SET revoked_nears_converted = (SELECT count(*) FROM converted);

-- Only RSK events have this sidecar.
WITH filled AS (
    UPDATE child_block_observation o
       SET btc_parent_coinbase_txid = substring(o.btc_parent_header_bytes FROM 37 FOR 32)
      FROM rsk_merge_mining_evidence r
      JOIN migration_0034_link l ON l.event_id = r.event_id
     WHERE o.id = l.observation_id
       AND l.btc_parent_kind = 'near'
       AND o.observation_kind = 'near'
       AND o.btc_parent_coinbase_txid IS NULL
       AND (octet_length(r.merkle_proof) = 0
            OR (octet_length(r.merkle_proof) = 39
                AND substring(r.merkle_proof FROM 1 FOR 5) = '\x0100000001'::bytea
                AND substring(r.merkle_proof FROM 38 FOR 2) = '\x0101'::bytea
                AND substring(r.merkle_proof FROM 6 FOR 32)
                    = substring(o.btc_parent_header_bytes FROM 37 FOR 32)))
    RETURNING 1
)
UPDATE migration_0034_count SET rsk_near_coinbase_txids_filled = (SELECT count(*) FROM filled);

CREATE TABLE merge_mining_event_slim (
    id                                BIGINT  GENERATED ALWAYS AS IDENTITY,
    source_id                         BIGINT  NOT NULL,
    child_height                      INTEGER,
    child_block_hash                  BYTEA   CONSTRAINT merge_mining_event_child_block_hash_check
                                              CHECK (octet_length(child_block_hash) = 32),
    btc_parent_header_hash            BYTEA   NOT NULL
                                              CONSTRAINT merge_mining_event_btc_parent_header_hash_check
                                              CHECK (octet_length(btc_parent_header_hash) = 32),
    btc_parent_header_bytes           BYTEA   NOT NULL
                                              CONSTRAINT merge_mining_event_btc_parent_header_bytes_check
                                              CHECK (octet_length(btc_parent_header_bytes) = 80),
    btc_parent_header_time            BIGINT  NOT NULL,
    btc_parent_height                 INTEGER,
    btc_parent_kind                   TEXT    NOT NULL
                                              CONSTRAINT merge_mining_event_btc_parent_kind_check
                                              CHECK (btc_parent_kind IN ('canonical', 'stale', 'error_block', 'unknown')),
    pow_validates_btc_target          BOOLEAN NOT NULL,
    difficulty_epoch_ok               BOOLEAN,
    btc_parent_coinbase_txid          BYTEA   CONSTRAINT merge_mining_event_btc_parent_coinbase_txid_check
                                              CHECK (octet_length(btc_parent_coinbase_txid) = 32),
    btc_parent_coinbase_script        BYTEA,
    btc_parent_coinbase_outputs       BYTEA,
    aux_merkle_proof                  BYTEA,
    discovered_at                     BIGINT  NOT NULL,
    confirmed_at                      BIGINT  NOT NULL,
    revoked_at                        BIGINT,
    revocation_reason                 TEXT,
    btc_parent_prev_header_hash       BYTEA   NOT NULL
                                              CONSTRAINT merge_mining_event_btc_parent_prev_header_hash_check
                                              CHECK (octet_length(btc_parent_prev_header_hash) = 32),
    btc_parent_coinbase_outputs_text  TEXT,
    btc_parent_coinbase_tx_bytes      BYTEA,
    observation_id                    BIGINT  NOT NULL,

    CONSTRAINT merge_mining_event_child_identity_present
        CHECK (child_block_hash IS NOT NULL OR child_height IS NOT NULL),
    CONSTRAINT chk_mme_parent_kind_consistency
        CHECK (btc_parent_kind = 'unknown'
               OR (btc_parent_height IS NOT NULL AND pow_validates_btc_target))
);

INSERT INTO merge_mining_event_slim (
    id, source_id, child_height, child_block_hash,
    btc_parent_header_hash, btc_parent_header_bytes, btc_parent_header_time,
    btc_parent_height, btc_parent_kind, pow_validates_btc_target, difficulty_epoch_ok,
    btc_parent_coinbase_txid, btc_parent_coinbase_script, btc_parent_coinbase_outputs,
    aux_merkle_proof, discovered_at, confirmed_at, revoked_at, revocation_reason,
    btc_parent_prev_header_hash, btc_parent_coinbase_outputs_text,
    btc_parent_coinbase_tx_bytes, observation_id
)
OVERRIDING SYSTEM VALUE
SELECT e.id, e.source_id, e.child_height, e.child_block_hash,
       e.btc_parent_header_hash, e.btc_parent_header_bytes, e.btc_parent_header_time,
       e.btc_parent_height, e.btc_parent_kind, e.pow_validates_btc_target,
       e.difficulty_epoch_ok,
       e.btc_parent_coinbase_txid, e.btc_parent_coinbase_script,
       e.btc_parent_coinbase_outputs,
       e.aux_merkle_proof, e.discovered_at, e.confirmed_at, e.revoked_at,
       e.revocation_reason,
       e.btc_parent_prev_header_hash, e.btc_parent_coinbase_outputs_text,
       e.btc_parent_coinbase_tx_bytes, l.observation_id
  FROM migration_0034_link l
  JOIN merge_mining_event e ON e.id = l.event_id
 WHERE l.btc_parent_kind <> 'near';

-- The link's last use; truncating returns its space before the larger copies.
TRUNCATE migration_0034_link;

CREATE TABLE rsk_merge_mining_evidence_slim (
    id                 BIGINT GENERATED ALWAYS AS IDENTITY,
    event_id           BIGINT NOT NULL,
    merge_mining_hash  BYTEA  NOT NULL
                              CONSTRAINT rsk_merge_mining_evidence_merge_mining_hash_check
                              CHECK (octet_length(merge_mining_hash) = 32),
    merkle_proof       BYTEA,
    coinbase_tail      BYTEA,
    proof_format       TEXT   NOT NULL
                              CONSTRAINT rsk_merge_mining_evidence_proof_format_check
                              CHECK (proof_format = 'rskj_rpc_opaque')
);

INSERT INTO rsk_merge_mining_evidence_slim (
    id, event_id, merge_mining_hash, merkle_proof, coinbase_tail, proof_format
)
OVERRIDING SYSTEM VALUE
SELECT r.id, r.event_id, r.merge_mining_hash, r.merkle_proof, r.coinbase_tail, r.proof_format
  FROM rsk_merge_mining_evidence r
  JOIN merge_mining_event_slim e ON e.id = r.event_id;

CREATE TABLE event_pool_attribution_slim (
    id               BIGINT GENERATED ALWAYS AS IDENTITY,
    event_id         BIGINT NOT NULL,
    side             TEXT   NOT NULL
                            CONSTRAINT event_pool_attribution_side_check
                            CHECK (side IN ('btc_parent', 'child_block')),
    namespace        TEXT   NOT NULL,
    match_kind       TEXT   NOT NULL,
    matched_value    TEXT   NOT NULL,
    pool_id          BIGINT,
    pool_identity_id BIGINT,
    source           TEXT   NOT NULL,
    confidence       TEXT   NOT NULL
                            CONSTRAINT event_pool_attribution_confidence_check
                            CHECK (confidence IN ('high', 'medium', 'low')),
    details          JSONB  NOT NULL DEFAULT '{}'::jsonb,
    first_seen_at    BIGINT NOT NULL,
    last_seen_at     BIGINT NOT NULL,

    CONSTRAINT event_pool_attribution_pool_identity_consistency
        CHECK (pool_identity_id IS NULL OR pool_id IS NOT NULL)
);

INSERT INTO event_pool_attribution_slim (
    id, event_id, side, namespace, match_kind, matched_value, pool_id,
    pool_identity_id, source, confidence, details, first_seen_at, last_seen_at
)
OVERRIDING SYSTEM VALUE
SELECT a.id, a.event_id, a.side, a.namespace, a.match_kind, a.matched_value, a.pool_id,
       a.pool_identity_id, a.source, a.confidence, a.details, a.first_seen_at,
       a.last_seen_at
  FROM event_pool_attribution a
  JOIN merge_mining_event_slim e ON e.id = a.event_id;

-- Each new identity sequence continues past the old one and every old id, near ids
-- included, so a removed id is never reused.
SELECT setval(pg_get_serial_sequence('merge_mining_event_slim', 'id'),
              GREATEST(nextval(pg_get_serial_sequence('merge_mining_event', 'id')),
                       (SELECT COALESCE(max(id), 0) + 1 FROM merge_mining_event)),
              false);
SELECT setval(pg_get_serial_sequence('rsk_merge_mining_evidence_slim', 'id'),
              GREATEST(nextval(pg_get_serial_sequence('rsk_merge_mining_evidence', 'id')),
                       (SELECT COALESCE(max(id), 0) + 1 FROM rsk_merge_mining_evidence)),
              false);
SELECT setval(pg_get_serial_sequence('event_pool_attribution_slim', 'id'),
              GREATEST(nextval(pg_get_serial_sequence('event_pool_attribution', 'id')),
                       (SELECT COALESCE(max(id), 0) + 1 FROM event_pool_attribution)),
              false);

-- Sidecar and provenance rows of near events go with them.
WITH removed AS (
    DELETE FROM hathor_merge_mining_evidence h
     WHERE NOT EXISTS (SELECT 1 FROM merge_mining_event_slim e WHERE e.id = h.event_id)
    RETURNING 1
)
UPDATE migration_0034_count SET hathor_near_sidecars_removed = (SELECT count(*) FROM removed);
WITH removed AS (
    DELETE FROM historical_event_provenance p
     WHERE NOT EXISTS (SELECT 1 FROM merge_mining_event_slim e WHERE e.id = p.event_id)
    RETURNING 1
)
UPDATE migration_0034_count SET near_provenance_removed = (SELECT count(*) FROM removed);

ALTER TABLE hathor_merge_mining_evidence
    DROP CONSTRAINT hathor_merge_mining_evidence_event_id_fkey;
ALTER TABLE historical_event_provenance
    DROP CONSTRAINT historical_event_provenance_event_id_fkey;

DROP TABLE event_pool_attribution;
DROP TABLE rsk_merge_mining_evidence;
DROP TABLE merge_mining_event;

ALTER TABLE merge_mining_event_slim RENAME TO merge_mining_event;
ALTER TABLE rsk_merge_mining_evidence_slim RENAME TO rsk_merge_mining_evidence;
ALTER TABLE event_pool_attribution_slim RENAME TO event_pool_attribution;
ALTER SEQUENCE merge_mining_event_slim_id_seq RENAME TO merge_mining_event_id_seq;
ALTER SEQUENCE rsk_merge_mining_evidence_slim_id_seq RENAME TO rsk_merge_mining_evidence_id_seq;
ALTER SEQUENCE event_pool_attribution_slim_id_seq RENAME TO event_pool_attribution_id_seq;

ALTER TABLE merge_mining_event
    ADD CONSTRAINT merge_mining_event_pkey PRIMARY KEY (id),
    ADD CONSTRAINT merge_mining_event_source_id_fkey
        FOREIGN KEY (source_id) REFERENCES source(id),
    ADD CONSTRAINT merge_mining_event_observation_id_fkey
        FOREIGN KEY (observation_id) REFERENCES child_block_observation(id),
    ADD CONSTRAINT merge_mining_event_observation_id_unique UNIQUE (observation_id);

CREATE UNIQUE INDEX merge_mining_event_exact_child_identity
    ON merge_mining_event (source_id, child_block_hash)
    WHERE child_block_hash IS NOT NULL;
CREATE UNIQUE INDEX merge_mining_event_partial_child_identity
    ON merge_mining_event (source_id, child_height, btc_parent_header_hash)
    WHERE child_block_hash IS NULL AND child_height IS NOT NULL;
CREATE INDEX idx_mme_btc_kind
    ON merge_mining_event (btc_parent_kind, btc_parent_height)
    WHERE revoked_at IS NULL;
CREATE INDEX idx_mme_btc_parent
    ON merge_mining_event (btc_parent_header_hash, source_id, child_height, id);
CREATE INDEX idx_mme_parent_prev
    ON merge_mining_event (btc_parent_prev_header_hash, child_height, id)
    WHERE pow_validates_btc_target AND revoked_at IS NULL;
CREATE INDEX idx_mme_source_height
    ON merge_mining_event (source_id, child_height);

COMMENT ON COLUMN merge_mining_event.observation_id IS
  'The child_block_observation holding this event''s child block: its child fields, chain role, identities and child pool.';

ALTER TABLE rsk_merge_mining_evidence
    ADD CONSTRAINT rsk_merge_mining_evidence_pkey PRIMARY KEY (id),
    ADD CONSTRAINT rsk_evidence_event_unique UNIQUE (event_id),
    ADD CONSTRAINT rsk_merge_mining_evidence_event_id_fkey
        FOREIGN KEY (event_id) REFERENCES merge_mining_event(id) ON DELETE CASCADE;

ALTER TABLE event_pool_attribution
    ADD CONSTRAINT event_pool_attribution_pkey PRIMARY KEY (id),
    ADD CONSTRAINT event_pool_attribution_side_namespace_value_unique
        UNIQUE (event_id, side, namespace, matched_value),
    ADD CONSTRAINT event_pool_attribution_event_id_fkey
        FOREIGN KEY (event_id) REFERENCES merge_mining_event(id) ON DELETE CASCADE,
    ADD CONSTRAINT event_pool_attribution_pool_id_fkey
        FOREIGN KEY (pool_id) REFERENCES pool(id),
    ADD CONSTRAINT event_pool_attribution_pool_identity_id_fkey
        FOREIGN KEY (pool_identity_id) REFERENCES pool_identity(id);

CREATE INDEX idx_event_pool_attr_event_side
    ON event_pool_attribution (event_id, side);
CREATE INDEX idx_event_pool_attr_identity
    ON event_pool_attribution (pool_identity_id)
    WHERE pool_identity_id IS NOT NULL;
CREATE INDEX idx_event_pool_attr_namespace_value
    ON event_pool_attribution (namespace, matched_value);
CREATE INDEX idx_event_pool_attr_side_pool
    ON event_pool_attribution (side, pool_id)
    WHERE pool_id IS NOT NULL;

ALTER TABLE hathor_merge_mining_evidence
    ADD CONSTRAINT hathor_merge_mining_evidence_event_id_fkey
        FOREIGN KEY (event_id) REFERENCES merge_mining_event(id) ON DELETE CASCADE;
ALTER TABLE historical_event_provenance
    ADD CONSTRAINT historical_event_provenance_event_id_fkey
        FOREIGN KEY (event_id) REFERENCES merge_mining_event(id) ON DELETE CASCADE;

UPDATE read_model_invariant
   SET source_health_ready = FALSE,
       updated_at = extract(epoch FROM now())::bigint;

-- The rebuild that follows the window plans against these.
ANALYZE merge_mining_event, rsk_merge_mining_evidence, event_pool_attribution,
        child_block_observation;

DO $$
DECLARE
    c migration_0034_count%ROWTYPE;
    mismatched_kinds BIGINT;
    misfiled_links BIGINT;
    dangling_proof_ids BIGINT;
    events_after BIGINT;
    rsk_sidecars_after BIGINT;
    attributions_after BIGINT;
    observations_after BIGINT;
    near_after BIGINT;
    unverified_after BIGINT;
BEGIN
    SELECT * INTO c FROM migration_0034_count;

    SELECT count(*) INTO mismatched_kinds
      FROM (
          SELECT source_id, btc_parent_kind, count(*) AS events
            FROM merge_mining_event
           GROUP BY source_id, btc_parent_kind
      ) after
      FULL JOIN migration_0034_evidence_before before
        USING (source_id, btc_parent_kind)
     WHERE after.events IS DISTINCT FROM before.events;

    SELECT count(*) INTO misfiled_links
      FROM merge_mining_event e
      JOIN child_block_observation o ON o.id = e.observation_id
     WHERE o.observation_kind <> 'evidence';

    SELECT count(*) INTO dangling_proof_ids
      FROM attestation_proof p
     CROSS JOIN LATERAL jsonb_array_elements_text(p.evidence -> 'contributing_event_ids')
           AS contributing(event_id)
     WHERE NOT EXISTS (
         SELECT 1 FROM merge_mining_event e WHERE e.id = contributing.event_id::bigint
     );

    SELECT count(*) INTO events_after FROM merge_mining_event;
    SELECT count(*) INTO rsk_sidecars_after FROM rsk_merge_mining_evidence;
    SELECT count(*) INTO attributions_after FROM event_pool_attribution;
    SELECT count(*),
           count(*) FILTER (WHERE observation_kind = 'near'),
           count(*) FILTER (WHERE observation_kind = 'unverified_parent')
      INTO observations_after, near_after, unverified_after
      FROM child_block_observation;

    IF mismatched_kinds > 0
       OR misfiled_links > 0
       OR dangling_proof_ids > 0
       OR observations_after <> c.observations_before
       OR near_after <> c.near_observations_before - c.revoked_nears_converted
       OR unverified_after <> c.unverified_observations_before + c.revoked_nears_converted THEN
        RAISE EXCEPTION
            'migration 0034 swap check failed: % (source, kind) evidence counts differ, % events link a non-evidence observation, % proof event ids dangle, observations % -> % (near % -> %, unverified % -> %, % converted)',
            mismatched_kinds, misfiled_links, dangling_proof_ids,
            c.observations_before, observations_after,
            c.near_observations_before, near_after,
            c.unverified_observations_before, unverified_after,
            c.revoked_nears_converted;
    END IF;

    RAISE NOTICE
        '0034: removed % near events; converted % revoked-near observations to unverified_parent; filled % RSK near coinbase txids; kept % events, % RSK sidecars, % attributions; removed % Hathor near sidecars and % near provenance rows',
        c.near_events, c.revoked_nears_converted, c.rsk_near_coinbase_txids_filled,
        events_after, rsk_sidecars_after, attributions_after,
        c.hathor_near_sidecars_removed, c.near_provenance_removed;
END
$$;
