-- 0033_add_child_block_observation.sql
--
-- Adds `child_block_observation`, one row per observed child block on every
-- chain, and `child_miner_identity`, the raw child identifiers those blocks
-- carry (RSK miner, child payout and reward addresses, Elastos minerinfo).
--
-- An observation holds the child block's own facts. Its `observation_kind`
-- says where the Bitcoin side lives: `evidence` when a `merge_mining_event`
-- holds the parent's Bitcoin classification, `near` when the parent misses
-- Bitcoin's target (the near parent's header and coinbase are kept here
-- because no event will hold them), `not_merge_mined` for a block without
-- AuxPoW, `non_bitcoin_parent` for a block whose AuxPoW parent failed the
-- Bitcoin lineage gate (no parent field is ever stored for it), and
-- `unverified_parent` for an authenticated child block whose AuxPoW proof is
-- malformed or incomplete, so its parent cannot be checked (no parent field).
--
-- Every existing event gets one observation whose `id` equals the event's id.
-- That equality is used only by this migration's checks and by the later swap
-- migration, which sets `merge_mining_event.observation_id` while it rewrites
-- the event table; filling the column here would rewrite all 23M event rows.
-- Events written after this migration link their observation through
-- `observation_id` directly. Near events and the event-side child columns stay
-- until that swap.

-- Disk prerequisite: the new table and its indexes are written beside the
-- existing ones inside this transaction (roughly 12-14 GB for production's 23M
-- events, an estimate until the rehearsal on a production copy measures it), on
-- top of the wrapper's pre-migration archive. Do not start the window unless free
-- space exceeds the rehearsal's measured peak plus that archive with a 10% margin;
-- release older on-host archives first. Running out of space mid-transaction
-- rolls the whole migration back after a long run.
--
-- Writers must be stopped for this migration (stop, migrate, start), as the
-- production wrapper's drain does: an event written by an older binary after
-- it commits would have no observation. The SHARE locks below keep a writer
-- that is still running from changing the copied tables mid-migration; reads
-- continue.
LOCK TABLE merge_mining_event, rsk_merge_mining_evidence, event_pool_attribution IN SHARE MODE;

CREATE TABLE child_miner_identity (
    id          BIGINT GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    namespace   TEXT NOT NULL,
    identifier  TEXT NOT NULL,

    CONSTRAINT child_miner_identity_namespace_identifier_unique
        UNIQUE (namespace, identifier)
);

COMMENT ON TABLE child_miner_identity IS
  'Raw child-side identifiers seen on any child block, keyed like pool_identity (namespace, identifier). A pool_identity row with the same key maps it to a pool; unmapped identifiers stay here so a registry addition re-attributes without rewriting observations.';

