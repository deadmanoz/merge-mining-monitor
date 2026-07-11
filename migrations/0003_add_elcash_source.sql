-- 0003_add_elcash_source.sql
--
-- Adds Electric Cash (ELCASH) as the 34th `source`: a recovered historical
-- AuxPoW dataset (chain ID 0x2137). This is the first source added after the
-- 0001/0002 baseline squash.
--
-- Two-path reconciliation, the post-squash pattern for adding a source:
--   * Fresh / reset DBs are seeded by the regenerated `0002_seed_sources.sql`,
--     which now appends `auxpow:elcash` as id 34 (existing ids 1..33 unchanged,
--     so this is an append, not a renumbering reorder). For those DBs this
--     migration is a no-op.
--   * Databases that already applied the previous 0002 (and skip it now) get the
--     new row here instead.
-- `source.id` is GENERATED ALWAYS AS IDENTITY and the existing rows hold ids
-- 1..33, so this append takes id 34, matching the registry / 0002 order.
--
-- Idempotent (WHERE NOT EXISTS), so it is safe on both paths and on re-runs.

INSERT INTO source (code, kind, chain, instance, created_at)
SELECT 'auxpow:elcash', 'auxpow', 'elcash', NULL, extract(epoch from now())::bigint
WHERE NOT EXISTS (SELECT 1 FROM source WHERE code = 'auxpow:elcash');
