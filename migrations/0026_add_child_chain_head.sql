-- 0026_add_child_chain_head.sql
--
-- Records the block a child chain last carried at each processed height, so
-- a trailing rescan can decide from one block-hash lookup whether anything at
-- the height changed. Until now a rescanned height was captured again in
-- full (block hash, header and AuxPoW, the full block for the payout
-- identity, then the event upsert) on every tick, because the only durable
-- record of the chain's block at a height was the event row, and a
-- non-merge-mined block leaves no event row at all. From the production host
-- every remote call costs about 340 ms, so a 20-height rescan window turned a
-- 30-second poll tick into minutes.
--
-- One row per (source, height), written by every processed height in the
-- same transaction as its capture. `outcome` says what the producer made of
-- the block: `captured` (an event was written), `recorded` (the block's
-- proof verified and its verdict is settled, but it yields no event, such as
-- a parent that misses Bitcoin's target), `non_auxpow` (the block carries no
-- AuxPoW), `unverified` (its proof did not parse, its verdict still depends
-- on the Bitcoin Core cache and may change on re-observation, or a tolerated
-- Core lookup failure cut its parent's classification short), or
-- `held` (the producer holds the cursor at it). Only the first three are
-- final: a rescan that finds the same hash with a final outcome skips the
-- proof fetch; any other outcome, a different hash, or no row at all runs the
-- full capture again. `btc_parent_header_hash` is the parent the block's
-- proof named, kept so displacement maintenance on a skipped rescan can
-- decide hashless observations the same way the original capture did; it is
-- NULL when the block yielded no verified proof.
--
-- A verdict that came from the Bitcoin Core header cache (Hathor and Elastos
-- take the parent's nBits from it) can change when the cache replaces a
-- shallow boundary or gains a boundary inside existing coverage, and today
-- that is caught by re-observing the height. `core_cache_generation` on the
-- cache state counts those replacements; the head row records the generation
-- it was written under, and a rescan treats a row from an older generation
-- as not final, so the height is captured again and its verdict re-derived.
--
-- `evidence_marker` fingerprints the evidence at the height a producer's
-- decision to record depends on, as the producer defines it (Hathor holds a
-- block's declared work against every captured block's sidecar there, so it
-- records the newest sidecar row at the height; a producer that records on
-- its own proof alone records NULL), so a rescan can tell that an import or
-- capture since has changed that evidence and take the full capture, which
-- re-runs the check.
--
-- No backfill: the first rescan after this migration fills the window, and a
-- missing row is the same as a changed hash. `source_id` follows the source
-- registry's identity, not its retirement, so no ON DELETE clause.

ALTER TABLE bitcoin_core_header_cache_state
    ADD COLUMN core_cache_generation BIGINT NOT NULL DEFAULT 0
        CHECK (core_cache_generation >= 0);

COMMENT ON COLUMN bitcoin_core_header_cache_state.core_cache_generation IS
  'Incremented by every cache replacement that can change a verdict already given (a replaced shallow boundary, or a boundary inside existing coverage); child_chain_head rows from an older generation are not final for a rescan.';

CREATE TABLE child_chain_head (
    source_id BIGINT NOT NULL REFERENCES source(id),
    child_height INTEGER NOT NULL,
    block_hash BYTEA NOT NULL CHECK (octet_length(block_hash) = 32),
    btc_parent_header_hash BYTEA CHECK (octet_length(btc_parent_header_hash) = 32),
    outcome TEXT NOT NULL CHECK (
        outcome IN ('captured', 'recorded', 'non_auxpow', 'unverified', 'held')
    ),
    core_cache_generation BIGINT NOT NULL,
    evidence_marker BIGINT,
    observed_at BIGINT NOT NULL,
    PRIMARY KEY (source_id, child_height)
);

COMMENT ON COLUMN child_chain_head.evidence_marker IS
  'A producer-defined fingerprint of the evidence at this height its recording depended on (Hathor: the newest sidecar row there; NULL when the producer records on its own proof alone); evidence added since makes a rescan take the full capture.';