CREATE TABLE child_block_observation (
    id                           BIGINT GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    source_id                    BIGINT  NOT NULL REFERENCES source(id),
    child_height                 INTEGER,
    child_block_hash             BYTEA   CHECK (child_block_hash IS NULL OR octet_length(child_block_hash) = 32),
    observation_kind             TEXT    NOT NULL CHECK (
        observation_kind IN (
            'evidence', 'near', 'not_merge_mined', 'non_bitcoin_parent', 'unverified_parent'
        )
    ),

    child_block_time             BIGINT,
    child_header_bytes           BYTEA   CHECK (child_header_bytes IS NULL OR octet_length(child_header_bytes) = 80),
    child_nbits                  BIGINT  CHECK (child_nbits IS NULL OR child_nbits BETWEEN 0 AND 4294967295),
    child_prev_block_hash        BYTEA   CHECK (child_prev_block_hash IS NULL OR octet_length(child_prev_block_hash) = 32),
    child_difficulty_reported    NUMERIC CHECK (child_difficulty_reported IS NULL OR child_difficulty_reported >= 0),
    child_coinbase_txid          BYTEA   CHECK (child_coinbase_txid IS NULL OR octet_length(child_coinbase_txid) = 32),
    child_coinbase_script        BYTEA,
    child_coinbase_outputs       BYTEA,
    pow_validates_child_target   BOOLEAN,
    child_chain_role             TEXT    CHECK (child_chain_role IS NULL OR child_chain_role IN ('canonical', 'uncle')),
    uncle_index                  INTEGER,
    uncle_parent_height          INTEGER,
    child_displaced_at           BIGINT,
    child_displaced_by           BYTEA   CHECK (child_displaced_by IS NULL OR octet_length(child_displaced_by) = 32),

    btc_parent_header_hash       BYTEA   CHECK (btc_parent_header_hash IS NULL OR octet_length(btc_parent_header_hash) = 32),
    btc_parent_header_bytes      BYTEA   CHECK (btc_parent_header_bytes IS NULL OR octet_length(btc_parent_header_bytes) = 80),
    btc_parent_header_time       BIGINT,
    btc_parent_coinbase_txid     BYTEA   CHECK (btc_parent_coinbase_txid IS NULL OR octet_length(btc_parent_coinbase_txid) = 32),
    btc_parent_coinbase_script   BYTEA,
    btc_parent_coinbase_outputs  BYTEA,

    child_identity_ids           BIGINT[] NOT NULL DEFAULT '{}',
    child_pool_id                BIGINT  REFERENCES pool(id),
    btc_parent_pool_id           BIGINT  REFERENCES pool(id),
    recorded_at                  BIGINT  NOT NULL,

    CONSTRAINT chk_cbo_child_identity_present CHECK (
        child_block_hash IS NOT NULL OR child_height IS NOT NULL
    ),
    CONSTRAINT chk_cbo_parent_by_kind CHECK (
        CASE observation_kind
            WHEN 'evidence' THEN btc_parent_header_hash IS NOT NULL
                AND btc_parent_header_bytes IS NULL
                AND btc_parent_header_time IS NULL
                AND btc_parent_coinbase_txid IS NULL
                AND btc_parent_coinbase_script IS NULL
                AND btc_parent_coinbase_outputs IS NULL
                AND btc_parent_pool_id IS NULL
            WHEN 'near' THEN btc_parent_header_hash IS NOT NULL
                AND btc_parent_header_bytes IS NOT NULL
                AND btc_parent_header_time IS NOT NULL
            ELSE child_block_hash IS NOT NULL
                AND btc_parent_header_hash IS NULL
                AND btc_parent_header_bytes IS NULL
                AND btc_parent_header_time IS NULL
                AND btc_parent_coinbase_txid IS NULL
                AND btc_parent_coinbase_script IS NULL
                AND btc_parent_coinbase_outputs IS NULL
                AND btc_parent_pool_id IS NULL
        END
    ),
    CONSTRAINT chk_cbo_uncle_consistency CHECK (
        CASE WHEN child_chain_role = 'uncle'
            THEN uncle_index IS NOT NULL AND uncle_parent_height IS NOT NULL
            ELSE uncle_index IS NULL AND uncle_parent_height IS NULL
        END
    ),
    CONSTRAINT chk_cbo_child_displacement_pair CHECK (
        (child_displaced_at IS NULL) = (child_displaced_by IS NULL)
    ),
    CONSTRAINT chk_cbo_child_displaced_by_other_block CHECK (
        child_displaced_by IS NULL
        OR child_block_hash IS NULL
        OR child_displaced_by <> child_block_hash
    )
);

COMMENT ON TABLE child_block_observation IS
  'One row per observed child block on every chain. Bitcoin-relevant parents (canonical, stale, error_block, unknown) keep their classification and proof on merge_mining_event, linked by merge_mining_event.observation_id; near parents keep their header and coinbase here.';
COMMENT ON COLUMN child_block_observation.child_prev_block_hash IS
  'Internal-order hash of the child block''s predecessor, from the child header or the child node''s block response; NULL when the capture did not carry it.';
COMMENT ON COLUMN child_block_observation.child_difficulty_reported IS
  'Child difficulty as the child node reported it (RSK); a reported claim, not authenticated evidence.';
COMMENT ON COLUMN child_block_observation.child_identity_ids IS
  'Sorted child_miner_identity ids this block''s child coinbase or miner field names. A Syscoin block often pays several addresses; an Elastos block carries a reward address and a minerinfo.';
COMMENT ON COLUMN child_block_observation.child_pool_id IS
  'The unique known pool among this block''s child-side matches, else NULL.';
COMMENT ON COLUMN child_block_observation.btc_parent_pool_id IS
  'The unique known pool among a near parent''s coinbase matches, else NULL; evidence rows keep parent attribution on event_pool_attribution.';
