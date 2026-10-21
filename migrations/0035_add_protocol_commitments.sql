-- 0035_add_protocol_commitments.sql
--
-- Adds the derived tables for Bitcoin coinbase OP_RETURN declarations of Core DAO
-- delegated proof of work (CORE: validator and reward address) and exSat (synchronizer
-- account), decoded by `mmm_capture::protocol_commitment` and written only by
-- mmm-read-model. A declaration names a protocol account; it says nothing about who
-- built the block template or which account the protocol credited.
--
-- The tables start empty: `reconcile-read-model --rebuild-protocol-commitments` fills
-- them and then records the decoder version, and the API serves nothing until it has.

CREATE TABLE protocol_commitment_script (
    id                   BIGINT GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    protocol             TEXT     NOT NULL CHECK (protocol IN ('core', 'exsat')),
    script               BYTEA    NOT NULL,
    -- Coinbase outputs are unbounded, so identity is a digest an index entry always fits.
    script_sha256        BYTEA    GENERATED ALWAYS AS (sha256(script)) STORED UNIQUE,
    decode_status        TEXT     NOT NULL
                         CHECK (decode_status IN ('valid', 'malformed', 'unsupported_version')),
    malformed_reason     TEXT,
    version              SMALLINT,
    core_validator       BYTEA    CHECK (octet_length(core_validator) = 20),
    core_reward_address  BYTEA    CHECK (octet_length(core_reward_address) = 20),
    core_binding_hash    BYTEA    CHECK (octet_length(core_binding_hash) = 32),
    exsat_account        TEXT,
    -- The declared account as `pool_identity` keys it; NULL unless the script is valid.
    account_namespace    TEXT     GENERATED ALWAYS AS (
                             CASE WHEN core_reward_address IS NOT NULL THEN 'core_reward_address'
                                  WHEN exsat_account IS NOT NULL THEN 'exsat_account' END
                         ) STORED,
    account_identifier   TEXT     GENERATED ALWAYS AS (
                             COALESCE('0x' || encode(core_reward_address, 'hex'), exsat_account)
                         ) STORED,

    CONSTRAINT chk_pcs_malformed_reason CHECK (
        (decode_status = 'malformed') = (malformed_reason IS NOT NULL)
    ),
    CONSTRAINT chk_pcs_decoded_fields CHECK (
        CASE
            WHEN decode_status = 'valid' AND protocol = 'core' THEN
                core_validator IS NOT NULL AND core_reward_address IS NOT NULL
                AND exsat_account IS NULL
            WHEN decode_status = 'valid' AND protocol = 'exsat' THEN
                exsat_account IS NOT NULL AND core_validator IS NULL
                AND core_reward_address IS NULL AND core_binding_hash IS NULL
            ELSE
                core_validator IS NULL AND core_reward_address IS NULL
                AND core_binding_hash IS NULL AND exsat_account IS NULL
        END
    )
);

COMMENT ON TABLE protocol_commitment_script IS
  'One row per distinct CORE or exSat OP_RETURN script ever decoded, with its strict-layout status and decoded fields. Rows are never deleted; a decoder change rewrites the decoded columns in place.';

CREATE TABLE protocol_commitment (
    btc_header_hash    BYTEA   NOT NULL CHECK (octet_length(btc_header_hash) = 32),
    vout               INTEGER NOT NULL CHECK (vout >= 0),
    script_id          BIGINT  NOT NULL REFERENCES protocol_commitment_script(id),
    upstream_selected  BOOLEAN,
    btc_header_time    BIGINT  NOT NULL,
    evidence_source    TEXT    NOT NULL
                       CHECK (evidence_source IN ('bitcoin_core', 'auxpow_event', 'near_observation')),
    PRIMARY KEY (btc_header_hash, vout)
);

CREATE INDEX idx_protocol_commitment_script_time
    ON protocol_commitment (script_id, btc_header_time);

COMMENT ON TABLE protocol_commitment IS
  'Each CORE or exSat declaration in a Bitcoin header''s coinbase, decoded from the highest-priority stored outputs (Core''s block coinbase, else an active AuxPoW event''s parent coinbase, else a near observation''s). No foreign key to block: near parents have no block row.';
COMMENT ON COLUMN protocol_commitment.upstream_selected IS
  'CORE: whether Core DAO''s relayer parser takes this output as the block''s commitment (never vout 0). NULL for exSat, whose selection also depends on its registered payout addresses and cannot be decided from the coinbase.';

CREATE TABLE protocol_commitment_undecodable (
    btc_header_hash  BYTEA PRIMARY KEY CHECK (octet_length(btc_header_hash) = 32),
    evidence_source  TEXT  NOT NULL
                     CHECK (evidence_source IN ('bitcoin_core', 'auxpow_event', 'near_observation')),
    error            TEXT  NOT NULL
);

COMMENT ON TABLE protocol_commitment_undecodable IS
  'Headers whose chosen stored coinbase outputs do not deserialize, so a failed decode is never reported as a coinbase without declarations. Expected to stay empty.';

ALTER TABLE read_model_invariant
    ADD COLUMN protocol_commitments_decoder_version INTEGER;

COMMENT ON COLUMN read_model_invariant.protocol_commitments_decoder_version IS
  'The protocol-commitment decoder version the last complete rebuild used; the API serves protocol commitments only when it equals the running decoder''s version.';

-- Protocol-account associations share pool_identity with the child registries. Widen
-- only the namespace check.
ALTER TABLE pool_identity DROP CONSTRAINT pool_identity_namespace_check;
ALTER TABLE pool_identity ADD CONSTRAINT pool_identity_namespace_check CHECK (namespace IN (
    'rsk_miner_address',
    'namecoin_payout_address',
    'syscoin_payout_address',
    'fractal_reward_address',
    'hathor_reward_address',
    'elastos_reward_address',
    'elastos_minerinfo',
    'qbit_payout_address',
    'core_reward_address',
    'exsat_account'
));