COMMENT ON COLUMN child_block_observation.recorded_at IS
  'Epoch seconds when the monitor first recorded the block: the event''s discovered_at for rows migration 0033 filled from events, the chain head''s observed_at for rows it filled from child_chain_head.';

INSERT INTO child_miner_identity (namespace, identifier)
SELECT namespace, matched_value
  FROM event_pool_attribution
 WHERE side = 'child_block'
   AND namespace NOT LIKE 'btc\_%'
UNION
SELECT 'rsk_miner_address', encode(rsk_miner, 'hex')
  FROM rsk_merge_mining_evidence;

INSERT INTO child_block_observation (
    id, source_id, child_height, child_block_hash, observation_kind,
    child_block_time, child_header_bytes, child_nbits, child_prev_block_hash,
    child_coinbase_txid, child_coinbase_script, child_coinbase_outputs,
    pow_validates_child_target, child_chain_role, uncle_index, uncle_parent_height,
    child_displaced_at, child_displaced_by,
    btc_parent_header_hash, btc_parent_header_bytes, btc_parent_header_time,
    btc_parent_coinbase_txid, btc_parent_coinbase_script, btc_parent_coinbase_outputs,
    child_identity_ids, child_pool_id, btc_parent_pool_id, recorded_at
)
OVERRIDING SYSTEM VALUE
SELECT
    e.id, e.source_id, e.child_height, e.child_block_hash,
    CASE WHEN e.btc_parent_kind = 'near' THEN 'near' ELSE 'evidence' END,
    e.child_block_time, e.child_header_bytes, e.child_nbits,
    substring(e.child_header_bytes FROM 5 FOR 32),
    e.child_coinbase_txid, e.child_coinbase_script, e.child_coinbase_outputs,
    e.pow_validates_child_target,
    CASE WHEN r.event_id IS NULL THEN NULL WHEN r.is_uncle THEN 'uncle' ELSE 'canonical' END,
    r.uncle_index, r.uncle_parent_height,
    e.child_displaced_at, e.child_displaced_by,
    e.btc_parent_header_hash,
    CASE WHEN e.btc_parent_kind = 'near' THEN e.btc_parent_header_bytes END,
    CASE WHEN e.btc_parent_kind = 'near' THEN e.btc_parent_header_time END,
    CASE WHEN e.btc_parent_kind = 'near' THEN e.btc_parent_coinbase_txid END,
    CASE WHEN e.btc_parent_kind = 'near' THEN e.btc_parent_coinbase_script END,
    CASE WHEN e.btc_parent_kind = 'near' THEN e.btc_parent_coinbase_outputs END,
    COALESCE(identities.ids, '{}'),
    e.child_miner_pool_id,
    CASE WHEN e.btc_parent_kind = 'near' THEN parent_pool.pool_id END,
    e.discovered_at
  FROM merge_mining_event e
  LEFT JOIN rsk_merge_mining_evidence r ON r.event_id = e.id
  LEFT JOIN (
      SELECT named.event_id, array_agg(DISTINCT ci.id ORDER BY ci.id) AS ids
        FROM (
            SELECT a.event_id, a.namespace, a.matched_value AS identifier
              FROM event_pool_attribution a
             WHERE a.side = 'child_block'
               AND a.namespace NOT LIKE 'btc\_%'
            UNION ALL
            SELECT s.event_id, 'rsk_miner_address', encode(s.rsk_miner, 'hex')
              FROM rsk_merge_mining_evidence s
        ) named
        JOIN child_miner_identity ci
          ON ci.namespace = named.namespace AND ci.identifier = named.identifier
       GROUP BY named.event_id
  ) identities ON identities.event_id = e.id
  LEFT JOIN (
      SELECT a.event_id,
             CASE WHEN count(DISTINCT a.pool_id) = 1 THEN min(a.pool_id) END AS pool_id
        FROM event_pool_attribution a
        JOIN merge_mining_event near_event
          ON near_event.id = a.event_id AND near_event.btc_parent_kind = 'near'
       WHERE a.side = 'btc_parent'
         AND a.pool_id IS NOT NULL
       GROUP BY a.event_id
  ) parent_pool ON parent_pool.event_id = e.id;

SELECT setval(
    pg_get_serial_sequence('child_block_observation', 'id'),
    COALESCE((SELECT max(id) FROM merge_mining_event), 0) + 1,
    false
);

DO $$
DECLARE
    missing_observations BIGINT;
    mismatched_observations BIGINT;
    unlinked_rsk_miners BIGINT;
BEGIN
    SELECT count(*) FILTER (WHERE o.id IS NULL),
           count(*) FILTER (
               WHERE o.id IS NOT NULL AND (o.source_id <> e.source_id
                  OR o.child_block_hash IS DISTINCT FROM e.child_block_hash
                  OR o.child_height IS DISTINCT FROM e.child_height
                  OR o.observation_kind
                     <> CASE WHEN e.btc_parent_kind = 'near' THEN 'near' ELSE 'evidence' END))
      INTO missing_observations, mismatched_observations
      FROM merge_mining_event e
      LEFT JOIN child_block_observation o ON o.id = e.id;

    SELECT count(*)
      INTO unlinked_rsk_miners
      FROM rsk_merge_mining_evidence r
      JOIN child_block_observation o ON o.id = r.event_id
      JOIN child_miner_identity ci
        ON ci.namespace = 'rsk_miner_address' AND ci.identifier = encode(r.rsk_miner, 'hex')
     WHERE NOT (ci.id = ANY (o.child_identity_ids));

    IF missing_observations > 0 OR mismatched_observations > 0 OR unlinked_rsk_miners > 0 THEN
        RAISE EXCEPTION
            'migration 0033 observation fill is incomplete: % events without an observation, % mismatched, % RSK miners unlinked',
            missing_observations, mismatched_observations, unlinked_rsk_miners;
    END IF;
END
$$;

CREATE UNIQUE INDEX child_block_observation_exact_child_identity
    ON child_block_observation (source_id, child_block_hash)
    WHERE child_block_hash IS NOT NULL;
CREATE UNIQUE INDEX child_block_observation_partial_child_identity
    ON child_block_observation (source_id, child_height, btc_parent_header_hash)
    WHERE child_block_hash IS NULL AND child_height IS NOT NULL;
CREATE INDEX idx_cbo_source_height
    ON child_block_observation (source_id, child_height);
CREATE INDEX idx_cbo_near_parent
    ON child_block_observation (btc_parent_header_hash, source_id)
    WHERE observation_kind = 'near';

-- Bitcoind-family blocks recorded before this migration without an event
-- exist only as child_chain_head rows, and a final head also stops a rescan
-- from capturing the block again. Give each its child-side observation: for
-- this family `non_auxpow` is a block without AuxPoW and `recorded` a parent
-- the lineage gate refused. Elastos and Hathor `recorded` heads (a near or a
-- refused parent) stay as they are: a near observation needs the parent
-- header, which a head does not keep.
INSERT INTO child_block_observation (
    source_id, child_height, child_block_hash, observation_kind, recorded_at
)
SELECT h.source_id, h.child_height, h.block_hash,
       CASE h.outcome WHEN 'non_auxpow' THEN 'not_merge_mined' ELSE 'non_bitcoin_parent' END,
       h.observed_at
  FROM child_chain_head h
  JOIN source s ON s.id = h.source_id
 WHERE h.outcome IN ('non_auxpow', 'recorded')
   AND s.code IN ('auxpow:namecoin', 'auxpow:syscoin', 'auxpow:fractal',
                  'auxpow:qbit', 'auxpow:terracoin')
ON CONFLICT (source_id, child_block_hash) WHERE child_block_hash IS NOT NULL DO NOTHING;

ALTER TABLE merge_mining_event ADD COLUMN observation_id BIGINT;
ALTER TABLE merge_mining_event
    ADD CONSTRAINT merge_mining_event_observation_id_fkey
        FOREIGN KEY (observation_id) REFERENCES child_block_observation(id)
        NOT VALID;
CREATE UNIQUE INDEX merge_mining_event_observation_id_unique
    ON merge_mining_event (observation_id)
    WHERE observation_id IS NOT NULL;

COMMENT ON COLUMN merge_mining_event.observation_id IS
  'The child_block_observation holding this event''s child block. NULL only for events that predate migration 0033, whose observation has the same id until the swap migration sets this column.';
